//! MVP-17 模型 fallback —— 当规则解析输出不完整时，调用本地模型补全。
//!
//! # Class 3 触发器（用户洞察）
//!
//! 传统设计："解析失败才触发模型"。问题：parser 产出**合法但不完整**的 intent
//! 时（例如看到时间词但没成功提取 `modified_time`），fallback 不触发，缺陷
//! 模型也救不了。
//!
//! 本模块的触发条件是"**结构性遗漏**"：[`signals::scan`] 检出某类信号，但
//! parser 产出的 intent 对应字段为空 → 触发模型 fallback。具体由
//! [`analyze_structural_omissions`] 判定。
//!
//! # 调用流程
//!
//! ```ignore
//! let parsed = parse_with_signals(query);
//! match should_invoke_model(&parsed) {
//!     FallbackDecision::UseParser => parsed.intent,
//!     FallbackDecision::InvokeModel { .. } => {
//!         fallback.invoke(query, &parsed)?  // 调 ModelDaemon
//!     }
//! }
//! ```
//!
//! # 与 MVP-15 / MVP-16 / MVP-02 的关系
//!
//! - 复用 [`crate::prompt::PromptBuilder`]（MVP-16）构造模型输入
//! - 复用 `scout-model-runtime::ModelDaemon`（MVP-15）做推理
//! - 模型输出的 JSON 用 `serde_json::from_str::<SearchIntent>` 校验（schema 严
//!   格校验由调用方接 `scout-harness::SchemaValidator` 完成；本 crate 不强
//!   依赖 harness 以避免循环）

#![allow(clippy::module_name_repetitions)]

use std::fmt;
use std::sync::Arc;

use scout_model_runtime::{GenerateParams, ModelError, SharedModelDaemon};
use scout_search_backend::{MediaType, SearchIntent};

use crate::hybrid::{apply_patch, hybrid_prompt_prefix, hybrid_prompt_suffix, IntentDraft};
use crate::prompt::PromptBuilder;
use crate::signals::{scan, CandidateSignals};

/// 规则解析的扩展输出：intent + 信号扫描结果。
#[derive(Debug, Clone)]
pub struct ParseResult {
    /// 原始查询（BETA-23：内容词覆盖检测需要）。
    pub query: String,
    /// 规则解析得到的 intent。
    pub intent: SearchIntent,
    /// 原始查询中扫描到的信号。
    pub signals: CandidateSignals,
}

/// 在规则解析基础上附加信号扫描。Class 3 触发器的输入。
pub fn parse_with_signals(query: &str) -> ParseResult {
    let intent = crate::parse(query);
    let signals = scan(query);
    ParseResult {
        query: query.to_owned(),
        intent,
        signals,
    }
}

// ============================================================
// 触发器：识别结构性遗漏
// ============================================================

/// Fallback 是否触发的判断结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackDecision {
    /// parser 输出已经够好，不必触发模型。
    UseParser,
    /// 触发模型 fallback，附带触发原因。
    InvokeModel(FallbackReason),
}

/// 模型 fallback 触发原因。Tracer / UI 应当上报本字段帮助调试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackReason {
    /// parser 显式返回 `Clarify`（说明它知道自己不行）。
    ParserClarified,
    /// 信号检出但 parser 漏字段（结构性遗漏，用户 Class 3 洞察）。
    StructuralOmission {
        /// 漏掉的字段名列表（"time" / "size" / "sort" / "location" / "action" / "media" / "keywords"）。
        fields: Vec<&'static str>,
    },
}

/// BETA-23：命名结构噪声词——覆盖检测时从残留段中剥离（不算未覆盖内容）。
/// BETA-24 补「开头/结尾」：「名字以X开头」类结构定位词，与「文件名/名字」同类框架词，
/// 非自由内容（实测「会议纪要开头的文档」残留出「开头」误触发）。
const COVERAGE_NOISE_WORDS: &[&str] = &["文件名", "名字", "名称", "开头", "结尾"];

/// BETA-24：MediaSearch 臂覆盖检测专用——媒体框架噪声词，在 FileSearch
/// 词表基础上叠加剥离。BETA-23 实测正是这些词未剥导致 +4.7% 误触发。
/// 按字符数降序排列（先替长词，防短词先替碎长词留残渣）。
const MEDIA_COVERAGE_NOISE_WORDS: &[&str] = &[
    // 英文（≥3 字母才可能构成 content run；两字母词如 by 不必列）
    "playlists",
    "playlist",
    "lossless",
    "duration",
    "quality", // BETA-24：「high quality songs」品质框架词
    "minutes",
    "longer", // BETA-24：「songs longer than X」时长框架词
    "albums",
    "artist",
    "tracks",
    "listen",
    "hours", // BETA-24：「longer than 2 hours」时长单位框架词
    "songs",
    "music",
    "album",
    "track",
    "high", // BETA-24：「high quality」品质框架词
    "song",
    "play",
    // 中文
    "播放列表",
    "高品质",
    "高音质",
    "格式", // BETA-24：「无损格式的歌曲」格式框架词
    "来点", // BETA-24：「来点摇滚歌曲」口语框架词（给我来点…）
    "放点", // BETA-24：「放点古典音乐」口语框架词（放点/放些…）
    "放些", // BETA-24：同上口语变体
    "放首", // BETA-24：「放首…唱的」口语框架词（放一首…）
    "歌单",
    "歌曲",
    "单曲",
    "专辑",
    "音乐",
    "一首",
    "几首",
    "两首",
    "三首",
    "无损",
    "音质",
    "品质",
    "时长",
    "分钟",
    "小时",
    "播放",
    "听听",
    "的歌",
    "歌",
];

/// BETA-24：类型词（file_type/media_type 的中文表层词）——内容词覆盖检测时剥离，
/// 不算自由关键词。`ZH_CONTAINER_NOUNS` 已含 文件/图片/照片，此处补它缺的类型词
/// （文档/视频/截图/演示文稿…）。**不并入 `ZH_CONTAINER_NOUNS`**：那是 parser 复用
/// 常量，动它会破 v0.5/v0.9 byte-equal。按字符数降序排列。
const FILE_TYPE_NOISE_WORDS: &[&str] = &[
    "电子表格",
    "演示文稿",
    "幻灯片",
    "文件夹",
    "压缩包",
    "可执行",
    "文档",
    "视频",
    "截图",
    "表格",
    "音频",
    "代码",
    "脚本",
];

/// BETA-24：常见曲风词——媒体臂内容词覆盖检测时剥离。parser 偶尔漏抽 genre
/// （如「放点古典音乐」未抽 genre=古典），导致 genre 词被当未覆盖内容误触发。
/// held-out 媒体样本是主题/情境词（毕业旅行…）非曲风，剥曲风不伤其召回。
const MEDIA_GENRE_NOISE_WORDS: &[&str] = &[
    "轻音乐",
    "古典",
    "摇滚",
    "爵士",
    "流行",
    "民谣",
    "电子",
    "嘻哈",
    "说唱",
    "乡村",
    "蓝调",
    "古风",
    "国风",
    "金属",
    "朋克",
];

/// intent 中「已覆盖」的内容值：keywords / extensions / artist / album / title /
/// genre / location.hint。残留段中被这些值吃掉的部分不算遗漏。
fn covered_values(intent: &SearchIntent) -> Vec<String> {
    let mut vals: Vec<String> = Vec::new();
    let push_list = |vals: &mut Vec<String>, list: &Option<Vec<String>>| {
        if let Some(items) = list {
            vals.extend(items.iter().cloned());
        }
    };
    match intent {
        SearchIntent::FileSearch(fs) => {
            push_list(&mut vals, &fs.keywords);
            push_list(&mut vals, &fs.extensions);
            if let Some(hint) = fs.location.as_ref().and_then(|loc| loc.hint.as_ref()) {
                vals.push(hint.clone());
            }
        }
        SearchIntent::MediaSearch(ms) => {
            push_list(&mut vals, &ms.keywords);
            push_list(&mut vals, &ms.extensions);
            for s in [&ms.artist, &ms.album, &ms.title, &ms.genre]
                .into_iter()
                .flatten()
            {
                vals.push(s.clone());
            }
            if let Some(hint) = ms.location.as_ref().and_then(|loc| loc.hint.as_ref()) {
                vals.push(hint.clone());
            }
        }
        _ => {}
    }
    vals
}

/// 残留段去掉已覆盖值与噪声词后，是否仍含「实质内容」（≥2 连续 CJK 或 ≥3 字母英文词）。
fn has_uncovered_content(query: &str, intent: &SearchIntent) -> bool {
    let mut covered: Vec<String> = covered_values(intent)
        .into_iter()
        .map(|v| v.to_lowercase())
        .filter(|v| !v.is_empty())
        .collect();
    // BETA-23 review：按字符数降序替换——互为子串的 covered 值（如 artist=「周杰伦」、
    // album=「周杰伦的床边故事」）先替换短值会把长值打碎留残渣误触发。
    covered.sort_by_key(|w| std::cmp::Reverse(w.chars().count()));
    for seg in crate::parsers::file_search::residual_content_segments(query) {
        let mut s = seg.to_lowercase();
        // BETA-23 review：反向包含——covered 值是整段的超串（如 parser 把字段值提成
        // 带脏前缀的「找邓紫棋」而段是「邓紫棋」）时，该段同样视为已覆盖。
        if covered.iter().any(|c| c.contains(s.as_str())) {
            continue;
        }
        for c in &covered {
            s = s.replace(c.as_str(), " ");
        }
        for w in COVERAGE_NOISE_WORDS {
            s = s.replace(w, " ");
        }
        if matches!(intent, SearchIntent::MediaSearch(_)) {
            for w in MEDIA_COVERAGE_NOISE_WORDS {
                s = s.replace(w, " ");
            }
        }
        for w in crate::parsers::file_search::ZH_CONTAINER_NOUNS {
            s = s.replace(w, " ");
        }
        // BETA-24：类型词（文档/视频/截图…）是 parser 的 file_type/media_type 表层词，
        // 不属自由内容关键词。ZH_CONTAINER_NOUNS 是 parser 复用常量（动它破 byte-equal），
        // 故在此局部补剥；媒体臂另剥常见曲风词（古典/摇滚…，parser genre 漏抽时防误触发）。
        for w in FILE_TYPE_NOISE_WORDS {
            s = s.replace(w, " ");
        }
        if matches!(intent, SearchIntent::MediaSearch(_)) {
            for w in MEDIA_GENRE_NOISE_WORDS {
                s = s.replace(w, " ");
            }
        }
        if has_content_run(&s) {
            return true;
        }
    }
    false
}

/// ≥2 连续 CJK 字符，或 ≥3 连续 ASCII 字母 → 视为实质内容。
fn has_content_run(s: &str) -> bool {
    let mut cjk_run = 0usize;
    let mut ascii_run = 0usize;
    for c in s.chars() {
        if crate::parsers::common::is_cjk(c) {
            cjk_run += 1;
            ascii_run = 0;
            if cjk_run >= 2 {
                return true;
            }
        } else if c.is_ascii_alphabetic() {
            ascii_run += 1;
            cjk_run = 0;
            if ascii_run >= 3 {
                return true;
            }
        } else {
            cjk_run = 0;
            ascii_run = 0;
        }
    }
    false
}

/// 检查 parser 输出 vs 信号扫描的结构性遗漏。
///
/// 规则：
///
/// - **time** 信号检出 + intent 没设任何时间字段（modified/created/accessed）→ 遗漏
/// - **size** 信号检出 + intent 没设 size 字段（且 sort 不是 size_* 排序）→ 遗漏
/// - **sort** 信号检出 + intent 没设 sort 字段（保留 RelevanceDesc 默认不算）→ 遗漏
/// - **location** 信号检出 + intent 没设 location 字段 → 遗漏
/// - **action** 信号检出 + intent 不是 FileAction / Clarify → 遗漏
/// - **media** 信号检出 + intent 不是 MediaSearch → 遗漏
/// - **keywords**（BETA-23 FileSearch 臂 / BETA-24 MediaSearch 臂）：query 残留段
///   去掉已覆盖值/噪声词后仍含实质内容 → 遗漏（媒体臂叠加剥离媒体框架噪声词表）
///
/// FileAction / Refine / Clarify 这几种 intent 本身不期望含 file_search 类
/// 字段，故跳过 time / size / sort / location 的遗漏检查。
/// keywords 字段是否为空（None 或空 vec）——fill-empty-only 判定（MSRV 1.80，
/// 不用 `Option::is_none_or`〔1.82〕，用已在用的 `is_some_and`〔1.70〕）。
fn keywords_is_empty(kw: &Option<Vec<String>>) -> bool {
    !kw.as_ref().is_some_and(|k| !k.is_empty())
}

#[must_use]
pub fn analyze_structural_omissions(parsed: &ParseResult) -> Vec<&'static str> {
    let mut missing = Vec::new();
    let signals = &parsed.signals;

    match &parsed.intent {
        SearchIntent::FileSearch(fs) => {
            if signals.time
                && fs.modified_time.is_none()
                && fs.created_time.is_none()
                && fs.accessed_time.is_none()
            {
                missing.push("time");
            }
            if signals.size && fs.size.is_none() && !is_size_sort(fs.sort) {
                missing.push("size");
            }
            if signals.sort && fs.sort.is_none() {
                missing.push("sort");
            }
            if signals.location && fs.location.is_none() {
                missing.push("location");
            }
            if signals.action {
                missing.push("action");
            }
            if signals.media {
                missing.push("media");
            }
            // BETA-13-G13：fill-empty-only —— parser 已抽出 keywords 时不标 fillable。
            // 追加已填 keywords 在受闸评测集（v0.5/v0.9）净收益 0、净伤害 14（模型把
            // file_type 来源词如「合同/截图」回声进已对的 keywords）；只在 parser 零抽词
            // 时让模型从头补（BETA-23 核心价值，无回退风险）。
            if keywords_is_empty(&fs.keywords)
                && has_uncovered_content(&parsed.query, &parsed.intent)
            {
                missing.push("keywords");
            }
        }
        SearchIntent::MediaSearch(ms) => {
            if signals.time
                && ms.modified_time.is_none()
                && ms.created_time.is_none()
                && ms.accessed_time.is_none()
            {
                missing.push("time");
            }
            if signals.size && ms.size.is_none() && !is_size_sort(ms.sort) {
                missing.push("size");
            }
            if signals.sort && ms.sort.is_none() {
                missing.push("sort");
            }
            if signals.location && ms.location.is_none() {
                missing.push("location");
            }
            if signals.action {
                missing.push("action");
            }
            // BETA-24：媒体臂内容词覆盖检测**限定 media_type=Audio**（音乐）。
            // 自由主题/情境关键词只在音乐查询有意义（held-out 媒体样本全是音乐主题词）；
            // screenshot/image/video 查询的「内容」是被搜对象，v0.9 标注不作 keywords
            // （如「找上周截的 synthetic-receipt 截图」标注无 keywords）——放开会回退。
            // 误触发门见 fire-rate 报告（Task 2）；audio-only 收口见 Task 9 回归修复。
            // BETA-13-G13：同 FileSearch 臂——parser 已抽出 keywords 时不标 fillable。
            if matches!(ms.media_type, MediaType::Audio)
                && keywords_is_empty(&ms.keywords)
                && has_uncovered_content(&parsed.query, &parsed.intent)
            {
                missing.push("keywords");
            }
        }
        SearchIntent::FileAction(_) | SearchIntent::Refine(_) => {
            // 这两种 intent 不期望持有 file_search 字段；signals.action 也已被 intent 形态体现
        }
        SearchIntent::Clarify(_) => {
            // Clarify 自己就是 fallback 信号；本函数返回空，由 should_invoke_model 单独处理
        }
    }

    missing
}

fn is_size_sort(sort: Option<scout_search_backend::SortOrder>) -> bool {
    use scout_search_backend::SortOrder;
    matches!(sort, Some(SortOrder::SizeAsc | SortOrder::SizeDesc))
}

/// 综合决策：parser 给出 Clarify → 触发；结构性遗漏 → 触发；否则用 parser。
#[must_use]
pub fn should_invoke_model(parsed: &ParseResult) -> FallbackDecision {
    if matches!(parsed.intent, SearchIntent::Clarify(_)) {
        return FallbackDecision::InvokeModel(FallbackReason::ParserClarified);
    }
    let missing = analyze_structural_omissions(parsed);
    if missing.is_empty() {
        FallbackDecision::UseParser
    } else {
        FallbackDecision::InvokeModel(FallbackReason::StructuralOmission { fields: missing })
    }
}

// ============================================================
// 模型调用编排
// ============================================================

/// 模型 fallback 调用错误。
#[derive(Debug)]
pub enum FallbackError {
    /// 模型推理失败。
    Model(ModelError),
    /// 模型输出不是合法 JSON。
    InvalidJson(String),
    /// 模型输出 JSON 不能反序列化为 `SearchIntent`。
    InvalidIntent(String),
}

impl fmt::Display for FallbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Model(err) => write!(f, "model inference failed: {err}"),
            Self::InvalidJson(detail) => write!(f, "model output not valid JSON: {detail}"),
            Self::InvalidIntent(detail) => {
                write!(f, "model output not valid SearchIntent: {detail}")
            }
        }
    }
}

impl std::error::Error for FallbackError {}

/// 模型 fallback 编排器。
pub struct ModelFallback {
    daemon: SharedModelDaemon,
    prompt_builder: PromptBuilder,
    generate_params: GenerateParams,
    /// MVP-17 v0.3：true 时走 [`crate::hybrid`] 混合路径（parser 锁 variant，
    /// 模型只填字段补丁）；false 时走 v0.2 全 JSON 重写路径。
    hybrid_mode: bool,
}

impl fmt::Debug for ModelFallback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelFallback")
            .field("generate_params", &self.generate_params)
            .finish_non_exhaustive()
    }
}

impl ModelFallback {
    /// 用默认 [`PromptBuilder`] 和**针对 JSON 任务优化**的 [`GenerateParams`] 构造。
    ///
    /// JSON 输出任务需要低温 + 紧停止：
    /// - `temperature = 0.1`（接近 greedy，避免字段值幻觉）
    /// - `top_p = 0.95`
    /// - `max_tokens = 256`（SearchIntent JSON 通常 < 200 token，过大易触发"复读 JSON
    ///   到 max_tokens"模式）
    #[must_use]
    pub fn new(daemon: SharedModelDaemon) -> Self {
        let generate_params = GenerateParams {
            max_tokens: 256,
            temperature: 0.1,
            top_p: 0.95,
            stop_sequences: Vec::new(),
            seed: 42,
            grammar: None,
            // BETA-17：full / hybrid 两条路径都只取首个 JSON 对象，故开启"首个对象闭合即停"。
            // 小模型常把同一 JSON 复读到 max_tokens，弱核显上这些多余 token 是延迟主因；
            // 停在首个对象省掉无效 decode，准确率不变（解析口径本就只看第一个对象）。
            stop_at_json: true,
        };
        Self {
            daemon,
            prompt_builder: PromptBuilder::default(),
            generate_params,
            hybrid_mode: false,
        }
    }

    /// MVP-17 v0.3：启用 hybrid 模式（parser 锁 variant + 模型填字段）。
    ///
    /// 启用后 [`Self::invoke`] 自动走 hybrid 路径；不需要调用方区分。
    /// hybrid 模式预期较小 max_tokens 足够（patch 通常 < 80 token），但保留默认。
    #[must_use]
    pub fn with_hybrid_mode(mut self) -> Self {
        self.hybrid_mode = true;
        self
    }

    /// 当前是否启用 hybrid 模式。
    #[must_use]
    pub const fn is_hybrid(&self) -> bool {
        self.hybrid_mode
    }

    /// MVP-17 v0.2：附加 GBNF grammar 约束。llama-cpp 后端会用
    /// `LlamaSampler::grammar` 在采样阶段就排除非法 token。
    ///
    /// 推荐配合 [`crate::SEARCH_INTENT_GBNF`] 使用。
    ///
    /// **⚠️ 当前 llama-cpp-4 0.3.0 + Qwen2.5-1.5B 组合下会 panic**（"Unexpected
    /// empty grammar stack after accepting piece"）。本接口已就绪，等 llama-cpp-4
    /// 升级（或换 ≥ llama.cpp v0.3.x 底层）后可直接启用。
    #[must_use]
    pub fn with_grammar(mut self, gbnf: impl Into<String>) -> Self {
        self.generate_params.grammar = Some(gbnf.into());
        self
    }

    /// 自定义 prompt 与生成参数。
    #[must_use]
    pub fn with_overrides(
        mut self,
        prompt_builder: PromptBuilder,
        generate_params: GenerateParams,
    ) -> Self {
        self.prompt_builder = prompt_builder;
        self.generate_params = generate_params;
        self
    }

    /// 调用模型推理 + 解析为 [`SearchIntent`]。**调用方应当先调
    /// [`should_invoke_model`] 判断是否值得调本函数**。
    ///
    /// **分派**：`hybrid_mode = true` 时走 [`Self::invoke_hybrid`]；否则走原
    /// v0.2 全 JSON 重写路径。
    pub fn invoke(&self, query: &str) -> Result<SearchIntent, FallbackError> {
        if self.hybrid_mode {
            let draft = IntentDraft::from_query(query);
            self.invoke_hybrid(query, &draft)
        } else {
            self.invoke_full(query)
        }
    }

    /// v0.2 全 JSON 重写路径：让模型输出完整 SearchIntent JSON。
    ///
    /// **解析策略**：用 `serde_json::Deserializer::into_iter` 只取第一个 JSON
    /// 对象。1.5B 小模型在低温下偶尔会"输完后继续把同一 JSON 复读"直到 max_tokens；
    /// 如果直接 `from_str` 会因为多 JSON 拼接而 InvalidIntent。
    fn invoke_full(&self, query: &str) -> Result<SearchIntent, FallbackError> {
        let prompt = self.build_full_prompt(query);
        let raw = self
            .daemon
            .generate(&prompt, &self.generate_params)
            .map_err(FallbackError::Model)?;
        let cleaned = strip_code_fence(&raw);
        let mut stream = serde_json::Deserializer::from_str(cleaned).into_iter::<SearchIntent>();
        match stream.next() {
            Some(Ok(intent)) => Ok(intent),
            Some(Err(err)) => Err(FallbackError::InvalidIntent(err.to_string())),
            None => Err(FallbackError::InvalidJson(
                "model output contained no JSON".to_owned(),
            )),
        }
    }

    /// MVP-17 v0.3 hybrid 路径：parser 锁定 variant + 已知字段，模型只输出
    /// 字段 patch JSON。merge 后回 SearchIntent。
    ///
    /// 比 [`Self::invoke_full`] 更稳：模型不能改 variant，不会把 parser 已对的
    /// MediaSearch 推翻成 FileSearch 等。
    ///
    /// **失败回落**：模型 patch 不合法时返回 [`FallbackError::InvalidIntent`]，
    /// 调用方应当回落到 draft 自身（即 parser 输出）。
    pub fn invoke_hybrid(
        &self,
        query: &str,
        draft: &IntentDraft,
    ) -> Result<SearchIntent, FallbackError> {
        // BETA-17：固定指令前缀 + 每条 query 尾巴 分离传入，让 model-runtime 的 worker
        // 复用前缀 KV（前缀只 prefill 一次）。语义等价于喂 `build_hybrid_prompt` 整串。
        let raw = self
            .daemon
            .generate_cached_prefix(
                hybrid_prompt_prefix(),
                &hybrid_prompt_suffix(query, draft),
                &self.generate_params,
            )
            .map_err(FallbackError::Model)?;
        let cleaned = strip_code_fence(&raw);
        let mut stream =
            serde_json::Deserializer::from_str(cleaned).into_iter::<serde_json::Value>();
        let patch = match stream.next() {
            Some(Ok(v)) => v,
            Some(Err(err)) => return Err(FallbackError::InvalidIntent(err.to_string())),
            None => {
                return Err(FallbackError::InvalidJson(
                    "model hybrid patch contained no JSON".to_owned(),
                ));
            }
        };
        apply_patch(draft, &patch).map_err(|err| FallbackError::InvalidIntent(err.to_string()))
    }

    /// 复合 prompt：system + user_prompt（user_prompt 内部已包含 few-shots）。
    fn build_full_prompt(&self, query: &str) -> String {
        let mut buf = String::new();
        buf.push_str(&self.prompt_builder.system_prompt());
        buf.push_str("\n\n");
        buf.push_str(&self.prompt_builder.user_prompt(query));
        buf
    }
}

/// 去除模型偶尔加的 ```json ... ``` 围栏。MVP-16 prompt 已要求不要包，
/// 此处兜底。
fn strip_code_fence(raw: &str) -> &str {
    let trimmed = raw.trim();
    let s = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    s.trim().strip_suffix("```").unwrap_or(s).trim()
}

/// 便捷入口：一次性走完 parse + 信号扫描 + fallback 决策 + 必要时调模型。
///
/// 如果不需要模型 fallback（决策 UseParser），返回 `Ok(intent)`；
/// 决策 InvokeModel 时调 `fallback.invoke(query)` 并返回模型结果（成功）或
/// 把模型 error wrap 成 [`FallbackError`]（失败）。
///
/// **`fallback = None` 时**：决策即便是 InvokeModel，也只返回 parser 的 intent
/// （forward compatibility — CLI / 早期 dev box 无模型时仍能跑）。
pub fn resolve_intent(
    query: &str,
    fallback: Option<&ModelFallback>,
) -> Result<ResolvedIntent, FallbackError> {
    let parsed = parse_with_signals(query);
    let decision = should_invoke_model(&parsed);

    let (intent, source) = match (&decision, fallback) {
        (FallbackDecision::UseParser, _) => (parsed.intent.clone(), IntentSource::Parser),
        (FallbackDecision::InvokeModel(_), None) => {
            (parsed.intent.clone(), IntentSource::ParserNoFallback)
        }
        (FallbackDecision::InvokeModel(_), Some(fb)) => (fb.invoke(query)?, IntentSource::Model),
    };

    Ok(ResolvedIntent {
        intent,
        source,
        decision,
        signals: parsed.signals,
    })
}

/// 最终 intent 来自哪里。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentSource {
    /// 规则解析直接产出。
    Parser,
    /// 触发器认为该调模型，但调用方未提供 fallback；用 parser 兜底。
    ParserNoFallback,
    /// 模型 fallback 产出。
    Model,
}

/// [`resolve_intent`] 的完整输出。
#[derive(Debug, Clone)]
pub struct ResolvedIntent {
    /// 最终 intent。
    pub intent: SearchIntent,
    /// 来源。
    pub source: IntentSource,
    /// fallback 决策（即便最终用了 parser）。
    pub decision: FallbackDecision,
    /// 信号扫描结果。
    pub signals: CandidateSignals,
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use scout_model_runtime::ModelDaemon;
    use scout_search_backend::{
        FileSearch, FileType, Language, Location, RelativeTime, SchemaVersion, SearchIntent,
        SortOrder, TimeExpression,
    };
    use std::path::PathBuf;

    fn mk_file_search_full() -> FileSearch {
        FileSearch {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            keywords: None,
            extensions: Some(vec!["pptx".to_owned()]),
            file_type: Some(vec![FileType::Presentation]),
            location: Some(Location {
                hint: Some("下载".to_owned()),
                include: None,
                exclude: None,
            }),
            modified_time: Some(TimeExpression::Relative {
                value: RelativeTime::Yesterday,
            }),
            created_time: None,
            accessed_time: None,
            size: None,
            exclude_extensions: None,
            exclude_file_type: None,
            sort: Some(SortOrder::ModifiedDesc),
            limit: None,
        }
    }

    fn mk_file_search_bare() -> FileSearch {
        FileSearch {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            keywords: Some(vec!["ppt".to_owned()]),
            extensions: None,
            file_type: None,
            location: None,
            modified_time: None,
            created_time: None,
            accessed_time: None,
            size: None,
            exclude_extensions: None,
            exclude_file_type: None,
            sort: None,
            limit: None,
        }
    }

    // ---- 触发器：结构性遗漏检测 ----

    /// 用户 Class 3 主场景：查询 B "一周内编辑过的ppt" — parser 漏 modified_time
    #[test]
    fn structural_omission_time_missing_triggers_model() {
        let parsed = ParseResult {
            query: "一周内编辑过的ppt".to_owned(),
            intent: SearchIntent::FileSearch(mk_file_search_bare()),
            signals: CandidateSignals {
                time: true,
                ..CandidateSignals::default()
            },
        };
        let decision = should_invoke_model(&parsed);
        let FallbackDecision::InvokeModel(FallbackReason::StructuralOmission { fields }) = decision
        else {
            panic!("expected InvokeModel(StructuralOmission)")
        };
        assert!(fields.contains(&"time"));
    }

    /// 用户 Class 1 / Class 3 场景：查询 A "最近一周下载的最大的文件"
    /// parser 漏 size（"最大"）
    #[test]
    fn structural_omission_size_missing_triggers_model() {
        let parsed = ParseResult {
            query: "最近一周下载的最大的文件".to_owned(),
            intent: SearchIntent::FileSearch(mk_file_search_bare()),
            signals: CandidateSignals {
                time: true,
                size: true,
                location: true,
                ..CandidateSignals::default()
            },
        };
        let decision = should_invoke_model(&parsed);
        let FallbackDecision::InvokeModel(FallbackReason::StructuralOmission { fields }) = decision
        else {
            panic!("expected InvokeModel(StructuralOmission)")
        };
        assert!(fields.contains(&"time"));
        assert!(fields.contains(&"size"));
        assert!(fields.contains(&"location"));
    }

    /// parser 输出已经完整，无信号遗漏 → 不触发
    #[test]
    fn complete_intent_uses_parser() {
        let parsed = ParseResult {
            query: String::new(),
            intent: SearchIntent::FileSearch(mk_file_search_full()),
            signals: CandidateSignals {
                time: true,
                location: true,
                sort: true,
                ..CandidateSignals::default()
            },
        };
        assert_eq!(should_invoke_model(&parsed), FallbackDecision::UseParser);
    }

    /// 大小已通过 size_desc 排序体现 → 不视为遗漏
    #[test]
    fn size_sort_satisfies_size_signal() {
        let mut fs = mk_file_search_bare();
        fs.sort = Some(SortOrder::SizeDesc);
        let parsed = ParseResult {
            query: String::new(),
            intent: SearchIntent::FileSearch(fs),
            signals: CandidateSignals {
                size: true,
                ..CandidateSignals::default()
            },
        };
        let omissions = analyze_structural_omissions(&parsed);
        assert!(!omissions.contains(&"size"), "size sort 已隐含 size 信号");
    }

    /// 信号全部为 false → 即便 intent 缺字段也不该触发
    #[test]
    fn no_signals_uses_parser() {
        let parsed = ParseResult {
            query: String::new(),
            intent: SearchIntent::FileSearch(mk_file_search_bare()),
            signals: CandidateSignals::default(),
        };
        assert_eq!(should_invoke_model(&parsed), FallbackDecision::UseParser);
    }

    /// parser 显式返回 Clarify → 直接触发模型
    #[test]
    fn clarify_intent_triggers_model() {
        use scout_search_backend::{Clarify, ClarifyReason};
        let parsed = ParseResult {
            query: String::new(),
            intent: SearchIntent::Clarify(Clarify {
                schema_version: SchemaVersion::V1,
                language: Some(Language::Zh),
                reason: ClarifyReason::AmbiguousTime,
                question: "什么时候?".to_owned(),
                options: None,
            }),
            signals: CandidateSignals::default(),
        };
        assert_eq!(
            should_invoke_model(&parsed),
            FallbackDecision::InvokeModel(FallbackReason::ParserClarified)
        );
    }

    // ---- BETA-23：内容词覆盖检测（keywords 结构性遗漏）----

    #[test]
    fn populated_keywords_no_longer_trigger_omission() {
        // BETA-13-G13：fill-empty-only 取代旧 BETA-24「追加已填 keywords」。
        // 「2025年的会议纪要文件名包含运维」parser 已抽出 [运维]（非空）——旧契约会
        // 标 keywords fillable 让模型补「会议纪要」（held-out 90%），但该追加机制在受闸
        // 评测集（v0.5/v0.9）净伤害 14、净收益 0（模型把 file_type 词回声进已对 keywords），
        // 故收为 fill-empty-only：parser 已抽词则不再标 fillable，模型不得追加。
        let parsed = parse_with_signals("2025年的会议纪要文件名包含运维");
        let SearchIntent::FileSearch(fs) = &parsed.intent else {
            panic!("应解析为 FileSearch");
        };
        assert!(
            fs.keywords.as_ref().is_some_and(|k| !k.is_empty()),
            "前提：parser 已抽出非空 keywords"
        );
        let missing = analyze_structural_omissions(&parsed);
        assert!(
            !missing.contains(&"keywords"),
            "parser 已抽词，不再标 keywords fillable：missing={missing:?}"
        );
    }

    #[test]
    fn keywords_not_fillable_when_parser_already_extracted() {
        // BETA-13-G13：fill-empty-only —— parser 已抽出 keywords 时不再标 keywords 为
        // fillable，杜绝模型在 hybrid 下追加类型词污染（实测 v0.9 12 条回退，如
        // 「正文包含张伟的合同」parser 已对地给 [张伟]，模型却追加 file_type 词「合同」）。
        for q in [
            "正文包含张伟的合同",
            "截图里写着余额不足的报错",
            "提到李娜的简历",
        ] {
            let parsed = parse_with_signals(q);
            // 前提：parser 已抽出非空 keywords
            let has_kw = match &parsed.intent {
                SearchIntent::FileSearch(fs) => fs.keywords.as_ref().is_some_and(|k| !k.is_empty()),
                SearchIntent::MediaSearch(ms) => {
                    ms.keywords.as_ref().is_some_and(|k| !k.is_empty())
                }
                _ => false,
            };
            assert!(has_kw, "前提：{q} parser 应已抽出非空 keywords");
            let missing = analyze_structural_omissions(&parsed);
            assert!(
                !missing.contains(&"keywords"),
                "parser 已填 keywords 不应再标 fillable，q={q} missing={missing:?}"
            );
        }
    }

    #[test]
    fn covered_queries_do_not_trigger_keywords_omission() {
        // parser 已全覆盖的查询不得误触发（反例集）
        let mut failed: Vec<String> = Vec::new();
        for q in [
            "上周的pdf",
            "找最大的文件",
            "周杰伦的歌",
            "find the annual budget report",
            "找名字里有「预算」的文件",
            // BETA-23 review：媒体侧已验证误触发样本（撤掉 MediaSearch 臂后不得再触发）
            "songs by Adele",
            "时长不到3分钟的歌曲",
            "找邓紫棋的歌曲",
        ] {
            let parsed = parse_with_signals(q);
            let missing = analyze_structural_omissions(&parsed);
            if missing.contains(&"keywords") {
                failed.push(format!("query={q} missing={missing:?}"));
            }
        }
        assert!(failed.is_empty(), "误触发样本：{failed:#?}");
    }

    /// BETA-24：MediaSearch 臂启用 keywords 覆盖检测——parser 真漏内容词的
    /// 媒体查询应触发（媒体框架噪声词表已叠加剥离，BETA-23 +4.7% 误触发由词表解决）。
    #[test]
    fn media_search_keywords_omission_fires_on_true_omission() {
        for q in [
            "适合跑步节奏感强的音乐",
            "play some indie tracks about rainy nights",
        ] {
            let parsed = parse_with_signals(q);
            assert!(
                matches!(parsed.intent, SearchIntent::MediaSearch(_)),
                "前提：{q} 应解析为 MediaSearch，实际 intent={:?}",
                parsed.intent
            );
            let missing = analyze_structural_omissions(&parsed);
            assert!(
                missing.contains(&"keywords"),
                "媒体臂真遗漏应触发 keywords，q={q} missing={missing:?}"
            );
        }
    }

    /// BETA-24：媒体框架词（songs by / 一首 / 无损 / 时长…）不得令媒体臂误触发。
    /// 反例集来自 BETA-23 review 实测误触发样本 + 常见媒体模板。
    #[test]
    fn media_framework_words_do_not_trigger_keywords_omission() {
        let mut failed: Vec<String> = Vec::new();
        for q in [
            "songs by Adele",
            "时长不到3分钟的歌曲",
            "找邓紫棋的歌曲",
            "周杰伦的歌",
            "来一首无损音质的单曲",
            "play some music",
            "高品质的专辑",
            "最近添加的playlist",
            // BETA-24 fire-rate 实测补充：v0.9 媒体模板里 parser 已全覆盖、仅剩
            // 品质/时长/口语框架词的 case，词表收紧后不得误触发。
            "find high quality songs",
            "songs longer than 5 minutes",
            "audio files longer than 2 hours",
            "high quality songs by Bruno Mars",
            "来点摇滚歌曲",
            "无损格式的歌曲",
            "放首李荣浩唱的",
        ] {
            let parsed = parse_with_signals(q);
            let missing = analyze_structural_omissions(&parsed);
            if missing.contains(&"keywords") {
                failed.push(format!("query={q} missing={missing:?}"));
            }
        }
        assert!(failed.is_empty(), "媒体框架词误触发：{failed:#?}");
    }

    /// BETA-24：把 `MEDIA_COVERAGE_NOISE_WORDS` 的「按字符数降序」从注释约定升级为
    /// 测试期不变量——剥离按声明序执行，短词排在其超串之前会打碎长词留残渣。
    /// 注：同字符数相邻不要求（无子串关系），只禁止长度回升。
    #[test]
    fn media_noise_words_are_length_descending() {
        // BETA-24：三张按声明序剥离的词表都守降序不变量（短词排在超串前会打碎长词）。
        for (name, list) in [
            ("MEDIA_COVERAGE_NOISE_WORDS", MEDIA_COVERAGE_NOISE_WORDS),
            ("FILE_TYPE_NOISE_WORDS", FILE_TYPE_NOISE_WORDS),
            ("MEDIA_GENRE_NOISE_WORDS", MEDIA_GENRE_NOISE_WORDS),
        ] {
            let offenders: Vec<String> = list
                .windows(2)
                .filter(|w| w[0].chars().count() < w[1].chars().count())
                .map(|w| format!("{} < {}", w[0], w[1]))
                .collect();
            assert!(
                offenders.is_empty(),
                "{name} 必须按字符数降序，违例：{offenders:?}"
            );
        }
    }

    /// BETA-24：媒体臂 keywords 检测限定 media_type=Audio——非音乐媒体（screenshot/
    /// image/video）的内容残留不得触发 keywords（v0.9 标注不把截图主题作 keywords）。
    #[test]
    fn non_audio_media_does_not_trigger_keywords() {
        // 「找上周截的 synthetic-receipt 截图」→ MediaSearch(screenshot)，内容词残留
        // 但非 audio，不应进 omissions（修复前此型 10 条回归）。
        let parsed = parse_with_signals("找上周截的 synthetic-receipt 截图");
        assert!(
            matches!(
                &parsed.intent,
                SearchIntent::MediaSearch(ms) if !matches!(ms.media_type, MediaType::Audio)
            ),
            "前提：该查询应解析为非 audio 的 MediaSearch，实际 {:?}",
            parsed.intent
        );
        let missing = analyze_structural_omissions(&parsed);
        assert!(
            !missing.contains(&"keywords"),
            "非 audio 媒体不得触发 keywords，missing={missing:?}"
        );
    }

    /// BETA-23 review：覆盖判断的「反向包含」——covered 值是残留段的**超串**时
    /// （如 parser 把 keywords 提成带脏前缀的「找邓紫棋」而段是「邓紫棋」），
    /// 该段也算已覆盖，不触发 keywords 遗漏。
    #[test]
    fn covered_superstring_counts_as_covered() {
        let mut fs = mk_file_search_bare();
        // covered 值「找邓紫棋的资料」是残留段「邓紫棋的资料」（来自 query）的超串
        fs.keywords = Some(vec!["找邓紫棋的资料".to_owned()]);
        let parsed = ParseResult {
            query: "找邓紫棋的资料".to_owned(),
            intent: SearchIntent::FileSearch(fs),
            signals: CandidateSignals::default(),
        };
        let missing = analyze_structural_omissions(&parsed);
        assert!(
            !missing.contains(&"keywords"),
            "残留段是 covered 值的子串应视为已覆盖，missing={missing:?}"
        );
    }

    // ---- 端到端 resolve_intent（无 fallback / 有 stub 模型）----

    #[test]
    fn resolve_intent_without_fallback_falls_back_to_parser() {
        // "找最近的" parser v0.2.1 仍会触发 Clarify(AmbiguousTime)，是稳定的
        // fallback 触发点（ParserClarified）。
        let resolved = resolve_intent("找最近的", None).unwrap();
        assert_eq!(resolved.source, IntentSource::ParserNoFallback);
        // 决策标记 InvokeModel(ParserClarified)，便于 Tracer 上报
        assert!(matches!(
            resolved.decision,
            FallbackDecision::InvokeModel(FallbackReason::ParserClarified)
        ));
    }

    #[test]
    fn resolve_intent_with_stub_model_returns_invalid_intent() {
        // StubLoader 的 echo 输出不是合法 JSON → 期望 InvalidIntent
        // 用 "找最近的" 走稳定的 Clarify 触发点
        // 注：本测试只验 stub loader 的 fallback wiring。当 `cargo test --workspace` 经 feature
        // 统一把 model-runtime 的 llama-cpp 真 loader 拉进来时（如 spike-retrieval 无条件开它），
        // 占位 "stub.gguf" 无法被真 loader 加载 → 加载失败即跳过（本测试不适用于真 loader 形态）。
        let Ok(daemon) =
            ModelDaemon::load_blocking(&PathBuf::from("stub.gguf"), Default::default())
        else {
            return;
        };
        let daemon = Arc::new(daemon);
        let fallback = ModelFallback::new(daemon);
        let result = resolve_intent("找最近的", Some(&fallback));
        // stub 不产合法 JSON；走 InvalidIntent 路径，验证 fallback 框架 wiring
        assert!(matches!(result, Err(FallbackError::InvalidIntent(_))));
    }

    /// 用户 Class 1 实测验证：v0.2.1 后 "一周内编辑过的ppt" 应直接 UseParser，
    /// 不再触发 fallback（因为 parser 现在能识别"一周内" → last_7_days）。
    #[test]
    fn parser_v0_2_1_handles_class1_time_synonym() {
        let resolved = resolve_intent("一周内编辑过的ppt", None).unwrap();
        assert_eq!(resolved.source, IntentSource::Parser);
        assert_eq!(resolved.decision, FallbackDecision::UseParser);
    }

    #[test]
    fn resolve_intent_complete_query_uses_parser_directly() {
        let resolved = resolve_intent("budget", None).unwrap();
        assert_eq!(resolved.source, IntentSource::Parser);
        assert_eq!(resolved.decision, FallbackDecision::UseParser);
    }

    // ---- strip_code_fence 工具 ----

    #[test]
    fn strip_code_fence_handles_plain_json() {
        assert_eq!(strip_code_fence("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn strip_code_fence_handles_json_fence() {
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
    }

    #[test]
    fn strip_code_fence_handles_bare_fence() {
        assert_eq!(strip_code_fence("```\n{\"a\":1}\n```"), "{\"a\":1}");
    }
}
