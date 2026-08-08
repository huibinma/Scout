use scout_intent_parser::fallback::{
    parse_with_signals, should_invoke_model, FallbackDecision, ModelFallback,
};
use scout_intent_parser::parse;
use scout_search_backend::SearchIntent;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

pub mod enterprise;
pub mod mcp_client;
pub mod recall;
pub mod runner_daemon;
pub mod scaling;
pub mod semantic_quality;

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Case {
    pub id: String,
    pub query: String,
    pub language: String,
    pub variant: String,
    pub expected_intent: Value,
}

#[derive(Debug, Deserialize)]
struct RawCase {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    intent: Option<Value>,
    #[serde(default)]
    expected_intent: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum EvalResult {
    Pass,
    Partial {
        diff: HashMap<String, (Value, Value)>,
    },
    Fail {
        actual_variant: String,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CaseReport {
    pub case: Case,
    pub nl_input: String,
    /// 最终评测结果（fallback 触发时为模型路径结果，否则为 parser 路径）。
    pub result: EvalResult,
    /// 最终 intent JSON。
    pub actual_json: Value,
    /// parser 单独的评测结果。仅 fallback 触发时填充；否则与 `result` 相同时为 `None` 节省体积。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser_result: Option<EvalResult>,
    /// 是否触发了模型 fallback。
    #[serde(default)]
    pub fallback_invoked: bool,
    /// 模型是否产出可反序列化为 `SearchIntent` 的 JSON。仅 `fallback_invoked` 为 true 时有意义。
    #[serde(default)]
    pub fallback_valid_intent: bool,
    /// 单 case 端到端耗时（ms）。
    #[serde(default)]
    pub elapsed_ms: u64,
}

/// 单次 evaluate 的上下文。
///
/// `fallback: None` 时走纯 parser 路径；`Some(&fallback)` 时按 MVP-17 Class 3 触发器
/// 判定是否调模型。
#[derive(Debug)]
pub struct EvalContext<'a> {
    pub fallback: Option<&'a ModelFallback>,
}

impl<'a> EvalContext<'a> {
    #[must_use]
    pub const fn parser_only() -> Self {
        Self { fallback: None }
    }

    #[must_use]
    pub const fn with_fallback(fallback: &'a ModelFallback) -> Self {
        Self {
            fallback: Some(fallback),
        }
    }
}

/// 读取指定版本的评测 fixture。
///
/// `v0.1` 保持读取 PROTO-08 的 50 条 schema fixture；`v0.5` 读取 MVP-25
/// 扩展集。新旧 JSON 字段名会在这里归一化成统一的 [`Case`]。
pub fn load_cases(version: &str) -> anyhow::Result<Vec<Case>> {
    let json_str = match version {
        "v0.1" => include_str!("../../search-backends/common/tests/fixtures/cases.json").to_owned(),
        "v0.5" => {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures")
                .join("v0.5")
                .join("cases.json");
            std::fs::read_to_string(path)?
        }
        // BETA-13：v0.9 = v0.5（500，逐字）+ coverage（500，手标 ground-truth）合并产物。
        "v0.9" => {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("fixtures")
                .join("v0.9")
                .join("cases.json");
            std::fs::read_to_string(path)?
        }
        // BETA-24：以 .json 结尾视为文件路径（held-out / 临时 fixture 评测）
        other
            if Path::new(other)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json")) =>
        {
            std::fs::read_to_string(Path::new(other))
                .map_err(|e| anyhow::anyhow!("读取 fixture 路径 {other} 失败：{e}"))?
        }
        other => anyhow::bail!("未知 fixture 版本：{other}"),
    };
    parse_cases_str(&json_str)
}

#[must_use]
pub fn nl_input(title: &str) -> String {
    let mut s = title.to_owned();
    if let Some(p) = s.find('（') {
        s.truncate(p);
    }
    if let Some(p) = s.find('(') {
        s.truncate(p);
    }
    for prefix in ["refine：", "refine: "] {
        if let Some(stripped) = s.strip_prefix(prefix) {
            s = stripped.to_owned();
        }
    }
    s.trim().to_owned()
}

pub fn parse_cases_str(json_str: &str) -> anyhow::Result<Vec<Case>> {
    let raw_cases: Vec<RawCase> = serde_json::from_str(json_str)?;
    raw_cases
        .into_iter()
        .map(|raw| {
            let query = raw
                .query
                .or(raw.title)
                .ok_or_else(|| anyhow::anyhow!("case {} 缺少 query/title", raw.id))?;
            let expected_intent = raw
                .expected_intent
                .or(raw.intent)
                .ok_or_else(|| anyhow::anyhow!("case {} 缺少 expected_intent/intent", raw.id))?;
            let expected_typed: SearchIntent = serde_json::from_value(expected_intent.clone())
                .map_err(|err| {
                    anyhow::anyhow!("case {} expected_intent 反序列化失败：{err}", raw.id)
                })?;
            let variant = raw
                .variant
                .unwrap_or_else(|| variant_name(&expected_typed).to_owned());
            let language = raw.language.unwrap_or_else(|| {
                expected_intent
                    .get("language")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned()
            });
            Ok(Case {
                id: raw.id,
                query: nl_input(&query),
                language,
                variant,
                expected_intent,
            })
        })
        .collect()
}

#[must_use]
pub fn variant_name(intent: &SearchIntent) -> &'static str {
    match intent {
        SearchIntent::FileSearch(_) => "FileSearch",
        SearchIntent::MediaSearch(_) => "MediaSearch",
        SearchIntent::FileAction(_) => "FileAction",
        SearchIntent::Refine(_) => "Refine",
        SearchIntent::Clarify(_) => "Clarify",
    }
}

#[must_use]
pub fn evaluate_case(case: &Case) -> CaseReport {
    evaluate_case_with_context(case, &EvalContext::parser_only())
}

/// 评测单条 case。`ctx.fallback = None` 走纯 parser；`Some(&fallback)` 时按
/// [`should_invoke_model`] 决策是否调模型。
///
/// 当模型被触发且 `ModelFallback` 调用失败（推理错 / JSON 不合法）时，回落到 parser
/// 路径并把 `fallback_invoked=true / fallback_valid_intent=false` 记录到 `CaseReport`。
#[must_use]
pub fn evaluate_case_with_context(case: &Case, ctx: &EvalContext<'_>) -> CaseReport {
    let start = Instant::now();

    let parser_intent = parse(&case.query);
    let parser_result = evaluate_intent(case, &parser_intent);

    let (final_intent, fallback_invoked, fallback_valid_intent) = match ctx.fallback {
        None => (parser_intent.clone(), false, false),
        Some(fallback) => {
            let parsed = parse_with_signals(&case.query);
            let decision = should_invoke_model(&parsed);
            if matches!(decision, FallbackDecision::UseParser) {
                (parser_intent.clone(), false, false)
            } else {
                match fallback.invoke(&case.query) {
                    Ok(model_intent) => (model_intent, true, true),
                    Err(_) => (parser_intent.clone(), true, false),
                }
            }
        }
    };

    let final_result = if fallback_invoked && fallback_valid_intent {
        evaluate_intent(case, &final_intent)
    } else {
        parser_result.clone()
    };

    let parser_result_field = if fallback_invoked {
        Some(parser_result)
    } else {
        None
    };

    let actual_json = serde_json::to_value(&final_intent).unwrap_or(Value::Null);
    let elapsed_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);

    CaseReport {
        case: case.clone(),
        nl_input: case.query.clone(),
        result: final_result,
        actual_json,
        parser_result: parser_result_field,
        fallback_invoked,
        fallback_valid_intent,
        elapsed_ms,
    }
}

fn evaluate_intent(case: &Case, actual_intent: &SearchIntent) -> EvalResult {
    let actual_variant = variant_name(actual_intent);
    if actual_variant == case.variant {
        let actual_json = serde_json::to_value(actual_intent).unwrap_or(Value::Null);
        let diff = compare_json(&case.expected_intent, &actual_json);
        if diff.is_empty() {
            EvalResult::Pass
        } else {
            EvalResult::Partial { diff }
        }
    } else {
        EvalResult::Fail {
            actual_variant: actual_variant.to_owned(),
        }
    }
}

/// `is_fallback_candidate` 用到的关键词列表（FileSearch 子集触发条件）。
const FALLBACK_KEYWORD_TRIGGERS: &[&str] = &[
    "video",
    "videos",
    "视频",
    "截图",
    "screenshot",
    "screenshots",
    "song",
    "songs",
    "歌",
    "音乐",
    "music",
    "rename",
    "move",
    "copy",
    "delete",
    "recent",
    "最近",
    "最大",
    "最小",
    "biggest",
    "largest",
];

/// 候选过滤：fallback 真正可能补全字段的 case 子集。用于 `--fallback-subset`。
///
/// 当前实现：`MediaSearch` / `Clarify` / `FileAction` 变体；`FileSearch` 中含时间/排序/
/// 媒体类语义词的也算（覆盖 Class 3 结构性遗漏）。
#[must_use]
pub fn is_fallback_candidate(case: &Case) -> bool {
    if matches!(
        case.variant.as_str(),
        "MediaSearch" | "Clarify" | "FileAction"
    ) {
        return true;
    }
    let q = case.query.as_str();
    FALLBACK_KEYWORD_TRIGGERS.iter().any(|t| q.contains(t))
}

/// v0.5：Clarify question 文案完全忽略。`reason` 字段（enum：`AmbiguousTime` / `UnsafeAction`
/// 等）已编码 clarify 的语义；question 文本是本地化呈现，不作 eval 判定标准。
///
/// 历史：v0.4 由 Gemini 落地为 normalize + substring contain（cross-lang 失败）；v0.5 完全
/// 放开是因为 parser-v0.5 出场报告 §7.2 指出 25 个
/// Clarify partial 全部源于跨语言 / 措辞差异，把它们关在 partial 里没有评估价值。
fn is_clarify_question_equal(_e: &str, _a: &str) -> bool {
    true
}

/// v0.5：Clarify options 只校验**结构存在**（都是 Array 或都是 null），不校验长度与内容。
/// 长度差异（如 parser 额外加"取消" / "Cancel" UX 选项）属于本地化呈现，不作 eval 判定。
fn is_clarify_options_equal(e: &Value, a: &Value) -> bool {
    matches!(
        (e, a),
        (Value::Array(_), Value::Array(_)) | (Value::Null, Value::Null)
    )
}

fn compare_json(expected: &Value, actual: &Value) -> HashMap<String, (Value, Value)> {
    let mut diff = HashMap::new();

    if let (Value::Object(e_map), Value::Object(a_map)) = (expected, actual) {
        let is_clarify = e_map.get("intent").and_then(Value::as_str) == Some("clarify");

        let mut keys: Vec<_> = e_map.keys().collect();
        for k in a_map.keys() {
            if !e_map.contains_key(k) {
                keys.push(k);
            }
        }
        keys.sort();
        keys.dedup();

        for key in keys {
            // 2026-07-04 拍板：`language` 降出严格匹配。v0.5 标注自身口径矛盾（「会议 Excel」
            // 期望 mixed、「budget pdf」期望 zh），产品面该字段仅影响 location hint 语种选择，
            // 判定价值低于标注噪声。分语言统计（language_stats）按标注分桶、不受影响。
            if key == "language" {
                continue;
            }
            let e_val = e_map.get(key).unwrap_or(&Value::Null);
            let a_val = a_map.get(key).unwrap_or(&Value::Null);

            let matched = if is_clarify {
                match key.as_str() {
                    "question" => {
                        if let (Some(e_str), Some(a_str)) = (e_val.as_str(), a_val.as_str()) {
                            is_clarify_question_equal(e_str, a_str)
                        } else {
                            is_equal(e_val, a_val)
                        }
                    }
                    "options" => is_clarify_options_equal(e_val, a_val),
                    _ => is_equal(e_val, a_val),
                }
            } else {
                is_equal(e_val, a_val)
            };

            if !matched {
                diff.insert(key.clone(), (e_val.clone(), a_val.clone()));
            }
        }
    } else if !is_equal(expected, actual) {
        diff.insert("root".to_owned(), (expected.clone(), actual.clone()));
    }

    diff
}

fn is_equal(v1: &Value, v2: &Value) -> bool {
    match (v1, v2) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(b1), Value::Bool(b2)) => b1 == b2,
        (Value::Number(n1), Value::Number(n2)) => {
            // Compare as f64 to avoid float vs int mismatch
            if let (Some(f1), Some(f2)) = (n1.as_f64(), n2.as_f64()) {
                (f1 - f2).abs() < f64::EPSILON
            } else {
                n1 == n2
            }
        }
        (Value::String(s1), Value::String(s2)) => s1 == s2,
        (Value::Array(a1), Value::Array(a2)) => {
            if a1.len() != a2.len() {
                return false;
            }
            a1.iter().zip(a2.iter()).all(|(i1, i2)| is_equal(i1, i2))
        }
        (Value::Object(o1), Value::Object(o2)) => {
            if o1.len() == o2.len() {
                o1.iter()
                    .all(|(k, v)| o2.get(k).map_or(v.is_null(), |v2| is_equal(v, v2)))
            } else {
                // Check if the extra keys are all Null
                let mut all_keys: Vec<_> = o1.keys().collect();
                for k in o2.keys() {
                    if !o1.contains_key(k) {
                        all_keys.push(k);
                    }
                }
                all_keys.sort();
                all_keys.dedup();

                for k in all_keys {
                    let v1 = o1.get(k).unwrap_or(&Value::Null);
                    let v2 = o2.get(k).unwrap_or(&Value::Null);
                    if !is_equal(v1, v2) {
                        return false;
                    }
                }
                true
            }
        }
        _ => false,
    }
}

#[derive(Debug)]
pub struct Summary {
    pub total: usize,
    pub pass: usize,
    pub partial: usize,
    pub fail: usize,
    pub variant_stats: HashMap<String, (usize, usize, usize)>, // variant -> (pass, partial, fail)
    pub language_stats: HashMap<String, (usize, usize, usize)>, // lang -> (pass, partial, fail)

    /// 模型 fallback 实际被调用的 case 数。
    pub fallback_invoked: usize,
    /// 模型产出可反序列化为 `SearchIntent` 的 JSON 的 case 数（`fallback_invoked` 子集）。
    pub fallback_valid_intent: usize,
    /// parser 路径 fail → fallback 路径 pass 的 case 数。
    pub rescued_to_pass: usize,
    /// parser 路径 fail/partial → fallback 路径 partial（且更好）的 case 数。
    pub rescued_to_partial: usize,
    /// parser 路径 pass/partial → fallback 路径变差（fail / partial 更差）的 case 数。
    pub regressed: usize,
    /// 全量 case 端到端耗时（ms），用于 p50/p95 计算。
    pub latencies_all_ms: Vec<u64>,
    /// 仅 fallback 触发的 case 端到端耗时（ms）。
    pub latencies_fallback_ms: Vec<u64>,
}

impl Summary {
    /// variant 命中数：variant 匹配即算命中，包含字段有差异的 partial。
    #[must_use]
    pub const fn variant_hits(&self) -> usize {
        self.pass + self.partial
    }

    /// 字段级精确匹配数：只有完全 pass 才算精确匹配。
    #[must_use]
    pub const fn field_exact_matches(&self) -> usize {
        self.pass
    }
}

#[must_use]
pub fn generate_summary(reports: &[CaseReport]) -> Summary {
    let mut summary = Summary {
        total: reports.len(),
        pass: 0,
        partial: 0,
        fail: 0,
        variant_stats: HashMap::new(),
        language_stats: HashMap::new(),
        fallback_invoked: 0,
        fallback_valid_intent: 0,
        rescued_to_pass: 0,
        rescued_to_partial: 0,
        regressed: 0,
        latencies_all_ms: Vec::new(),
        latencies_fallback_ms: Vec::new(),
    };

    for report in reports {
        let (p, pt, f) = match report.result {
            EvalResult::Pass => {
                summary.pass += 1;
                (1, 0, 0)
            }
            EvalResult::Partial { .. } => {
                summary.partial += 1;
                (0, 1, 0)
            }
            EvalResult::Fail { .. } => {
                summary.fail += 1;
                (0, 0, 1)
            }
        };

        let v_entry = summary
            .variant_stats
            .entry(report.case.variant.clone())
            .or_insert((0, 0, 0));
        v_entry.0 += p;
        v_entry.1 += pt;
        v_entry.2 += f;

        let lang = report.case.language.clone();
        let l_entry = summary.language_stats.entry(lang).or_insert((0, 0, 0));
        l_entry.0 += p;
        l_entry.1 += pt;
        l_entry.2 += f;

        if report.fallback_invoked {
            summary.fallback_invoked += 1;
            if report.fallback_valid_intent {
                summary.fallback_valid_intent += 1;
            }
            summary.latencies_fallback_ms.push(report.elapsed_ms);
        }

        if let Some(parser_result) = &report.parser_result {
            let parser_rank = result_rank(parser_result);
            let final_rank = result_rank(&report.result);
            if final_rank > parser_rank {
                if matches!(report.result, EvalResult::Pass) {
                    summary.rescued_to_pass += 1;
                } else if matches!(report.result, EvalResult::Partial { .. }) {
                    summary.rescued_to_partial += 1;
                }
            } else if final_rank < parser_rank {
                summary.regressed += 1;
            }
        }

        summary.latencies_all_ms.push(report.elapsed_ms);
    }

    summary
}

/// 用于 rescued/regressed 计算：Pass=2 > Partial=1 > Fail=0。
#[must_use]
pub const fn result_rank(r: &EvalResult) -> u8 {
    match r {
        EvalResult::Pass => 2,
        EvalResult::Partial { .. } => 1,
        EvalResult::Fail { .. } => 0,
    }
}

/// 统计 fail case 中 expected variant → actual variant 的转移情况。
#[must_use]
pub fn variant_confusion_matrix(reports: &[CaseReport]) -> Vec<((String, String), usize)> {
    let mut matrix = HashMap::new();
    for r in reports {
        if let EvalResult::Fail { actual_variant } = &r.result {
            let entry = matrix
                .entry((r.case.variant.clone(), actual_variant.clone()))
                .or_insert(0);
            *entry += 1;
        }
    }
    let mut sorted: Vec<_> = matrix.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sorted
}

/// 计算延迟 p50 / p95（ms）。空列表返回 (0, 0)。
#[must_use]
pub fn latency_percentiles(latencies_ms: &[u64]) -> (u64, u64) {
    if latencies_ms.is_empty() {
        return (0, 0);
    }
    let mut sorted: Vec<u64> = latencies_ms.to_vec();
    sorted.sort_unstable();
    let p50_idx = sorted.len() / 2;
    let p95_idx = (sorted.len() * 95 / 100).min(sorted.len() - 1);
    (sorted[p50_idx], sorted[p95_idx])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn load_cases_accepts_json_path() -> anyhow::Result<()> {
        // 路径形态（以 .json 结尾）应按文件路径读取——held-out 评测依赖
        let path = format!("{}/fixtures/v0.5/cases.json", env!("CARGO_MANIFEST_DIR"));
        let by_path = load_cases(&path)?;
        let by_name = load_cases("v0.5")?;
        assert_eq!(by_path.len(), by_name.len());
        assert_eq!(by_path[0].id, by_name[0].id);
        Ok(())
    }

    #[test]
    fn language_excluded_from_strict_match() {
        // 2026-07-04 拍板：language 降出严格匹配（v0.5 标注自身口径矛盾、产品面仅影响
        // hint 语种）；其余字段差异仍照常判 partial。
        let expected = json!({
            "intent": "file_search",
            "schema_version": "1.0",
            "language": "mixed",
            "keywords": ["budget"]
        });
        let actual_lang_only = json!({
            "intent": "file_search",
            "schema_version": "1.0",
            "language": "zh",
            "keywords": ["budget"]
        });
        assert!(
            compare_json(&expected, &actual_lang_only).is_empty(),
            "仅 language 差异不应计入 diff"
        );
        let actual_kw_diff = json!({
            "intent": "file_search",
            "schema_version": "1.0",
            "language": "mixed",
            "keywords": ["预算"]
        });
        let diff = compare_json(&expected, &actual_kw_diff);
        assert!(diff.contains_key("keywords"), "其余字段仍严格匹配");
    }

    #[test]
    fn test_is_clarify_question_equal_v05() {
        // v0.5 起 Clarify question 文案完全忽略 — reason 字段已编码语义，question 是本地化呈现。
        // 跨语言（en expected vs zh actual）必须 match。
        assert!(is_clarify_question_equal(
            "Which recent time range should I use?",
            "你说的「最近」是指最近几天？"
        ));
        // 同语言不同措辞也 match
        assert!(is_clarify_question_equal(
            "Which location?",
            "Which folder should I look in?"
        ));
        // 极端：完全无关也 match（reason 字段才是判定标准）
        assert!(is_clarify_question_equal(
            "Which location",
            "Different question"
        ));
    }

    #[test]
    fn test_is_clarify_options_equal_v05() {
        // v0.5：只校验结构（Array vs Array / null vs null），长度也不管。
        // 同语言
        let e = json!(["a", "b"]);
        let a = json!(["B", "a!"]);
        assert!(is_clarify_options_equal(&e, &a));

        // 跨语言
        let e = json!(["today", "past 3 days", "past week", "past month"]);
        let a = json!(["今天", "过去 3 天", "过去一周", "过去一个月"]);
        assert!(is_clarify_options_equal(&e, &a));

        // 长度不等也 match — parser 加"取消"等 UX 选项不应作 eval 惩罚
        let e = json!(["全盘搜索", "下载", "文稿", "桌面"]);
        let a = json!(["全盘搜索", "下载", "文稿", "桌面", "取消"]);
        assert!(is_clarify_options_equal(&e, &a));

        // null vs null
        let e = json!(null);
        let a = json!(null);
        assert!(is_clarify_options_equal(&e, &a));

        // Array vs null fail（结构性差异）
        let e = json!(["a"]);
        let a = json!(null);
        assert!(!is_clarify_options_equal(&e, &a));
    }

    #[test]
    fn test_compare_json_clarify() {
        let e = json!({
            "intent": "clarify",
            "reason": "ambiguous_time",
            "question": "Which time?",
            "options": ["Today", "Yesterday"],
            "language": "en",
            "schema_version": "1.0"
        });

        // Strict match
        let a = e.clone();
        assert!(compare_json(&e, &a).is_empty());

        // Weak match (punctuation, case, order)
        let a = json!({
            "intent": "clarify",
            "reason": "ambiguous_time",
            "question": "which time",
            "options": ["yesterday!", "TODAY"],
            "language": "en",
            "schema_version": "1.0"
        });
        assert!(compare_json(&e, &a).is_empty());

        // Reason different -> Fail (not matching)
        let a = json!({
            "intent": "clarify",
            "reason": "ambiguous_location",
            "question": "Which time?",
            "options": ["Today", "Yesterday"],
            "language": "en",
            "schema_version": "1.0"
        });
        let diff = compare_json(&e, &a);
        assert!(diff.contains_key("reason"));

        // v0.5：options 长度差异不再判 partial（"取消" UX 选项不应惩罚）
        let a = json!({
            "intent": "clarify",
            "reason": "ambiguous_time",
            "question": "Which time?",
            "options": ["Today", "Yesterday", "Last Week"],
            "language": "en",
            "schema_version": "1.0"
        });
        let diff = compare_json(&e, &a);
        assert!(!diff.contains_key("options"));

        // 结构性差异（Array vs null）仍判 partial
        let a = json!({
            "intent": "clarify",
            "reason": "ambiguous_time",
            "question": "Which time?",
            "options": null,
            "language": "en",
            "schema_version": "1.0"
        });
        let diff = compare_json(&e, &a);
        assert!(diff.contains_key("options"));
    }
}
