//! BETA-40 收尾：企业三场景（律所 / 审计 / 离职归档）daemon 端到端评测。
//!
//! 数据源是 [`test-materials/enterprise-scenarios-raw/expected/queries.tsv`]
//! （BETA-41 扩展材料附带的 query 期望集）：每行一个 case，`subject` 决定用哪枚
//! bearer token 查询（信息墙语义），`expected_paths` 为分号分隔的相对路径
//! （相对 materials root）；越权负样本写 `ACCESS_DENIED`（裸标记）或
//! `ACCESS_DENIED:<墙目标相对路径>`——后者声明「这道墙背后确有内容、且该 subject
//! 无权触达」，供离线闸门校验负样本非空洞（详 `tests/enterprise_scenarios_gate.rs`）。
//!
//! 本模块只放**纯逻辑**（TSV 解析 / 命中评分 / daemon TOML config 生成 / 报告
//! 渲染），好让 CI 不依赖真模型也能单测；async 编排（spawn daemon + MCP 调用）
//! 在 `bin/enterprise_scenarios.rs`。
//!
//! [`test-materials/enterprise-scenarios-raw/expected/queries.tsv`]:
//!     ../../../test-materials/enterprise-scenarios-raw/expected/queries.tsv

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{anyhow, bail, Result};
use serde::Serialize;

/// `expected_paths` 列的负样本标记：该 subject 无权检索到目标内容。
pub const ACCESS_DENIED_MARKER: &str = "ACCESS_DENIED";

/// 合法 scenario 值（与 BETA-41 材料目录一级结构对齐）。
pub const SCENARIOS: [&str; 3] = ["lawfirm", "audit", "offboarding"];

/// 评测用 collection 声明：镜像
/// `test-materials/enterprise-scenarios-raw/configs/scoutd-enterprise-test.toml`
/// 的 7 集合布局（该示例文件 token 不足 32 字符、不能直接喂 daemon——本模块
/// 生成合规 config，示例文件仅作人读参考）。
pub const COLLECTIONS: [(&str, &str, &str, &str); 7] = [
    // (id, subject_kind, 相对 materials root 的 root, display_name)
    (
        "case-2026-blueharbor",
        "case",
        "lawfirm/case-2026-blueharbor",
        "蓝湾贸易合同纠纷案（一审）",
    ),
    (
        "case-2026-northfield",
        "case",
        "lawfirm/case-2026-northfield",
        "北原并购尽调项目",
    ),
    (
        "audit-2026-procurement",
        "audit_project",
        "audit/audit-2026-procurement",
        "2026 行政采购专项审计",
    ),
    (
        "audit-2025-facilities",
        "audit_project",
        "audit/audit-2025-facilities",
        "2025 设施维护专项审计",
    ),
    (
        "offboarding-lishili-tech",
        "employee",
        "offboarding/lishili-tech",
        "李示例 · 技术交接归档",
    ),
    (
        "offboarding-lishili-hr",
        "employee",
        "offboarding/lishili-hr",
        "李示例 · HR 手续归档",
    ),
    (
        "offboarding-other-tech",
        "employee",
        "offboarding/other-employee-tech",
        "王样本 · 技术交接归档",
    ),
];

/// subject → 授权 collection 列表（信息墙）。与示例 TOML 的 `[[tokens]]` 对齐。
pub const SUBJECT_GRANTS: [(&str, &[&str]); 5] = [
    ("zhang.san", &["case-2026-blueharbor"]),
    ("li.si", &["case-2026-northfield"]),
    ("auditor.wang", &["audit-2026-procurement"]),
    ("wang.yangben", &["offboarding-lishili-tech"]),
    ("hr.zhao", &["offboarding-lishili-hr"]),
];

/// 一条评测 case（queries.tsv 的一行）。
#[derive(Debug, Clone, Serialize)]
pub struct EnterpriseCase {
    pub id: String,
    /// lawfirm / audit / offboarding。
    pub scenario: String,
    /// 查询者身份；决定用哪枚 token（信息墙语义）。
    pub subject: String,
    pub query: String,
    pub expectation: Expectation,
    /// 该 case 考察什么（报告用，人读）。
    pub purpose: String,
}

/// case 的期望结果。
#[derive(Debug, Clone, Serialize)]
pub enum Expectation {
    /// 全部相对路径（`/` 分隔、相对 materials root）需出现在 top-K。
    Hits(Vec<String>),
    /// 越权负样本：缺省检索不得泄漏未授权集合内容；显式指名未授权集合须报错。
    ///
    /// `target` 为可选的「墙目标」相对路径——该 subject 试图（且必须失败）触达的
    /// 真实内容。声明它可让离线闸门校验负样本非空洞（目标真实存在 + 落在某未授权
    /// collection 内），运行期不消费该字段（越权探针恒扫全部未授权 collection）。
    AccessDenied { target: Option<String> },
}

/// 解析 queries.tsv 全文。首行是表头（`id\tscenario\t...`），其余每行 6 列。
///
/// # Errors
///
/// 表头缺失、列数不对、scenario 非法、subject 无 token 映射、id 重复时报错——
/// 让 fixture 变更第一时间在 CI 炸出来，而不是评测时静默跑偏。
pub fn parse_queries_tsv(text: &str) -> Result<Vec<EnterpriseCase>> {
    let mut lines = text.lines();
    let header = lines.next().ok_or_else(|| anyhow!("queries.tsv 为空"))?;
    let expected_header = [
        "id",
        "scenario",
        "subject",
        "query",
        "expected_paths",
        "purpose",
    ];
    let header_cols: Vec<&str> = header.split('\t').map(str::trim).collect();
    if header_cols != expected_header {
        bail!("queries.tsv 表头不符：期望 {expected_header:?}，实得 {header_cols:?}");
    }

    let known_subjects: Vec<&str> = SUBJECT_GRANTS.iter().map(|(s, _)| *s).collect();
    let mut cases = Vec::new();
    let mut seen_ids = std::collections::BTreeSet::new();
    for (lineno, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
        if cols.len() != 6 {
            bail!(
                "queries.tsv 第 {} 行应有 6 列，实得 {} 列：{line:?}",
                lineno + 2,
                cols.len()
            );
        }
        let (id, scenario, subject, query, expected_raw, purpose) =
            (cols[0], cols[1], cols[2], cols[3], cols[4], cols[5]);
        if !SCENARIOS.contains(&scenario) {
            bail!("case {id}: scenario 非法：{scenario}（合法值 {SCENARIOS:?}）");
        }
        if !known_subjects.contains(&subject) {
            bail!("case {id}: subject {subject} 没有 token 映射（SUBJECT_GRANTS）");
        }
        if query.is_empty() {
            bail!("case {id}: query 为空");
        }
        if !seen_ids.insert(id.to_owned()) {
            bail!("case id 重复：{id}");
        }
        let expectation = if let Some(rest) = expected_raw.strip_prefix(ACCESS_DENIED_MARKER) {
            if rest.is_empty() {
                Expectation::AccessDenied { target: None }
            } else if let Some(target) = rest.strip_prefix(':') {
                let target = target.trim();
                if target.is_empty() {
                    bail!("case {id}: {ACCESS_DENIED_MARKER}: 后缺墙目标路径");
                }
                Expectation::AccessDenied {
                    target: Some(target.to_owned()),
                }
            } else {
                bail!(
                    "case {id}: expected_paths 以 {ACCESS_DENIED_MARKER} 开头但格式非法：{expected_raw:?}"
                );
            }
        } else {
            let paths: Vec<String> = expected_raw
                .split(';')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_owned)
                .collect();
            if paths.is_empty() {
                bail!("case {id}: expected_paths 为空且不是 {ACCESS_DENIED_MARKER}");
            }
            Expectation::Hits(paths)
        };
        cases.push(EnterpriseCase {
            id: id.to_owned(),
            scenario: scenario.to_owned(),
            subject: subject.to_owned(),
            query: query.to_owned(),
            expectation,
            purpose: purpose.to_owned(),
        });
    }
    if cases.is_empty() {
        bail!("queries.tsv 没有任何 case 行");
    }
    Ok(cases)
}

/// 给 subject 生成确定性 bearer token（≥ 32 字符，满足 daemon
/// `MIN_TOKEN_LEN` 校验）。本地评测进程内使用、无泄漏面。
#[must_use]
pub fn token_for_subject(subject: &str) -> String {
    let mut t = format!("evals-enterprise-{subject}-");
    while t.len() < 40 {
        t.push('x');
    }
    t
}

/// subject → 授权 collection 集。未知 subject 返回空表。
#[must_use]
pub fn grants_for_subject(subject: &str) -> &'static [&'static str] {
    SUBJECT_GRANTS
        .iter()
        .find(|(s, _)| *s == subject)
        .map_or(&[], |(_, g)| *g)
}

/// 生成 daemon collection 模式 TOML config（绝对路径 roots + 合规长度 token）。
///
/// 路径写成 TOML 字面量字符串（单引号）避免 Windows 反斜杠转义问题。
///
/// # Errors
///
/// materials root 下缺任一 collection 目录时报错（防跑在错误目录上出全零结果）。
pub fn render_config_toml(materials_root: &Path) -> Result<String> {
    let mut out = String::from(
        "# 自动生成：enterprise_scenarios 评测用 daemon config（勿手编）\n\n[audit]\nlog_query = true\n",
    );
    for (id, kind, rel_root, display) in COLLECTIONS {
        let root = materials_root.join(rel_root.replace('/', std::path::MAIN_SEPARATOR_STR));
        if !root.is_dir() {
            bail!("collection {id} 的 root 不存在：{}", root.display());
        }
        let root_str = root
            .to_str()
            .ok_or_else(|| anyhow!("collection {id} root 路径非 UTF-8"))?;
        if root_str.contains('\'') {
            bail!("collection {id} root 路径含单引号，无法写 TOML 字面量：{root_str}");
        }
        let _ = write!(
            out,
            "\n[[collections]]\nid = \"{id}\"\ndisplay_name = \"{display}\"\nsubject_kind = \"{kind}\"\nroots = ['{root_str}']\nread_only = true\n"
        );
    }
    for (subject, grants) in SUBJECT_GRANTS {
        let token = token_for_subject(subject);
        let cols = grants
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = write!(
            out,
            "\n[[tokens]]\ntoken = \"{token}\"\nsubject = \"{subject}\"\ncollections = [{cols}]\n"
        );
    }
    Ok(out)
}

/// 路径归一：反斜杠 → 正斜杠 + 全小写（Windows 大小写不敏感；fixture 文件名
/// 全小写 ASCII，Unix 上无误报风险）。
#[must_use]
pub fn normalize_path(p: &str) -> String {
    p.replace('\\', "/").to_lowercase()
}

/// 在结果 path 列表里找期望相对路径的排名（1-based）。按「`/` 边界后缀匹配」
/// 判命中，容忍结果侧是绝对路径 / 混合分隔符。
#[must_use]
pub fn find_rank(expected_rel: &str, result_paths: &[String]) -> Option<usize> {
    let needle = format!("/{}", normalize_path(expected_rel));
    result_paths
        .iter()
        .position(|p| normalize_path(p).ends_with(&needle))
        .map(|i| i + 1)
}

/// 单 case 评测结果。
#[derive(Debug, Clone, Serialize)]
pub struct CaseOutcome {
    pub id: String,
    pub scenario: String,
    pub subject: String,
    pub query: String,
    pub pass: bool,
    /// 人读细节（命中排名 / 泄漏与越权详情）。
    pub detail: String,
    /// 正样本：与 expected 对齐的排名（None = 不在 top-K）。负样本恒空。
    pub ranks: Vec<Option<usize>>,
    /// daemon 返回的结果条数（正负样本都记）。
    pub result_count: usize,
    /// daemon 返回的 degraded 标志。
    pub degraded: bool,
}

/// 正样本评分：全部期望路径进 top-K 才 pass；detail 记逐路径排名。
#[must_use]
pub fn score_hits(case: &EnterpriseCase, result_paths: &[String], degraded: bool) -> CaseOutcome {
    let Expectation::Hits(expected) = &case.expectation else {
        unreachable!("score_hits 只接正样本 case");
    };
    let ranks: Vec<Option<usize>> = expected
        .iter()
        .map(|e| find_rank(e, result_paths))
        .collect();
    let pass = ranks.iter().all(Option::is_some);
    let detail = expected
        .iter()
        .zip(&ranks)
        .map(|(e, r)| {
            let name = e.rsplit('/').next().unwrap_or(e);
            match r {
                Some(rank) => format!("{name}@{rank}"),
                None => format!("{name}@miss"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    CaseOutcome {
        id: case.id.clone(),
        scenario: case.scenario.clone(),
        subject: case.subject.clone(),
        query: case.query.clone(),
        pass,
        detail,
        ranks,
        result_count: result_paths.len(),
        degraded,
    }
}

/// 负样本评分：
///
/// - `result_collections`：缺省检索命中的 collection id 列表——任何一个不在
///   授权集里即判「泄漏」（物理信息墙被击穿，严重失败）；
/// - `denied_probe_failures`：显式指名各未授权 collection 后**没有**报错的
///   collection id 列表——应恒空。
#[must_use]
pub fn score_denied(
    case: &EnterpriseCase,
    result_collections: &[String],
    result_count: usize,
    denied_probe_failures: &[String],
    degraded: bool,
) -> CaseOutcome {
    let granted = grants_for_subject(&case.subject);
    let leaked: Vec<&String> = result_collections
        .iter()
        .filter(|c| !granted.contains(&c.as_str()))
        .collect();
    let pass = leaked.is_empty() && denied_probe_failures.is_empty();
    let mut parts = Vec::new();
    if leaked.is_empty() {
        parts.push("缺省检索无跨集合泄漏".to_owned());
    } else {
        parts.push(format!("泄漏未授权集合：{leaked:?}"));
    }
    if denied_probe_failures.is_empty() {
        parts.push("显式越权全部被拒".to_owned());
    } else {
        parts.push(format!("越权未被拒：{denied_probe_failures:?}"));
    }
    CaseOutcome {
        id: case.id.clone(),
        scenario: case.scenario.clone(),
        subject: case.subject.clone(),
        query: case.query.clone(),
        pass,
        detail: parts.join("；"),
        ranks: Vec::new(),
        result_count,
        degraded,
    }
}

/// 场景级汇总。
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioAgg {
    pub scenario: String,
    pub total: usize,
    pub passed: usize,
}

/// 按 scenario 聚合 + 末尾追加 OVERALL 行。
#[must_use]
pub fn aggregate(outcomes: &[CaseOutcome]) -> Vec<ScenarioAgg> {
    let mut by: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for o in outcomes {
        let e = by.entry(o.scenario.as_str()).or_insert((0, 0));
        e.0 += 1;
        if o.pass {
            e.1 += 1;
        }
    }
    let mut aggs: Vec<ScenarioAgg> = by
        .into_iter()
        .map(|(scenario, (total, passed))| ScenarioAgg {
            scenario: scenario.to_owned(),
            total,
            passed,
        })
        .collect();
    aggs.push(ScenarioAgg {
        scenario: "OVERALL".to_owned(),
        total: outcomes.len(),
        passed: outcomes.iter().filter(|o| o.pass).count(),
    });
    aggs
}

/// 完整评测报告（JSON 输出用顶层结构）。
#[derive(Debug, Serialize)]
pub struct EnterpriseReport {
    /// 运行参数快照（复现实验用）。
    pub topk: usize,
    pub semantic_weight: Option<f64>,
    pub model_file: String,
    pub outcomes: Vec<CaseOutcome>,
    pub aggregates: Vec<ScenarioAgg>,
}

/// 渲染人读 Markdown 报告。
#[must_use]
pub fn render_report_markdown(report: &EnterpriseReport) -> String {
    let mut out = String::new();
    out.push_str("# 企业三场景 daemon 评测报告\n\n");
    let _ = writeln!(
        out,
        "- topk={} semantic_weight={} model={}\n",
        report.topk,
        report
            .semantic_weight
            .map_or_else(|| "default".to_owned(), |w| format!("{w}")),
        report.model_file
    );
    out.push_str("## 场景汇总\n\n| scenario | passed | total |\n|---|---|---|\n");
    for a in &report.aggregates {
        let _ = writeln!(out, "| {} | {} | {} |", a.scenario, a.passed, a.total);
    }
    out.push_str("\n## 逐 case\n\n| id | subject | 结果 | degraded | 详情 | query |\n|---|---|---|---|---|---|\n");
    for o in &report.outcomes {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} |",
            o.id,
            o.subject,
            if o.pass { "✅" } else { "❌" },
            if o.degraded { "⚠️" } else { "" },
            o.detail,
            o.query
        );
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const TSV_OK: &str = "id\tscenario\tsubject\tquery\texpected_paths\tpurpose\n\
        L-01\tlawfirm\tzhang.san\t判决\tlawfirm/case-2026-blueharbor/a.txt\t测试\n\
        L-07\tlawfirm\tli.si\t判决书\tACCESS_DENIED\t越权\n\
        O-08\toffboarding\twang.yangben\t清单\ta/x.md;a/y.md\t近重复\n";

    #[test]
    fn parse_tsv_happy_path() {
        let cases = parse_queries_tsv(TSV_OK).unwrap();
        assert_eq!(cases.len(), 3);
        assert!(matches!(&cases[0].expectation, Expectation::Hits(p) if p.len() == 1));
        assert!(matches!(
            &cases[1].expectation,
            Expectation::AccessDenied { target: None }
        ));
        assert!(matches!(&cases[2].expectation, Expectation::Hits(p) if p.len() == 2));
    }

    #[test]
    fn parse_tsv_denied_target_variants() {
        // 带墙目标。
        let with_target = "id\tscenario\tsubject\tquery\texpected_paths\tpurpose\n\
            L-07\tlawfirm\tli.si\t判决书\tACCESS_DENIED:lawfirm/case-2026-blueharbor/x.txt\t越权\n";
        let cases = parse_queries_tsv(with_target).unwrap();
        assert!(matches!(
            &cases[0].expectation,
            Expectation::AccessDenied { target: Some(t) } if t == "lawfirm/case-2026-blueharbor/x.txt"
        ));
        // 裸标记仍受支持（target=None）。
        let bare = "id\tscenario\tsubject\tquery\texpected_paths\tpurpose\n\
            L-07\tlawfirm\tli.si\t判决书\tACCESS_DENIED\t越权\n";
        assert!(matches!(
            &parse_queries_tsv(bare).unwrap()[0].expectation,
            Expectation::AccessDenied { target: None }
        ));
        // `ACCESS_DENIED:` 后为空 → 报错。
        let empty = "id\tscenario\tsubject\tquery\texpected_paths\tpurpose\n\
            L-07\tlawfirm\tli.si\t判决书\tACCESS_DENIED:\t越权\n";
        assert!(parse_queries_tsv(empty).is_err());
        // 以 ACCESS_DENIED 开头但非 `:` 分隔 → 报错（防路径拼写事故被当负样本吞掉）。
        let malformed = "id\tscenario\tsubject\tquery\texpected_paths\tpurpose\n\
            L-07\tlawfirm\tli.si\t判决书\tACCESS_DENIED_typo\t越权\n";
        assert!(parse_queries_tsv(malformed).is_err());
    }

    #[test]
    fn parse_tsv_rejects_bad_header() {
        let text = "id\tscenario\tsubject\tquery\tWRONG\tpurpose\nx\tlawfirm\tzhang.san\tq\ta\tb\n";
        assert!(parse_queries_tsv(text).is_err());
    }

    #[test]
    fn parse_tsv_rejects_unknown_scenario_and_subject_and_dup_id() {
        let bad_scenario =
            "id\tscenario\tsubject\tquery\texpected_paths\tpurpose\nx\thr\tzhang.san\tq\ta\tb\n";
        assert!(parse_queries_tsv(bad_scenario).is_err());
        let bad_subject =
            "id\tscenario\tsubject\tquery\texpected_paths\tpurpose\nx\tlawfirm\tnobody\tq\ta\tb\n";
        assert!(parse_queries_tsv(bad_subject).is_err());
        let dup = "id\tscenario\tsubject\tquery\texpected_paths\tpurpose\n\
            x\tlawfirm\tzhang.san\tq\ta\tb\nx\tlawfirm\tzhang.san\tq2\ta\tb\n";
        assert!(parse_queries_tsv(dup).is_err());
    }

    #[test]
    fn token_meets_min_len_and_deterministic() {
        for (subject, _) in SUBJECT_GRANTS {
            let t = token_for_subject(subject);
            assert!(t.len() >= 32, "token 应 ≥32 字符：{t}");
            assert_eq!(t, token_for_subject(subject));
        }
    }

    #[test]
    fn find_rank_matches_windows_and_unix_separators() {
        let results = vec![
            r"D:\corpus\lawfirm\case-2026-blueharbor\scan-source\judgment.txt".to_owned(),
            "/corpus/audit/audit-2026-procurement/mailbox/quote.eml".to_owned(),
        ];
        assert_eq!(
            find_rank(
                "lawfirm/case-2026-blueharbor/scan-source/judgment.txt",
                &results
            ),
            Some(1)
        );
        assert_eq!(
            find_rank("audit/audit-2026-procurement/mailbox/quote.eml", &results),
            Some(2)
        );
        assert_eq!(find_rank("lawfirm/other.txt", &results), None);
        // `/` 边界防误配：`judgment.txt` 不应匹配 `xjudgment.txt`。
        let tricky = vec!["/c/xjudgment.txt".to_owned()];
        assert_eq!(find_rank("judgment.txt", &tricky), None);
    }

    fn hit_case(expected: &[&str]) -> EnterpriseCase {
        EnterpriseCase {
            id: "T-1".to_owned(),
            scenario: "lawfirm".to_owned(),
            subject: "zhang.san".to_owned(),
            query: "q".to_owned(),
            expectation: Expectation::Hits(expected.iter().map(|s| (*s).to_owned()).collect()),
            purpose: String::new(),
        }
    }

    #[test]
    fn score_hits_requires_all_expected_in_topk() {
        let case = hit_case(&["a/x.txt", "a/y.txt"]);
        let both = vec!["/r/a/x.txt".to_owned(), "/r/a/y.txt".to_owned()];
        assert!(score_hits(&case, &both, false).pass);
        let one = vec!["/r/a/x.txt".to_owned()];
        let outcome = score_hits(&case, &one, false);
        assert!(!outcome.pass);
        assert!(outcome.detail.contains("y.txt@miss"), "{}", outcome.detail);
    }

    #[test]
    fn score_denied_fails_on_leak_or_unrejected_probe() {
        let case = EnterpriseCase {
            id: "L-07".to_owned(),
            scenario: "lawfirm".to_owned(),
            subject: "li.si".to_owned(),
            query: "q".to_owned(),
            expectation: Expectation::AccessDenied { target: None },
            purpose: String::new(),
        };
        // li.si 只授权 case-2026-northfield。
        let ok = score_denied(&case, &["case-2026-northfield".to_owned()], 1, &[], false);
        assert!(ok.pass);
        let leak = score_denied(&case, &["case-2026-blueharbor".to_owned()], 1, &[], false);
        assert!(!leak.pass);
        let probe_fail = score_denied(&case, &[], 0, &["case-2026-blueharbor".to_owned()], false);
        assert!(!probe_fail.pass);
    }

    #[test]
    fn aggregate_appends_overall() {
        let case = hit_case(&["a/x.txt"]);
        let o1 = score_hits(&case, &["/r/a/x.txt".to_owned()], false);
        let o2 = score_hits(&case, &[], false);
        let aggs = aggregate(&[o1, o2]);
        let overall = aggs.last().unwrap();
        assert_eq!(overall.scenario, "OVERALL");
        assert_eq!(overall.total, 2);
        assert_eq!(overall.passed, 1);
    }

    #[test]
    fn render_config_toml_contains_collections_and_long_tokens() {
        // 用仓库真实 materials root（相对 CARGO_MANIFEST_DIR）。
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test-materials/enterprise-scenarios-raw");
        let toml = render_config_toml(&root).expect("真实 materials root 应能生成 config");
        for (id, ..) in COLLECTIONS {
            assert!(
                toml.contains(&format!("id = \"{id}\"")),
                "缺 collection {id}"
            );
        }
        for (subject, _) in SUBJECT_GRANTS {
            assert!(toml.contains(&format!("subject = \"{subject}\"")));
        }
        assert!(toml.contains("log_query = true"));
    }

    #[test]
    fn render_config_toml_rejects_missing_root() {
        let err = render_config_toml(Path::new("/nonexistent-materials-root")).unwrap_err();
        assert!(err.to_string().contains("root 不存在"), "{err}");
    }
}
