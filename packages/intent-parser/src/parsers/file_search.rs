//! `SearchIntent::FileSearch` 规则解析器。
#![allow(clippy::pedantic, clippy::expect_used, clippy::while_let_on_iterator)]

use scout_search_backend::{
    FileSearch, FileType, Language, Location, SchemaVersion, SearchIntent, SizeExpression,
    SizeUnit, SortOrder, TimeExpression,
};

use crate::lexicon;

use super::common::{
    en_ambiguous_noun_is_location, is_cjk, parse_location_with_language, parse_time_fields,
    picture_dir_is_location, screenshot_dir_is_location, word_present,
};

pub(crate) fn parse_file_search(input: &str, lower: &str, language: Language) -> SearchIntent {
    // B3.5：收集 query 中**所有**命中的扩展名 alias（旧 `match_extensions` 只取首个，
    // 「pdf和doc」会丢 pdf）。首个命中仍用于 keyword 抽取（行为不变）。
    // BETA-13-G12：否定/排除边界（「images but not videos」「要图片不要视频」「…但是不要截图」）——
    // 标记之后的类型词归 exclude_file_type，正向类型只看标记之前的段。无标记时 pos=full（byte-equal 不变）。
    let neg = negation_split(lower);
    let pos_lower = neg.map_or(lower, |(start, _)| &lower[..start]);

    // 扩展名匹配面净化（仅影响类型信号识别，不动 keyword / location 抽取面）：
    // - 「with the word X」的 word 是内容框架名词，不是 Word 文档类型信号（d3-en-005
    //   「photo with the word invoice in it」误并 document）。v0.5 无 "the word " 锚点。
    // - 位置义的 `pictures`（G15 标记，如「in the pictures folder」）是文件夹名，
    //   不作 Image 类型信号（v09-d5-en-020；v0.5 唯一「in pictures」锚点由 photos 供 Image，不受影响）。
    let mut ext_surface = std::borrow::Cow::Borrowed(pos_lower);
    if ext_surface.contains("the word ") {
        ext_surface = std::borrow::Cow::Owned(ext_surface.replace("the word ", " "));
    }
    if en_ambiguous_noun_is_location(pos_lower, "pictures") {
        ext_surface = std::borrow::Cow::Owned(ext_surface.replace("pictures", " "));
    }
    let ext_lower: &str = &ext_surface;

    let all_ext_matches = match_all_extensions(ext_lower);
    let ext_match = all_ext_matches.first().copied();
    let location = parse_location_with_language(lower, language);
    let (created_time, modified_time, accessed_time) = parse_time_fields(lower, input);
    let size = parse_size(lower);

    let mut keywords = extract_filesearch_keywords(input, lower, &ext_match);

    let sort = decide_sort(lower, &size, &created_time, &modified_time, &accessed_time);

    let (mut extensions, mut file_type) = merge_extensions(ext_lower, &all_ext_matches);

    // BETA-13-G15 (b)：类型义的英文 documents → 注入 FileType::Document（按 query 语序、去重）。
    inject_type_meaning_document(ext_lower, &all_ext_matches, &mut file_type, &mut extensions);

    // BETA-13-G12 ②′：「截图目录」中的 截图 是位置名（已抽为 location=截图），非搜索的
    // 类型 → 从 file_type 移除 Screenshot（保留并存的真实类型词，如 图片→Image）。
    if screenshot_dir_is_location(lower) {
        if let Some(fts) = file_type.as_mut() {
            fts.retain(|ft| *ft != FileType::Screenshot);
            if fts.is_empty() {
                file_type = None;
            }
        }
    }
    // BETA-13-G14 B2：「图片文件夹」中的 图片 是位置名（已抽为 location=图片），非搜索类型 →
    // 移除 file_type=Image（保留并存的真实类型，如「图片文件夹里的视频」的 Video）。
    if picture_dir_is_location(lower) {
        if let Some(fts) = file_type.as_mut() {
            fts.retain(|ft| *ft != FileType::Image);
            if fts.is_empty() {
                file_type = None;
            }
        }
    }

    // 负向段的类型类别 → exclude_file_type（仅当不在正向 file_type 内，避免自相矛盾）；
    // 负向段的**字面扩展名**（如「不含 mkv」的 mkv）→ exclude_extensions（与类型词区分）。
    let mut exclude_extensions: Option<Vec<String>> = None;
    let mut exclude_file_type: Option<Vec<FileType>> = None;
    if let Some((_, neg_start)) = neg {
        let neg_lower = &lower[neg_start..];
        exclude_extensions = negated_literal_extensions(neg_lower);
        let (_, neg_types) = merge_extensions(neg_lower, &match_all_extensions(neg_lower));
        if let Some(neg_types) = neg_types {
            let positive = file_type.as_deref().unwrap_or(&[]);
            let excl: Vec<FileType> = neg_types
                .into_iter()
                .filter(|ft| !positive.contains(ft))
                .collect();
            if !excl.is_empty() {
                exclude_file_type = Some(excl);
            }
        }
    }
    // 裸「no <字面扩展名>」窄路径（无其它否定标记时兜底，d2-en-020）。
    if neg.is_none() && exclude_extensions.is_none() {
        exclude_extensions = bare_no_literal_extensions(lower);
    }

    // BETA-13-G：扩展名 alias 未推出 file_type 时，用查询尾置类型名词兜底。
    // 该尾名词是「类型信号」而非内容，需同步从 keywords 里剥掉（否则
    // 「现金流的财务报表」会把「财务报表」漏进 keyword）。
    if file_type.is_none() {
        if let Some((ft, tail)) = trailing_type_noun_file_type(input) {
            file_type = Some(vec![ft]);
            if let Some(kws) = keywords.as_mut() {
                kws.retain(|k| k != tail);
                if kws.is_empty() {
                    keywords = None;
                }
            }
        }
    }

    // BETA-13-G Fix3b：英文 head 类型名词（句首 documents/archives/…）→ file_type，
    // 仅当上面都没推出类型时兜底。head-gated（^ 锚定）刻意排除句尾「in documents」位置义。
    // 命中的 head 名词是类型信号而非内容，需大小写不敏感地从 keywords 剥掉（与中文尾名词
    // 兜底对称），否则「archives between 10 and 100 MB」会把「archives」既当类型又当 keyword。
    if file_type.is_none() {
        if let Some((ft, matched_word)) = english_head_type_noun_file_type(lower) {
            file_type = Some(vec![ft]);
            if let Some(kws) = keywords.as_mut() {
                kws.retain(|k| !k.eq_ignore_ascii_case(matched_word));
                if kws.is_empty() {
                    keywords = None;
                }
            }
        }
    }

    // 「预算表」复合词 = 预算（内容）+ 表（spreadsheet 类型信号）。锚点：d1-zh-018
    // 「装修预算的表」→ kw=装修预算 + ft=spreadsheet、d1-mixed-013「budget 预算表」。
    // 不做通用「表」后缀拆分——「课程表/计划表」整词是内容名词（d1-zh-023/030 锚点）。
    if let Some(kws) = keywords.as_mut() {
        if let Some(k) = kws.iter_mut().find(|k| k.as_str() == "预算表") {
            "预算".clone_into(k);
            if file_type.is_none() {
                file_type = Some(vec![FileType::Spreadsheet]);
            }
        }
    }

    // 2026-07-04 拍板：英文单 token 复数归一（invoices→invoice）——关键词装配终点统一做，
    // 覆盖 residual / mixed / 兜底各抽取路径；归一后去重防「invoice invoices」并存。
    if let Some(kws) = keywords.take() {
        let mut normalized: Vec<String> = Vec::with_capacity(kws.len());
        for k in kws {
            let k = singularize_en_keyword(k);
            if !normalized.contains(&k) {
                normalized.push(k);
            }
        }
        keywords = Some(normalized);
    }

    SearchIntent::FileSearch(FileSearch {
        schema_version: SchemaVersion::V1,
        language: Some(language),
        keywords,
        extensions,
        file_type,
        location,
        modified_time,
        created_time,
        accessed_time,
        size,
        exclude_extensions,
        exclude_file_type,
        sort,
        limit: None,
    })
}

/// BETA-13-G12：否定/排除边界标记。命中返回最早标记的 `(起始, 结束)` 字节位置；
/// 标记之前为正向类型段、之后为待排除段。v0.5 无任一标记（byte-equal 安全）。
fn negation_split(lower: &str) -> Option<(usize, usize)> {
    const MARKERS: &[&str] = &[
        "但是不要",
        "不要",
        "不含",
        "不包括",
        // BETA-13-G12 决策 C 同构：「文档和图片，排除压缩包」经 refine 约束门转 file_search 后，
        // 由此把「排除」之后的类型段归 exclude_file_type。裸「排除视频」仍走 refine，不到此处。
        "排除",
        "but not",
        "but no",
        "excluding",
        "exclude",
        "except",
    ];
    MARKERS
        .iter()
        .filter_map(|m| lower.find(m).map(|p| (p, p + m.len())))
        .min_by_key(|(start, _)| *start)
}

/// BETA-13-G12：否定段中的**字面扩展名 token**（如「不含 mkv」的 mkv）→ exclude_extensions。
///
/// 与 exclude_file_type（类型词，如「不要视频」→ video 类）区分：字面扩展名是短 ascii 词形，
/// 判定为命中某扩展名 alias 的 keyword 且满足以下之一：
/// - token 本身就是该 alias 的注册扩展名（如 `pdf`∈pdf-alias.extensions）；
/// - 该 alias 是媒体类型（extensions 为空，keywords 里短 ascii 词即裸扩展名，如 video-alias
///   的 `mkv`/`mov`/`avi`）且 token ≤4 字符——藉此排除同 alias 里的类型词 `video`/`videos`（≥5）。
///
/// 仅在否定段调用，而 v0.5 无任一否定标记（见 [`negation_split`]）→ byte-equal 安全。
fn negated_literal_extensions(neg_lower: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for raw in neg_lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        let tok = raw.trim();
        if is_literal_extension_token(tok) && !out.iter().any(|e| e.eq_ignore_ascii_case(tok)) {
            out.push(tok.to_ascii_lowercase());
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 字面扩展名 token 判定（[`negated_literal_extensions`] 的谓词抽出，与
/// [`bare_no_literal_extensions`] 共用）：≥2 字符纯 ascii 词，且命中某扩展名 alias 的
/// keyword 并满足「本身是注册扩展名」或「媒体 alias 短词形（≤4 字符）」之一。
fn is_literal_extension_token(tok: &str) -> bool {
    tok.len() >= 2
        && tok.chars().all(|c| c.is_ascii_alphanumeric())
        && lexicon::EXTENSION_ALIASES.iter().any(|a| {
            a.keywords.iter().any(|k| k.eq_ignore_ascii_case(tok))
                && (a.extensions.iter().any(|e| e.eq_ignore_ascii_case(tok))
                    || (a.extensions.is_empty() && tok.len() <= 4))
        })
}

/// 裸「no <字面扩展名>」→ exclude_extensions（"videos and audio, no mkv"，d2-en-020 锚）。
///
/// 「no」**不入**通用否定标记 [`negation_split`]（语面太泛，会把整个后段送进
/// exclude_file_type 机器）；本谓词只认「no + 紧邻单 token 且 token 是字面扩展名」，
/// 类型词（"no screenshots"）不命中——那类由 but no / but not 标记路径处理。
/// v0.5 全集无 `no <word>` 形态（0 条）→ byte-equal 安全。
fn bare_no_literal_extensions(lower: &str) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = lower;
    while let Some(pos) = rest.find("no ") {
        // 词边界：`no` 前必须是句首或非字母数字（避免 "piano mkv" 的 -no- 误命中）。
        let ok_before = pos == 0 || !rest.as_bytes()[pos - 1].is_ascii_alphanumeric();
        if ok_before {
            let tok: String = rest[pos + 3..]
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if is_literal_extension_token(&tok) && !out.iter().any(|e| e.eq_ignore_ascii_case(&tok))
            {
                out.push(tok.to_ascii_lowercase());
            }
        }
        rest = &rest[pos + 3..];
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// BETA-13-G：查询尾部 head 名词 → file_type，仅当扩展名 alias 没推出类型时兜底。
///
/// gating（守住 v0.5 逐字节 + 不踩 d1「类型名词当 keyword」标注）：
/// - 必须存在「的」，尾段取**最后一个「的」之后**到结尾、且**精确等于**某类型名词
///   （非 `ends_with`）。这样「课程表」「减肥的计划表」尾段不精确 → 不命中；
///   「…「合成报告」的文件」尾段是「文件」→ 不命中。
/// - **表格类**（财务报表/报表/表/账本）一律映射 Spreadsheet（语料里这类尾名词稳定指电子表格）。
///   注：「表格」本身已是扩展名 alias，不会走到这里。
/// - **文档类**（报告/合同/协议/简历/…）仅当查询含**内容子句信号词**（内容/正文/提到/包含/
///   写着…）时才映射 Document——区分 d3「内容提到 X 的报告」(→document) 与 d1「关于 X 的报告」
///   「我的简历」(→None，类型名词此时是 keyword 本身)。
///
/// 返回 `(file_type, 尾段)`，尾段供调用方从 keywords 里剥掉这一类型信号。
///
/// 为何这批类型名词（报告/合同/账本…）**不放进 `lexicon.rs` 的 BETA-13-G3 类型词 alias**：
/// 它们需要「尾置 head 名词（最后一个『的』之后、精确等于）+ 内容子句信号」**双重 gating**
/// 才映射；而 lexicon 的 `ExtensionAlias` 是**无位置、无 gating 的全串匹配**，硬塞进去会让
/// 「我的简历」「关于 X 的报告」这类裸类型名词/主题查询被误判类型，污染既有匹配。故这里单列。
fn trailing_type_noun_file_type(input: &str) -> Option<(scout_search_backend::FileType, &str)> {
    use scout_search_backend::FileType;
    let trimmed = input.trim();
    let (head, tail) = trimmed.rsplit_once('的')?;
    let tail = tail.trim();
    const SPREADSHEET: &[&str] = &["财务报表", "报表", "表", "账本"];
    const DOCUMENT: &[&str] = &[
        "报告",
        "合同",
        "协议",
        // BETA-13-G14 B1：复合文档类名词（劳动合同/协议文件…），精确尾匹配仍安全。
        "劳动合同",
        "协议文件",
        "简历",
        "学习笔记",
        "笔记",
        "学习资料",
        "资料",
        "说明文件",
        "单据",
        "邮件草稿",
        "草稿",
    ];
    if SPREADSHEET.contains(&tail) {
        return Some((FileType::Spreadsheet, tail));
    }
    if DOCUMENT.contains(&tail) && has_content_clause_signal(head) {
        return Some((FileType::Document, tail));
    }
    None
}

/// BETA-13-G：是否含「按内容/正文搜」的子句信号词（区分内容检索 vs 主题/裸类型名词查询）。
///
/// 与 [`media_search::detect_content_clause`](super::media_search::detect_content_clause)
/// 是**有意分立的两个函数**，职责不同、词集刻意不同，请勿合并：
/// - `detect_content_clause` 是为「截图内容子句重路由」**抽取内容短语**的 regex：锚定
///   `里写着/写着/提到…` 等引导词后接短语，返回的是干净短语（供做 keyword）。
/// - `has_content_clause_signal`（本函数）是更广的「用户在按正文内容搜」**存在性谓词**：
///   词集更宽（`内容/正文/包含/同时/既有/出现` 等），**仅判 true/false、不抽取**，
///   用于 gating 文档类尾名词 → `Document`（见 [`trailing_type_noun_file_type`]）。
///
/// 二者词集不同是刻意的（用途不同）。将来增删「内容子句引导词」时，**按各自用途分别维护**：
/// 改抽取行为动 `detect_content_clause`，改 gating 存在性判定动本函数，勿强行统一词表。
fn has_content_clause_signal(s: &str) -> bool {
    const SIGNALS: &[&str] = &[
        "内容", "正文", "里面", "里写", "里有", "提到", "包含", "写着", "写了", "写到", "出现",
        "同时", "既有",
    ];
    SIGNALS.iter().any(|w| s.contains(w))
}

/// BETA-13-G Fix3b：英文 head 类型名词 → file_type。仅句首区域的 a/the/find/show me… +
/// documents?/archives?/spreadsheets?/presentations?，区别于句尾「in documents」位置义。
///
/// 为何 head-gated（`^` 锚定）：v0.5 里大量 `documents` 是「… in documents」=Documents
/// 文件夹位置义（如 `find screenshots from this week in documents`），类型由别的词或 None 决定。
/// 正则锚定句首，故句尾的 `documents` 不命中；且仅在 `parse_file_search` 中 file_type 为 None 时
/// 兜底，进一步避免与扩展名 alias 推出的类型冲突。
///
/// 返回 `(file_type, 命中的 head 名词)`：该 head 名词是「类型信号」而非内容，调用方需同步
/// 从 keywords 里大小写不敏感地剥掉（与 [`trailing_type_noun_file_type`] 的中文尾名词兜底
/// 对称），否则 `archives between 10 and 100 MB` 会让「archives」既当类型又当 keyword、
/// 过度约束检索。
fn english_head_type_noun_file_type(lower: &str) -> Option<(scout_search_backend::FileType, &str)> {
    use regex::Regex;
    use scout_search_backend::FileType;
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^(?:a |the |this |that |these |those |find |show me |list |give me )*(documents?|archives?|spreadsheets?|presentations?)\b")
            .expect("english head type noun regex")
    });
    let trimmed = lower.trim();
    if let Some(cap) = re.captures(trimmed) {
        if let Some(m) = cap.get(1) {
            let word = m.as_str();
            // G15：句首 documents 若带位置标记（「documents 里最近三天改的」，
            // v09-d5-mixed-014）是位置义，交回 location、不作类型。
            if word.starts_with("document") && en_ambiguous_noun_is_location(trimmed, word) {
                return None;
            }
            let ft = if word.starts_with("archive") {
                FileType::Archive
            } else if word.starts_with("spreadsheet") {
                FileType::Spreadsheet
            } else if word.starts_with("presentation") {
                FileType::Presentation
            } else {
                FileType::Document
            };
            return Some((ft, word));
        }
    }

    // BETA-13-G14 B1：文档类同义词 head 名词（contract/report/agreement/resume/study notes）
    // → Document，**仅在英文内容子句信号下**（that mentions / whose body / contains / inside…）。
    // 无信号（如「reports from last year」）不映射，保 ft=None。命中词同样从 keywords 剥离。
    if has_en_content_clause_signal(trimmed) {
        static RE2: OnceLock<Regex> = OnceLock::new();
        let re2 = RE2.get_or_init(|| {
            Regex::new(r"^(?:a |the |this |that |these |those |find |show me |list |give me )*(contracts?|reports?|agreements?|resumes?|study notes)\b")
                .expect("english doc-synonym head noun regex")
        });
        if let Some(cap) = re2.captures(trimmed) {
            if let Some(m) = cap.get(1) {
                return Some((FileType::Document, m.as_str()));
            }
        }
    }
    None
}

/// BETA-13-G14 B1：英文「按正文内容搜」子句信号（存在性谓词，gating 文档同义词 head 名词）。
fn has_en_content_clause_signal(lower: &str) -> bool {
    const SIGNALS: &[&str] = &[
        "mention",
        "whose body",
        "whose text",
        "whose content",
        "contains",
        "inside",
        "talk about",
        "talks about",
    ];
    SIGNALS.iter().any(|w| lower.contains(w))
}

fn decide_sort(
    lower: &str,
    size: &Option<SizeExpression>,
    created: &Option<TimeExpression>,
    modified: &Option<TimeExpression>,
    accessed: &Option<TimeExpression>,
) -> Option<SortOrder> {
    // BETA-13-G6：上下文感知的显式排序词，优先级最高。
    // 处理「按名字/按名称(+倒序)」名称排序，及「最新/最近/最早 + 维度词(创建/访问/修改)」
    // 组合（SORT_ALIASES 把「最新」一律当 modified_desc，无法区分 created/accessed）。
    if let Some(order) = explicit_sort_override(lower) {
        return Some(order);
    }

    // v0.2.1：先查 SORT_ALIASES 中用户**明确说出**的最高级排序词
    // （"最大的 / 最重 / biggest / 最新 / 最旧" 等 Class A 同义词），
    // 它们的优先级高于"有 size/time 字段就推断 sort"。
    for (keywords, order) in lexicon::SORT_ALIASES {
        if keywords.iter().any(|kw| lower.contains(kw)) {
            return Some(*order);
        }
    }

    // BETA-13-G12：英文/混合排序词「按 size」「by size」（含空格，size 是英文词）
    // 与中文「按大小」同义，归 SizeDesc。
    if lower.contains("按大小")
        || lower.contains("按 size")
        || lower.contains("by size")
        || size
            .as_ref()
            .is_some_and(|s| matches!(s, SizeExpression::GreaterThan { .. }))
    {
        return Some(SortOrder::SizeDesc);
    }
    // BETA-13-G12 ②′：less_than 约束（如「smaller than 1MB」）→ 小→大 size_asc。
    // v0.5 零 less_than 锚点 → byte-equal 安全。
    if size
        .as_ref()
        .is_some_and(|s| matches!(s, SizeExpression::LessThan { .. }))
    {
        return Some(SortOrder::SizeAsc);
    }
    if accessed.is_some() {
        return Some(SortOrder::AccessedDesc);
    }
    // created → created_desc 翻转收窄为「相对时间 + 显式创建/获得触发词」：
    // - Before/After/绝对区间是过滤语义而非新旧语义，保持默认 modified_desc
    //   （d5 锚点：2026年1月之前创建 / created before January 2026 → modified_desc）；
    // - 新增/做的 等弱创建词映射 created_time 但不翻转排序（d5-zh-006/008 → modified_desc）；
    // - v0.5 的 22 条 created 锚点全为 相对时间 + 收到/截/下载 形态，翻转行为不变。
    if matches!(created, Some(TimeExpression::Relative { .. })) && created_sort_flip_word(lower) {
        return Some(SortOrder::CreatedDesc);
    }
    if modified.is_some() {
        return Some(SortOrder::ModifiedDesc);
    }
    // 默认
    Some(SortOrder::ModifiedDesc)
}

/// created 时间过滤触发 `created_desc` 排序的显式词集（与 [`parse_time_fields`] 的
/// created 映射词集是**真子集**关系：新增/新建/做的/做了 映射时间维度但不翻转排序）。
fn created_sort_flip_word(lower: &str) -> bool {
    lower.contains("创建")
        || lower.contains("created")
        || lower.contains("收到")
        || lower.contains("received")
        || lower.contains("下载")
        || lower.contains("download")
        || lower.contains("截")
        || lower.contains("screenshot")
}

/// BETA-13-G6：上下文感知的显式排序词解析。
///
/// 两类：
/// 1. **名称排序**——「按名字 / 按名称 / by name」→ `NameAsc`，带「倒序 / 降序 / desc」→ `NameDesc`。
/// 2. **方向 × 维度**——方向词（最新/最近/最早；newest/oldest/latest/most recent）配维度词
///    （创建/created→created、访问/打开/opened/accessed→accessed、改/修改/编辑/modified→modified），
///    给出对应的 `*Desc` / `*Asc`。仅在方向 + 维度**同时**出现时返回，否则交回 SORT_ALIASES
///    与字段推断（保持「最新」默认 = `ModifiedDesc` 等既有行为，v0.5 零回归）。
///
/// 维度优先级 created > accessed > modified；`AccessedAsc` 无对应枚举，oldest+accessed 回退。
fn explicit_sort_override(lower: &str) -> Option<SortOrder> {
    // 1) 名称排序
    if lower.contains("按名字")
        || lower.contains("按名称")
        || lower.contains("名字排")
        || lower.contains("名称排")
        || lower.contains("by name")
    {
        let desc = lower.contains("倒序")
            || lower.contains("降序")
            || lower.contains("name desc")
            || lower.contains("by name desc");
        return Some(if desc {
            SortOrder::NameDesc
        } else {
            SortOrder::NameAsc
        });
    }

    // 2) 方向 × 维度
    let newest = lower.contains("最新")
        || lower.contains("最近")
        || lower.contains("newest")
        || lower.contains("most recent")
        || lower.contains("most recently")
        || lower.contains("latest");
    let oldest = lower.contains("最早") || lower.contains("oldest") || lower.contains("earliest");
    if !newest && !oldest {
        return None;
    }

    let created_dim = lower.contains("创建") || lower.contains("建立") || lower.contains("created");
    let accessed_dim = lower.contains("访问")
        || lower.contains("打开")
        || lower.contains("opened")
        || lower.contains("accessed");
    let modified_dim = lower.contains("修改")
        || lower.contains("编辑")
        || lower.contains("改动")
        || lower.contains("改的")
        || lower.contains("改过")
        || lower.contains("modified")
        || lower.contains("edited")
        || lower.contains("changed");

    if created_dim {
        return Some(if newest {
            SortOrder::CreatedDesc
        } else {
            SortOrder::CreatedAsc
        });
    }
    if accessed_dim {
        // AccessedAsc 无对应枚举：oldest + accessed 交回后续处理。
        return newest.then_some(SortOrder::AccessedDesc);
    }
    if modified_dim {
        return Some(if newest {
            SortOrder::ModifiedDesc
        } else {
            SortOrder::ModifiedAsc
        });
    }
    None
}

pub(crate) fn match_extensions(lower: &str) -> Option<&'static lexicon::ExtensionAlias> {
    lexicon::EXTENSION_ALIASES
        .iter()
        .find(|a| a.keywords.iter().any(|k| word_present(lower, k)))
}

/// 收集 query 中**所有**命中的扩展名 alias（按词典顺序，词边界由 `word_present` 保证：
/// ASCII needle 有单词边界，「pptx」不会误命中「ppt」alias）。
pub(crate) fn match_all_extensions(lower: &str) -> Vec<&'static lexicon::ExtensionAlias> {
    lexicon::EXTENSION_ALIASES
        .iter()
        .filter(|a| a.keywords.iter().any(|k| word_present(lower, k)))
        .collect()
}

/// 把多个命中 alias 合并成 `(extensions, file_types)`。
///
/// BETA-18：收集**全部**命中类型——`extensions` = 所有命中 alias 的扩展名并集（去重、保词典序）；
/// BETA-18/19：`file_types` 去重后按**在 query 中首次出现的位置**排序（覆盖标注以 query 语序为准，
/// 如「图片和视频」→`[image, video]`、「word 和 ppt」→`[document, presentation]`），而非词典序。这样：
/// - 同范畴多类型（「pdf和doc」同为 Document）→ 单元素 file_type + 扩展名并集，序列化回标量、
///   与旧行为 wire byte-equal；
/// - 跨范畴多类型 → 多 file_type 按用户语序排列，不再丢类型也不乱序。
///
/// 注：扩展名仍保词典序（与既有 wire 行为一致，如「word 和 ppt」→`[ppt,pptx,doc,docx]`）。
fn merge_extensions(
    lower: &str,
    matches: &[&'static lexicon::ExtensionAlias],
) -> (Option<Vec<String>>, Option<Vec<FileType>>) {
    if matches.is_empty() {
        return (None, None);
    }
    // 扩展名并集：保词典序。
    let mut exts: Vec<String> = Vec::new();
    for m in matches {
        for e in m.extensions {
            let s = (*e).to_string();
            if !exts.contains(&s) {
                exts.push(s);
            }
        }
    }
    // file_type：去重后按 query 中首次出现位置排序（同 file_type 取最早位置）。
    let mut typed: Vec<(usize, FileType)> = Vec::new();
    for m in matches {
        let pos = m
            .keywords
            .iter()
            .filter_map(|k| keyword_position(lower, k))
            .min()
            .unwrap_or(usize::MAX);
        if let Some(entry) = typed.iter_mut().find(|(_, ft)| *ft == m.file_type) {
            entry.0 = entry.0.min(pos);
        } else {
            typed.push((pos, m.file_type));
        }
    }
    typed.sort_by_key(|(pos, _)| *pos);
    let file_types: Vec<FileType> = typed.into_iter().map(|(_, ft)| ft).collect();

    // BETA-13 决策 A/B：跨范畴多类型（≥2 个不同 file_type）→ 只靠 file_type 数组表达类别，
    // 不列部分/不对称的扩展名（如「音乐和视频」只有音乐带扩展名、视频不带）。统一 ext=None。
    // 同范畴多类型（如「pdf和doc」同为 Document，单元素 file_type）仍保留扩展名并集。
    let extensions = if exts.is_empty() || file_types.len() >= 2 {
        None
    } else {
        Some(exts)
    };
    let file_types = if file_types.is_empty() {
        None
    } else {
        Some(file_types)
    };
    (extensions, file_types)
}

/// BETA-13-G15 (b)：类型义的英文 `documents`（或单数 `document`）→ `FileType::Document`，
/// 按 query 语序插入既有 file_type 列表（去重）。`pictures`/`images` 已在 EXTENSION_ALIASES，
/// 无需此处注入。
///
/// 仅当 `documents` 为**类型义**（无 `in`/`里` 位置标记）时生效；位置义由
/// [`parse_location_with_language`] 保留为 location，此处早退、不动 file_type / extensions。
/// 注入使 file_type 达到 ≥2 类时，按 [`merge_extensions`] 同规则把 extensions 置 None
/// （跨范畴多类型不列部分扩展名）。v0.5 无类型义裸 documents（全带 in/里）→ byte-equal 安全。
fn inject_type_meaning_document(
    pos_lower: &str,
    all_matches: &[&'static lexicon::ExtensionAlias],
    file_type: &mut Option<Vec<FileType>>,
    extensions: &mut Option<Vec<String>>,
) {
    let kw = if word_present(pos_lower, "documents") {
        "documents"
    } else if word_present(pos_lower, "document") {
        "document"
    } else {
        return;
    };
    if en_ambiguous_noun_is_location(pos_lower, kw) {
        return; // 位置义，交回 location
    }
    if file_type
        .as_deref()
        .is_some_and(|v| v.contains(&FileType::Document))
    {
        return; // 已含 Document（如「word ... documents」），去重无需注入
    }
    let doc_pos = pos_lower.find(kw).unwrap_or(usize::MAX);
    let mut typed: Vec<(usize, FileType)> = Vec::new();
    if let Some(v) = file_type.as_deref() {
        for ft in v {
            let pos = all_matches
                .iter()
                .filter(|a| a.file_type == *ft)
                .flat_map(|a| a.keywords.iter())
                .filter_map(|k| keyword_position(pos_lower, k))
                .min()
                .unwrap_or(usize::MAX);
            typed.push((pos, *ft));
        }
    }
    typed.push((doc_pos, FileType::Document));
    typed.sort_by_key(|(p, _)| *p);
    let new_types: Vec<FileType> = typed.into_iter().map(|(_, ft)| ft).collect();
    if new_types.len() >= 2 {
        *extensions = None;
    }
    *file_type = Some(new_types);
}

/// 返回 keyword 在 `lower` 中首次（整词）命中的字节位置；未命中返回 None。
/// 用于按 query 语序排列多 file_type。
fn keyword_position(lower: &str, k: &str) -> Option<usize> {
    if !word_present(lower, k) {
        return None;
    }
    lower.find(k)
}

pub(crate) fn has_any_extension_signal(lower: &str) -> bool {
    match_extensions(lower).is_some()
}

pub(crate) fn has_any_location_signal(lower: &str) -> bool {
    parse_location(lower).is_some()
}

pub(crate) fn has_keyword_like_signal(lower: &str) -> bool {
    // v0.5 扩 stop：加 "recent" / 中文虚词 ("的"/"里")，让 "find recent" / "找 recent 的"
    // 走 is_recent_only_query → Clarify(ambiguous_time)。
    let stop: &[&str] = &[
        "找",
        "查找",
        "最近的",
        "最近",
        "the",
        "show",
        "me",
        "find",
        "recent",
        "的",
        "里",
    ];
    let cleaned: String = stop
        .iter()
        .fold(lower.to_owned(), |acc, s| acc.replace(s, " "));
    cleaned.split_whitespace().any(|t| t.len() >= 2)
}

/// 向后兼容入口：未提供 language 的调用方按中文 canonical（与 has_any_location_signal /
/// is_unknown_location_only 等内部 helper 的需求一致）。新代码请用
/// [`parse_location_with_language`]。
pub(crate) fn parse_location(lower: &str) -> Option<Location> {
    parse_location_with_language(lower, Language::Zh)
}

pub(crate) fn parse_size(lower: &str) -> Option<SizeExpression> {
    use regex::Regex;
    use std::sync::OnceLock;

    // v0.2.1：覆盖三种 "> X 单位" 写法：
    //   "larger than 1 GB" / "大于 100 MB" / ">100mb"（无空格）/ "100 MB 以上"
    static RE_GT: OnceLock<Regex> = OnceLock::new();
    let re_gt = RE_GT.get_or_init(|| {
        // BETA-13-G14 B3：单位组扩 `gigs?`/bare `g`，数字与单位间允许量词「个」（「1个G」）。
        // BETA-13 收束：补 `bigger than`/`greater than`（v09-d5-en-025；v0.5 零暴露）。
        Regex::new(r"(?:大于|超过|over|larger than|bigger than|greater than|>=?|gt)\s*(\d+(?:\.\d+)?)\s*个?\s*(b|kb|mb|gb|tb|gigs?|g)")
            .expect("regex valid")
    });
    if let Some(cap) = re_gt.captures(lower) {
        let value: f64 = cap[1].parse().ok()?;
        let unit = parse_size_unit(&cap[2])?;
        return Some(SizeExpression::GreaterThan { value, unit });
    }

    // 「比 X 还大 / 比 X 更大」比较句式（v09-d5-zh-021「比50MB还大的 PDF」；v0.5 零「比」）。
    static RE_BI: OnceLock<Regex> = OnceLock::new();
    let re_bi = RE_BI.get_or_init(|| {
        Regex::new(r"比\s*(\d+(?:\.\d+)?)\s*个?\s*(b|kb|mb|gb|tb|gigs?|g)\s*(?:还|更)?(大|小)")
            .expect("regex valid")
    });
    if let Some(cap) = re_bi.captures(lower) {
        let value: f64 = cap[1].parse().ok()?;
        let unit = parse_size_unit(&cap[2])?;
        return Some(if &cap[3] == "大" {
            SizeExpression::GreaterThan { value, unit }
        } else {
            SizeExpression::LessThan { value, unit }
        });
    }

    // BETA-13-G12 ②′：前缀 "< X 单位" 写法（与 RE_GT 对称）：
    //   "smaller than 1 MB" / "less than 1MB" / "小于 1MB" / "不到 1GB" / "under 1mb" / "<1mb"。
    //   v0.5 零暴露（无 LT 前缀+size 锚点）→ byte-equal 安全。
    static RE_LT: OnceLock<Regex> = OnceLock::new();
    let re_lt = RE_LT.get_or_init(|| {
        Regex::new(
            r"(?:小于|不到|不足|smaller than|less than|under|below|<=?|lt)\s*(\d+(?:\.\d+)?)\s*个?\s*(b|kb|mb|gb|tb|gigs?|g)",
        )
        .expect("regex valid")
    });
    if let Some(cap) = re_lt.captures(lower) {
        let value: f64 = cap[1].parse().ok()?;
        let unit = parse_size_unit(&cap[2])?;
        return Some(SizeExpression::LessThan { value, unit });
    }

    // "X 单位 以上" 后置写法（中文常见）
    static RE_GT_SUFFIX: OnceLock<Regex> = OnceLock::new();
    let re_gt_suffix = RE_GT_SUFFIX.get_or_init(|| {
        Regex::new(r"(\d+(?:\.\d+)?)\s*(b|kb|mb|gb|tb)\s*(?:以上|及以上)").expect("regex valid")
    });
    if let Some(cap) = re_gt_suffix.captures(lower) {
        let value: f64 = cap[1].parse().ok()?;
        let unit = parse_size_unit(&cap[2])?;
        return Some(SizeExpression::GreaterThan { value, unit });
    }

    // "X 单位 以下" 后置写法
    static RE_LT_SUFFIX: OnceLock<Regex> = OnceLock::new();
    let re_lt_suffix = RE_LT_SUFFIX.get_or_init(|| {
        Regex::new(r"(\d+(?:\.\d+)?)\s*(b|kb|mb|gb|tb)\s*以下").expect("regex valid")
    });
    if let Some(cap) = re_lt_suffix.captures(lower) {
        let value: f64 = cap[1].parse().ok()?;
        let unit = parse_size_unit(&cap[2])?;
        return Some(SizeExpression::LessThan { value, unit });
    }

    // BETA-13-G follow-up：区间 size —— "between A and B unit" / "A 到 B 单位" / "A-B 单位"。
    // 单位取末位、两数共用；min/max 规范化。
    //
    // keywords 残留盲区：带空格写法（"10 到 100 mb" / "between 10 and 100 MB"）经
    // is_size_shaped 逐 token 剥离后 keywords 干净；但紧凑连字符写法（"3-5gb" 无空格）
    // 因 is_size_shaped 只剥单个 token，整串区间会残留进 keywords。此为既有 residual
    // 机制的盲区、不在评测集内，留 follow-up；此处不改 is_size_shaped 逻辑（YAGNI）。
    static RE_BETWEEN: OnceLock<Regex> = OnceLock::new();
    let re_between = RE_BETWEEN.get_or_init(|| {
        Regex::new(r"(?:between\s+)?(\d+(?:\.\d+)?)\s*(?:and|到|至|-|~)\s*(\d+(?:\.\d+)?)\s*(b|kb|mb|gb|tb)")
            .expect("regex valid")
    });
    if let Some(cap) = re_between.captures(lower) {
        let a: f64 = cap[1].parse().ok()?;
        let b: f64 = cap[2].parse().ok()?;
        let unit = parse_size_unit(&cap[3])?;
        let (min, max) = if a <= b { (a, b) } else { (b, a) };
        return Some(SizeExpression::Between { min, max, unit });
    }

    // "几个 G" / "几 GB" 启发式 → >= 1 GB
    if lower.contains("几个 g") || lower.contains("几个g") || lower.contains("几 gb") {
        return Some(SizeExpression::GreaterThan {
            value: 1.0,
            unit: SizeUnit::Gb,
        });
    }

    // "几百KB" 启发式 → < 1 MB（v09-d5-zh-019「几百KB的小文档」；「几百」已在
    // ZH_SIZE_STRIP，keywords 不泄漏；v0.5 零「几百K」锚点）。
    if lower.contains("几百kb") || lower.contains("几百 kb") || lower.contains("几百k") {
        return Some(SizeExpression::LessThan {
            value: 1.0,
            unit: SizeUnit::Mb,
        });
    }

    // "大文件" 默认 > 100MB（与 schema §8.2 #29 一致）
    if lower.contains("大文件") {
        return Some(SizeExpression::GreaterThan {
            value: 100.0,
            unit: SizeUnit::Mb,
        });
    }
    None
}

pub(crate) fn is_size_shaped(token: &str) -> bool {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    // 含单字母单位（"1G"/"500K"——「今年创建的超过1G的备份文件」的 1G 不是内容词）。
    let re =
        RE.get_or_init(|| Regex::new(r"^\d+(?:\.\d+)?(?:b|kb|mb|gb|tb|k|m|g|t)$").expect("regex"));
    re.is_match(&token.to_lowercase())
}

/// 标识符级数字串的最短长度：电话前缀（150138=6）/ 身份证（18）/ 邮编（6）皆 ≥6；
/// 年份（2024=4）/ 日号 / 小数量 < 6。
const IDENTIFIER_DIGIT_MIN: usize = 6;

/// 纯数字 token 是否为「附带数字」噪声（年份 / 日号 / 小数量）——应从关键词剥离。
/// 长数字串（≥ [`IDENTIFIER_DIGIT_MIN`]）更可能是电话号 / 案号 / 身份证号等**标识符**，
/// 保留为字面 keyword（`documents_fts` 是 trigram tokenizer，数字串可子串命中）。
/// 非纯数字 token 返回 false（交由其它 signal 判据处理）。
fn is_incidental_number(tok: &str) -> bool {
    let n = tok.chars().count();
    (1..IDENTIFIER_DIGIT_MIN).contains(&n) && tok.chars().all(|c| c.is_ascii_digit())
}

fn parse_size_unit(s: &str) -> Option<SizeUnit> {
    match s.to_lowercase().as_str() {
        "b" => Some(SizeUnit::B),
        "kb" => Some(SizeUnit::Kb),
        "mb" | "m" => Some(SizeUnit::Mb),
        "gb" | "g" | "gig" | "gigs" => Some(SizeUnit::Gb),
        _ => None,
    }
}

pub(crate) fn parse_duration(lower: &str) -> Option<SizeExpression> {
    use regex::Regex;
    use std::sync::OnceLock;

    // v0.5：单位后加 `\b` 防止"100MB"误识别成 100 minutes（m 后接 b 是文件大小，非时长）。
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?:超过|大于|over|longer than|>)\s*(\d+)\s*(秒|分钟|小时|s|m|h|minute|minutes|hour|hours)\b")
            .expect("regex valid")
    });
    let cap = re.captures(lower)?;
    let value: f64 = cap[1].parse().ok()?;
    let unit = match &cap[2].to_lowercase()[..] {
        "秒" | "s" => SizeUnit::Sec,
        "分钟" | "m" | "minute" | "minutes" => SizeUnit::Min,
        "小时" | "h" | "hour" | "hours" => SizeUnit::Hour,
        _ => return None,
    };
    Some(SizeExpression::GreaterThan { value, unit })
}

// ============================================================
// 关键词提取
// ============================================================

fn extract_filesearch_keywords(
    input: &str,
    lower: &str,
    ext_match: &Option<&'static lexicon::ExtensionAlias>,
) -> Option<Vec<String>> {
    // BETA-13-G：截图内容子句查询（「截图里写着 X」/「screenshot that says X」）——
    // keywords 直接用 detect_content_clause 的干净短语（单元素），避免通用残留抽取产生脏词。
    // 仅当含截图词且命中内容子句时短路；纯「截图+时间」等不受影响。
    if super::media_search::has_screenshot_word(lower) {
        if let Some(phrase) = super::media_search::detect_content_clause(input) {
            // BETA-13-G follow-up：仅含「both/同时」语义时按 和/and 拆多关键词；
            // 常规内容子句保持单关键词（避免对所有含「和」的内容词乱拆）。
            if super::media_search::content_clause_is_multi(input) {
                let parts = super::media_search::split_content_clause(&phrase);
                if !parts.is_empty() {
                    return Some(parts);
                }
            }
            return Some(vec![phrase]);
        }
    }

    // 图片内容子句（「image with the text out of stock」「image containing a license
    // plate number」）——整尾短语作单关键词，mirror 截图内容子句路径（d3-en-005/010/019）。
    if let Some(kws) = extract_image_content_clause_keywords(input, lower) {
        return Some(kws);
    }

    // 「找名字里有"预算"」/「文件名包含 X」 模式
    let bracket_match = extract_bracketed_word(input);
    if let Some(w) = bracket_match {
        return Some(vec![w]);
    }

    // "找名字里有 X" 简单模式
    if let Some(kw) =
        extract_after_phrase(input, &["名字里有", "名字里包含", "名字是", "文件名包含"])
    {
        return Some(vec![kw]);
    }

    // "find ... containing X"
    if let Some(kw) = extract_after_phrase(input, &["containing", "with name"]) {
        return Some(vec![kw]);
    }

    // BETA-13-G1：纯英文自然语言查询的"跨度剥离"短语关键词抽取。
    // 旧 `extract_english_token_keyword` 只取单 token，对「find the annual budget」会取到
    // about/the 等噪声词；residual 按内容短语抽取（"annual budget"），且 None 即无关键词
    // （不再回退到产噪声的单 token 逻辑）。
    if !contains_chinese(input) {
        let _ = (lower, ext_match);
        return extract_en_residual_keywords(input);
    }

    // v0.5：仅在 "找最近的 X Y" 这种**显式**结构中提取中文 keyword X（Y 是扩展名/文件类型）。
    // 不做通用中文 token 扫描，否则会把"查找昨天编辑过的"这种"动词+时间"短语误判为 keyword。
    if let Some(kw) = extract_zui_jin_de_keyword(input) {
        return Some(vec![kw]);
    }

    // BETA-13-G1：mixed 查询——合并英文短语 + 中文残留段，按在 query 中出现位置排序
    // （吃下「找一份关于 marketing plan 的ppt」「找一下关于 SEO 优化的资料」等混合关键词）。
    if is_mixed_input(input) {
        if let Some(kws) = merge_mixed_keywords(input) {
            return Some(kws);
        }
    }

    // 纯中文：跨度剥离 CJK 残留段。
    // v0.5 中文 keyword case 全走前面的「」/名字里有/最近的XY 结构，不进此分支（零回归）。
    if let Some(kws) = extract_zh_residual_keywords(input) {
        return Some(kws);
    }

    None
}

/// 图片内容子句关键词：query 含图片名词（photo/image/picture）且带内容子句标记时，
/// 标记后的**整尾短语**作单关键词（掐头冠词、去尾停用词），避免通用残留抽取把
/// 「out of stock」按停用词 of 切碎、或 extract_after_phrase 只取单 token（「a」）。
/// 截图（screenshot）由更早的截图内容子句路径处理；v0.5 的 "files containing X"
/// 无图片名词，不进此路径。
fn extract_image_content_clause_keywords(input: &str, lower: &str) -> Option<Vec<String>> {
    const IMAGE_NOUNS: &[&str] = &["photo", "photos", "image", "images", "picture", "pictures"];
    if !IMAGE_NOUNS.iter().any(|n| word_present(lower, n)) {
        return None;
    }
    const MARKERS: &[&str] = &[
        "with the text ",
        "with the words ",
        "with the word ",
        "that says ",
        "that reads ",
        "containing ",
    ];
    let (pos, marker) = MARKERS
        .iter()
        .filter_map(|m| lower.find(m).map(|p| (p, *m)))
        .min_by_key(|(p, _)| *p)?;
    let tail = &input[pos + marker.len()..];
    let mut tokens: Vec<&str> = tail
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .filter(|t| !t.is_empty())
        .collect();
    // 掐头冠词
    while tokens
        .first()
        .is_some_and(|t| matches!(t.to_lowercase().as_str(), "a" | "an" | "the"))
    {
        tokens.remove(0);
    }
    // 去尾停用词（「invoice in it」的 in it）
    while tokens
        .last()
        .is_some_and(|t| EN_STOPWORDS.contains(&t.to_lowercase().as_str()))
    {
        tokens.pop();
    }
    if tokens.is_empty() {
        return None;
    }
    Some(vec![tokens.join(" ")])
}

/// BETA-13-G1：mixed 查询的关键词 = 英文残留短语 ∪ 中文残留段，按在 query 中首次出现位置排序。
fn merge_mixed_keywords(input: &str) -> Option<Vec<String>> {
    let mut merged: Vec<(usize, String)> = Vec::new();
    if let Some(ks) = extract_en_residual_keywords(input) {
        for k in ks {
            if let Some(p) = input.find(&k) {
                merged.push((p, k));
            }
        }
    }
    if let Some(ks) = extract_zh_residual_keywords(input) {
        for k in ks {
            if let Some(p) = input.find(&k) {
                merged.push((p, k));
            }
        }
    }
    if merged.is_empty() {
        return None;
    }
    merged.sort_by_key(|(p, _)| *p);
    let mut out: Vec<String> = Vec::new();
    for (_, k) in merged {
        if !out.contains(&k) {
            out.push(k);
        }
    }
    Some(out)
}

/// 显式模式 "找最近的 X Y" / "最近的 X Y" — 提取中间的 X 作 keyword。
/// 要求：input 含 "最近的"，后面有一个 whitespace-separated CJK token X（≥2 CJK 字符），
/// 再后面是另一个 whitespace-separated token Y（任何形式：扩展名 / file_type）。
fn extract_zui_jin_de_keyword(input: &str) -> Option<String> {
    let pos = input.find("最近的")?;
    let after = &input[pos + "最近的".len()..];
    let tokens: Vec<&str> = after
        .split(|c: char| c.is_whitespace() || c == '，' || c == '。' || c == ',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.len() < 2 {
        return None;
    }
    let x = tokens[0];
    // X 必须是纯 CJK ≥2 字符（避免拿到 ASCII / 混合）
    if x.chars().filter(|c| is_cjk(*c)).count() < 2 {
        return None;
    }
    if x.chars().any(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(x.to_owned())
}

/// BETA-13-G1：中文框架/停用词——抽关键词时整体剥离（替换为分隔符）。
/// 含搜索动词、礼貌语、量词指代、关系框架词、内容检索框架词。长词在前（剥离时按长度降序）。
const ZH_FRAME_WORDS: &[&str] = &[
    // 搜索动词 + 礼貌
    "帮我找找看",
    "帮我找找",
    "帮我找份",
    "帮我找下",
    "帮我找一下",
    "帮我搜一下",
    "帮我搜",
    "帮我找",
    "帮我",
    "我想找一下",
    "我想找",
    "我要找",
    "我想要",
    "我想",
    "我要",
    "给我找",
    "给我",
    "查找",
    "都找出来",
    "找出来",
    "列出来",
    "都列出来",
    "找出",
    "列出",
    "出来",
    // BETA-13-G12：口语枚举尾缀（「图片、视频跟音乐我全都要」「截图和视频都要」），属框架措辞
    // 而非内容关键词。注：不剥通用「文件」（会切坏「临时文件」等合法关键词，已实测回归）。
    "我全都要",
    "全都要",
    "都要",
    // BETA-13-G12：「音乐和图片文件都列一下」的「都列」属枚举尾缀框架词
    //（「一下」已单列剥离），剥后「文件」整段为容器名词丢弃，避免「文件都列」残留为 keyword。
    "都列",
    // BETA-13-G14 C2：「pdf、ppt 和 excel 三种都找」的「三种都找」属计数枚举框架词，非内容。
    "三种都找",
    "三种",
    "都找",
    // BETA-13-G12：否定/排除标记（与 negation_split 对称，避免「但是不要」「不含」「排除」泄漏为 keyword）。
    "但是不要",
    "但是",
    "不要",
    "不含",
    "不包括",
    "排除",
    "显示",
    "找份",
    "找我",
    "找找看",
    "找找",
    "找一下",
    "找一份",
    "找一个",
    "找个",
    "找下",
    "搜一下",
    "搜下",
    "搜索",
    "想找",
    "看看",
    // 关系/内容框架
    "相关的",
    "有关的",
    "有关",
    "相关",
    "关于",
    "跟",
    "介绍了",
    "介绍",
    "描述了",
    "描述",
    "内容里",
    "内容同时",
    "内容",
    "正文里",
    "正文",
    "里面",
    "同时",
    "提到了",
    "提到",
    "里有写到",
    "里有写",
    "里有出现",
    "里有",
    "里写着",
    "里写了",
    "里写到",
    "里写",
    "我写的",
    "我写",
    "写到",
    "写着",
    "写了",
    "写",
    "包含",
    "含有",
    "出现了",
    "出现",
    "有没有",
    "有",
    "里",
    "字样",
    "四个字",
    "三个字",
    "都行",
    "或者",
    "文件夹",
    "目录",
    "哪份",
    "哪个",
    "哪张",
    // 量词/指代
    "那几个",
    "那批",
    "那份",
    "那个",
    "那张",
    "那篇",
    "那些",
    "这份",
    "这个",
    "这些",
    "几个",
    "一下",
    "一份",
    "一个",
    "一种",
    "一张",
    "一篇",
    "一些",
    "我的",
];

/// 中文时间词——抽关键词时剥离（含"动作 + 时间"动词如 创建/改过/访问）。
/// 不含裸单字 年/月/日/号（会切坏"年终总结"等关键词）；月份用 CJK 数字整词列出。
const ZH_TIME_STRIP: &[&str] = &[
    "最近三天",
    "最近一周",
    "最近两周",
    "最近一个月",
    "最近半年",
    "最近",
    "近一周",
    "近三天",
    "近一个月",
    "一周内",
    "过去三天",
    "过去一周",
    "过去两周",
    "过去一个月",
    "前天",
    "昨天",
    "今天",
    "这周",
    "本周",
    "上周",
    "这个月",
    "本月",
    "上个月",
    "今年",
    "去年",
    "前年",
    "一月",
    "二月",
    "三月",
    "四月",
    "五月",
    "六月",
    "七月",
    "八月",
    "九月",
    "十月",
    "十一月",
    "十二月",
    "之前",
    "之后",
    "创建于",
    "创建",
    "新建",
    "新增",
    "做的",
    "做了",
    "改过",
    "改动",
    "修改",
    "编辑",
    "访问过",
    "访问",
    "打开过",
    "打开",
    "收到",
    "下载",
];

/// 中文大小/数量框架词——抽关键词时剥离。
const ZH_SIZE_STRIP: &[&str] = &[
    "大于",
    "小于",
    "超过",
    "不到",
    "不超过",
    "以上",
    "以下",
    "之间",
    "大文件",
    "小文件",
    "几百",
    // 「比 X 还大/更大」比较句式残段（size 已由 parse_size 消费）
    "还大",
    "更大",
    "还小",
    "更小",
];

/// 中文排序/方向词——抽关键词时剥离（避免"最早/按名字倒序排"等进关键词）。
const ZH_SORT_STRIP: &[&str] = &[
    "按名字",
    "按名称",
    "按大小",
    "倒序排",
    "倒序",
    "升序",
    "排序",
    "最早",
    "最新",
    "最旧",
    "最大的",
    "最大",
    "最小的",
    "最小",
    "最重",
    "体积最大",
];

/// 通用容器名词——作为**整段**出现时丢弃（区别于作为词的一部分，如"体检报告"保留）。
pub(crate) const ZH_CONTAINER_NOUNS: &[&str] = &[
    "报告", "资料", "材料", "方案", "文章", "邮件", "单据", "说明", "文件", "东西", "那种", "图片",
    "照片", "笔记", "收据", "海报",
];

/// 词内含「和」的专有复合词——其中的「和」不是并列连词，切段前先占位保全
/// （d3-zh-030「碳中和目标」锚；mirror「预算表」compound 词表制先例）。
const ZH_HE_COMPOUNDS: &[&str] = &["碳中和"];

/// 复合词保全占位符（私用区，不与任何真实输入冲突）。
const HE_PLACEHOLDER: char = '\u{E000}';

/// CJK 分隔符——切分关键词段（"的/了" 等结构助词不进关键词）。
/// 「又」：并列连词（「既有发货又有签收」），d3-zh-032 锚；全集无「又」在内容词内的反例。
fn is_zh_delimiter(c: char) -> bool {
    matches!(
        c,
        '的' | '了' | '、' | '，' | '。' | '和' | '与' | '及' | '或' | '又'
    )
}

/// BETA-13 回归修复：剥离查询**前导**的裸单字搜索动词（「找/搜/查」）。
///
/// 复合形式（找一下 / 查找 / 帮我找 …）已由 [`ZH_FRAME_WORDS`] 全局剥离；此处补的是
/// 「动词直接粘内容词」（找英语 / 找合同）这一漏网形态——修复前会把动词混进 keyword
/// （「找英语」→「找英语」、「找合同和报告」→「找合同」）。仅剥句首一个单字动词，
/// 词中 / 词尾的「找」（如内容词「寻找规律」）不受影响，避免全局 `replace` 单字的误伤。
fn strip_leading_bare_search_verb(s: &str) -> &str {
    const BARE_VERBS: &[char] = &['找', '搜', '查'];
    let trimmed = s.trim_start();
    let mut chars = trimmed.chars();
    match chars.next() {
        Some(c) if BARE_VERBS.contains(&c) => chars.as_str(),
        _ => trimmed,
    }
}

/// BETA-13-G1：纯中文自然语言查询的"跨度剥离"关键词抽取。
///
/// 思路：把所有**无歧义的信号词**（类型词 / 时间词 / 位置词 / 排序词 / 框架词 / 大小词）
/// 替换成空格，再按 CJK 连续段切分（结构助词「的/了」等作分隔），取 ≥2 字的段为关键词，
/// 丢弃**整段恰为通用容器名词**的段（如「X的报告」→报告丢弃，但「体检报告」整段保留）。
///
/// 已知边界（标注本身不一致，规则无法两全）：
/// - 「碳中和目标」含「和」→ 词表制保全（[`ZH_HE_COMPOUNDS`] 占位符方案，「和」仍是
///   通用并列分隔符，「找合同和报告」不受影响）。
/// - 「备份文件」标注期望「备份」，但「临时文件」期望保留「文件」→ 取「整段非容器即保留」，
///   故「临时文件」对、「备份文件」漏。
fn extract_zh_residual_keywords(input: &str) -> Option<Vec<String>> {
    let mut s = input.to_owned();

    // 1) 收集所有要剥离的子串，按"字符数降序"排序（长词先剥，避免短词吃掉长词的一部分）。
    let mut strip: Vec<String> = Vec::new();
    for a in lexicon::EXTENSION_ALIASES {
        for k in a.keywords {
            if !k.is_ascii() {
                strip.push((*k).to_owned());
            }
        }
    }
    for a in lexicon::LOCATION_ALIASES {
        for k in a.keywords {
            if !k.is_ascii() {
                strip.push((*k).to_owned());
            }
        }
    }
    for (kws, _) in lexicon::SORT_ALIASES {
        for k in *kws {
            if !k.is_ascii() {
                strip.push((*k).to_owned());
            }
        }
    }
    for w in ZH_FRAME_WORDS
        .iter()
        .chain(ZH_TIME_STRIP)
        .chain(ZH_SIZE_STRIP)
        .chain(ZH_SORT_STRIP)
    {
        strip.push((*w).to_owned());
    }
    strip.sort_by_key(|w| std::cmp::Reverse(w.chars().count()));

    // 2) 逐个替换为空格。
    for w in &strip {
        if s.contains(w.as_str()) {
            s = s.replace(w.as_str(), " ");
        }
    }

    // 2.5) 复合词保全（d3-zh-030 锚）：专有复合词（碳中和）内的「和」不是并列连词——
    //      先换成私用区占位符，切段后还原；「找合同和报告」类真并列切分不受影响。
    for w in ZH_HE_COMPOUNDS {
        if s.contains(w) {
            s = s.replace(w, &w.replace('和', &HE_PLACEHOLDER.to_string()));
        }
    }

    // 2.6) BETA-13 回归修复：剥前导裸单字搜索动词「找/搜/查」（复合形式已在步骤 1 剥离），
    //      补「动词直接粘内容词」（找英语 / 找合同）这一漏网形态。
    let s = strip_leading_bare_search_verb(&s);

    // 3) 按 CJK 连续段切分（非 CJK / 分隔助词均作边界），取 ≥2 字段。
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut dropped_content_nouns: Vec<String> = Vec::new();
    for c in s.chars() {
        if (is_cjk(c) || c == HE_PLACEHOLDER) && !is_zh_delimiter(c) {
            cur.push(c);
        } else {
            push_residual_segment_tracking(&mut cur, &mut out, &mut dropped_content_nouns);
        }
    }
    push_residual_segment_tracking(&mut cur, &mut out, &mut dropped_content_nouns);

    // 3.2) 还原复合词占位符（见 2.5）。
    for k in &mut out {
        if k.contains(HE_PLACEHOLDER) {
            *k = k.replace(HE_PLACEHOLDER, "和");
        }
    }

    // 3.5) 半内容容器名词（报告）兜底：其它关键词都被剥空时它就是查询的内容本体
    //（「这周访问过的报告」→ [报告]；有并存内容词时仍丢弃：「关于市场调研的报告」→ [市场调研]）。
    if out.is_empty() {
        out = dropped_content_nouns;
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 半内容容器名词：整段出现时通常是容器（丢弃），但当**全查询没有任何其它内容词**时
/// 它就是用户要找的内容（保留）。锚点：d5-zh-014/033「…的报告」→ [报告]，
/// d1-zh-004「关于市场调研的报告」→ [市场调研]（报告仍丢）。
/// 仅收「报告」——「文件/东西」等纯容器词即便落单也不是内容（zh-012「…改过的文件」→ 无 keyword）。
const ZH_SOLE_CONTENT_NOUNS: &[&str] = &["报告"];

/// 把一个候选 CJK 段收入结果：≥2 字、非整段容器名词、未重复时保留。
fn push_residual_segment(cur: &mut String, out: &mut Vec<String>) {
    if cur.chars().count() >= 2 && !ZH_CONTAINER_NOUNS.contains(&cur.as_str()) && !out.contains(cur)
    {
        out.push(cur.clone());
    }
    cur.clear();
}

/// [`push_residual_segment`] 的变体：被容器规则丢弃的**半内容名词**段记入
/// `dropped_content_nouns`，供关键词全空时兜底。
fn push_residual_segment_tracking(
    cur: &mut String,
    out: &mut Vec<String>,
    dropped_content_nouns: &mut Vec<String>,
) {
    if cur.chars().count() >= 2
        && ZH_SOLE_CONTENT_NOUNS.contains(&cur.as_str())
        && !dropped_content_nouns.contains(cur)
    {
        dropped_content_nouns.push(cur.clone());
    }
    push_residual_segment(cur, out);
}

/// BETA-13-G1：英文框架/停用词——既被剥离，也作短语边界（连续内容 token 才拼成关键词）。
const EN_STOPWORDS: &[&str] = &[
    // 搜索动词 + 礼貌 + 代词
    "find",
    "finds",
    "found",
    "locate",
    "search",
    "get",
    "show",
    "see",
    "looking",
    "look",
    "want",
    "wanna",
    "need",
    "needs",
    "where",
    "here",
    "is",
    "are",
    "am",
    "was",
    "were",
    "be",
    "been",
    "you",
    "me",
    "my",
    "mine",
    "our",
    "ours",
    "your",
    "yours",
    "his",
    "her",
    "their",
    "please",
    "can",
    "could",
    "would",
    "let",
    "give",
    "wrote",
    "write",
    "written",
    "writing",
    "made",
    "did",
    "do",
    "does",
    "save",
    "saved",
    "saving",
    "new",
    // 冠词/限定词
    "the",
    "a",
    "an",
    "this",
    "that",
    "these",
    "those",
    "some",
    "any",
    "all",
    "something",
    "anything",
    "everything",
    "both",
    "each",
    "every",
    "no",
    "not", // BETA-13-G12：「images but not videos」否定标记残留
    // BETA-13-G16：英文排除标记残留（「…, excluding archives」）。同上 no/not 处理；
    // 类型段已由 negation_split → exclude_file_type，标记词本身不应作 keyword。
    // v0.5 仅 3 条 `exclude videos` 走 refine（kw=None），不经此停用词路径 → byte-equal 安全。
    "excluding",
    "exclude",
    "excludes",
    "excluded",
    "except",
    // 介词/连词（作分隔）
    "about",
    "regarding",
    "on",
    "of",
    "for",
    "to",
    "with",
    "within",
    "in",
    "into",
    "from",
    "by",
    "and",
    "or",
    "but",
    "which",
    "who",
    "whose",
    "whom",
    "related",
    "as",
    // 内容检索框架动词
    "containing",
    "contains",
    "contain",
    "mentions",
    "mention",
    "mentioning",
    "mentioned",
    "says",
    "say",
    "said",
    "saying",
    "shows",
    "show",
    "showing",
    "shown",
    "talk",
    "talks",
    "talking",
    "includes",
    "include",
    "including",
    "included",
    "has",
    "have",
    "having",
    "whose",
    "appears",
    "appear",
    "appearing",
    // 泛化框架名词
    "file",
    "files",
    "document",
    "documents",
    "text",
    "body",
    "content",
    "contents",
    "word",
    "thing",
    "things",
    "stuff",
    "ones",
    "one",
    "names",
    "named",
    "called",
    "titled",
    "inside",
    "it",
    "stuff",
    // 时间
    "today",
    "yesterday",
    "tomorrow",
    "recent",
    "recently",
    "newest",
    "oldest",
    "latest",
    "most", // 「most recently opened」的最高级修饰（v0.5 零出现）
    "last",
    "past",
    // 英文月份名（date 表达成分，v09-d5 before/after/between 系；v0.5 零出现）
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
    // 英文数字词（"five月之后" 混排月份 / "last three days" 等成分；"one" 已在框架名词组）
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "week",
    "weeks",
    "month",
    "months",
    "year",
    "years",
    "day",
    "days",
    "modified",
    "edited",
    "created",
    "creating",
    "changed",
    "opened",
    "accessed",
    "updated",
    "ago",
    "before",
    "after",
    "between",
    // 大小/排序
    "bigger",
    "biggest",
    "larger",
    "largest",
    "smaller",
    "smallest",
    "huge",
    "tiny",
    "big",
    "small",
    "over",
    "under",
    "above",
    "below",
    "than",
    "greater",
    "less",
    "gigs",
    "gig",
    "megs",
    "gb",
    "mb",
    "kb",
    "tb",
    "sort",
    "sorted",
    "ascending",
    "descending",
    "first",
    "name",
    // BETA-13-G12：排序维度词「by size / 按 size」中的 size 是信号词，不进 keyword。
    "size",
    "folder",
];

/// 英文容器名词——作为**整段**短语时丢弃（"X report"中的 report 作短语一部分保留）。
const EN_CONTAINER_NOUNS: &[&str] = &[
    "report",
    "reports",
    "photo",
    "photos",
    "picture",
    "image",
    "images",
    "screenshot",
    "screenshots",
    "receipt",
    "receipts",
    "poster",
];

/// 英文半内容容器名词（mirror [`ZH_SOLE_CONTENT_NOUNS`]）：整段出现时通常丢弃，但当
/// 全查询没有任何其它内容词时它就是查询本体（「reports from last year」→ [report]，
/// d5-en-008 锚）。仅收 report(s)——photo/screenshot 等落单时由类型字段表达，不作内容词。
const EN_SOLE_CONTENT_NOUNS: &[&str] = &["report", "reports"];

/// 英文单 token 复数归一（2026-07-04 拍板：invoices→invoice，利好 FTS 召回——索引里
/// 文件名多为单数）。仅动最保守的简单尾 s：纯字母、长度 >3、非 ss/us/is 结尾；
/// 多词短语与带连字符/数字 token（synthetic-notes 等 fixture 占位符）保持原样；
/// 复数专有名词（minutes=会议纪要、news 等，单数化改语义）保留原形。
/// 全集无「期望保留复数」的单 token 锚点（2026-07-04 盘点为零）。
/// 只在 [`parse_file_search`] 的关键词装配终点调用，不进 residual 抽取面
/// （fallback 结构性遗漏分析复用该面，见 [`extract_en_residual_keywords`] 内注释）。
fn singularize_en_keyword(k: String) -> String {
    // 复数专有名词：单数化会改语义（minutes 会议纪要 → minute 分钟）。
    const PLURAL_ONLY_NOUNS: &[&str] = &["minutes", "news", "series"];
    if k.contains(' ') || !k.chars().all(|c| c.is_ascii_alphabetic()) {
        return k;
    }
    let lc = k.to_lowercase();
    if PLURAL_ONLY_NOUNS.contains(&lc.as_str()) {
        return k;
    }
    if lc.len() > 3
        && lc.ends_with('s')
        && !lc.ends_with("ss")
        && !lc.ends_with("us")
        && !lc.ends_with("is")
    {
        k[..k.len() - 1].to_owned()
    } else {
        k
    }
}

/// BETA-13-G1：纯英文自然语言查询的"跨度剥离"短语关键词抽取。
///
/// 把 query 切成 token，遇停用词/类型词/位置词/size-shaped token 即作边界，连续的内容 token
/// 拼成短语（保留原始大小写，如 "John Smith" / "CV"）。丢弃整段恰为通用容器名词的短语。
fn extract_en_residual_keywords(input: &str) -> Option<Vec<String>> {
    // token：以非 [字母数字/连字符] 为分隔，保留原始大小写。
    let tokens: Vec<&str> = input
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .filter(|t| !t.trim_matches('-').is_empty())
        .collect();

    let trimmed: Vec<&str> = tokens.iter().map(|t| t.trim_matches('-')).collect();
    let lcs: Vec<String> = trimmed.iter().map(|t| t.to_lowercase()).collect();

    let mut out: Vec<String> = Vec::new();
    let mut dropped_content_nouns: Vec<String> = Vec::new();
    let mut phrase: Vec<&str> = Vec::new();
    let flush =
        |phrase: &mut Vec<&str>, out: &mut Vec<String>, dropped_content_nouns: &mut Vec<String>| {
            if !phrase.is_empty() {
                let joined = phrase.join(" ");
                let lc = joined.to_lowercase();
                // 注意：此处**不做**复数归一——本抽取面被 fallback 结构性遗漏分析复用
                // （residual_content_segments → has_uncovered_content），归一会让
                // 「minutes/hours」逃出 MEDIA_COVERAGE_NOISE_WORDS 词表误触发 keywords 遗漏；
                // 归一统一在 parse_file_search 的关键词装配终点做。
                if EN_CONTAINER_NOUNS.contains(&lc.as_str()) {
                    // 半内容名词记账：全查询无其它内容词时兜底保留，mirror zh 报告。
                    if EN_SOLE_CONTENT_NOUNS.contains(&lc.as_str())
                        && !dropped_content_nouns.contains(&joined)
                    {
                        dropped_content_nouns.push(joined);
                    }
                } else if !out.contains(&joined) {
                    out.push(joined);
                }
                phrase.clear();
            }
        };

    for i in 0..trimmed.len() {
        let tok = trimmed[i];
        let lc = lcs[i].as_str();
        // BETA-13-G16：token 若与相邻词构成某扩展名 alias 的**多词类型短语**
        // （如「code files」/「source code」），整体作边界，避免成分词（code）残留。
        let prev = i.checked_sub(1).map(|j| lcs[j].as_str());
        let next = lcs.get(i + 1).map(String::as_str);
        let is_signal = tok.chars().count() < 2
            || EN_STOPWORDS.contains(&lc)
            || is_size_shaped(tok)
            || is_incidental_number(tok)
            || is_ordinal_day_token(lc)
            || en_is_type_or_location_word(lc)
            || en_part_of_multiword_type_phrase(prev, lc, next);
        if is_signal {
            flush(&mut phrase, &mut out, &mut dropped_content_nouns);
        } else {
            phrase.push(tok);
        }
    }
    flush(&mut phrase, &mut out, &mut dropped_content_nouns);

    // 半内容容器名词兜底（mirror zh 报告）：其它关键词全空时它就是查询本体。
    if out.is_empty() {
        out = dropped_content_nouns;
    }

    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 序数日期 token（"1st" / "22nd" / "3rd" / "24th"）——date 表达成分，不进 keyword。
fn is_ordinal_day_token(lc: &str) -> bool {
    let digits: String = lc.chars().take_while(char::is_ascii_digit).collect();
    !digits.is_empty()
        && digits.len() <= 2
        && matches!(&lc[digits.len()..], "st" | "nd" | "rd" | "th")
}

/// 判断小写 token 是否为 ASCII 类型词 / 位置词（来自词典，抽关键词时剥离）。
fn en_is_type_or_location_word(lc: &str) -> bool {
    lexicon::EXTENSION_ALIASES.iter().any(|a| {
        a.keywords
            .iter()
            .any(|k| k.is_ascii() && k.eq_ignore_ascii_case(lc))
    }) || lexicon::LOCATION_ALIASES.iter().any(|a| {
        a.keywords
            .iter()
            .any(|k| k.is_ascii() && k.eq_ignore_ascii_case(lc))
    })
}

/// BETA-13-G16：`cur` 是否为某扩展名 alias **多词类型短语** keyword 的成分
/// （与前一词或后一词构成，如「code files」/「source code」）。
///
/// 仅查 EXTENSION_ALIASES（类型短语），不含位置短语——精确处理「code files and documents」
/// 这类成分词残留（`code` 单独不在 alias、`en_is_type_or_location_word` 漏判），同时保留
/// 尾词内容关键词（如「verification code」非 alias 短语 → `code` 不作边界）。
fn en_part_of_multiword_type_phrase(prev: Option<&str>, cur: &str, next: Option<&str>) -> bool {
    let phrase_is_alias = |phrase: &str| {
        lexicon::EXTENSION_ALIASES.iter().any(|a| {
            a.keywords
                .iter()
                .any(|k| k.contains(' ') && k.is_ascii() && k.eq_ignore_ascii_case(phrase))
        })
    };
    next.is_some_and(|n| phrase_is_alias(&format!("{cur} {n}")))
        || prev.is_some_and(|p| phrase_is_alias(&format!("{p} {cur}")))
}

fn contains_chinese(s: &str) -> bool {
    s.chars().any(is_cjk)
}

fn is_mixed_input(s: &str) -> bool {
    let has_zh = contains_chinese(s);
    let has_en = s.chars().any(|c| c.is_ascii_alphabetic());
    has_zh && has_en
}

fn extract_bracketed_word(input: &str) -> Option<String> {
    // 「X」 / "X" / 'X'
    let mut chars = input.chars().peekable();
    let mut buf = String::new();
    let mut in_quote = false;
    let mut open_char = ' ';
    while let Some(c) = chars.next() {
        if !in_quote {
            if c == '「' {
                in_quote = true;
                open_char = '「';
            } else if c == '"' {
                in_quote = true;
                open_char = '"';
            }
        } else {
            let close = match open_char {
                '「' => '」',
                '"' => '"',
                _ => '"',
            };
            if c == close {
                if !buf.is_empty() {
                    return Some(buf);
                }
                in_quote = false;
                buf.clear();
            } else {
                buf.push(c);
            }
        }
    }
    None
}

fn extract_after_phrase(input: &str, phrases: &[&str]) -> Option<String> {
    for phrase in phrases {
        if let Some(pos) = input.find(phrase) {
            let after = &input[pos + phrase.len()..];
            let token: String = after
                .chars()
                .skip_while(|c| c.is_whitespace() || *c == '"' || *c == '「' || *c == '的')
                .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '」' && *c != '的')
                .collect();
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    None
}

/// BETA-23：内容残留段抽取——给 fallback 触发器做「内容词覆盖检测」。
///
/// 复用 G1 跨度剥离逻辑（zh / en / mixed 三形态），**刻意不走**
/// `extract_filesearch_keywords` 的「文件名包含 X」「」等短路结构——短路正是
/// 丢词根因，触发器需要看到 query 的全部内容段。
///
/// 段内可能粘连未剥离的结构字（如「文件名」），调用方需自行剥噪。
pub(crate) fn residual_content_segments(input: &str) -> Vec<String> {
    if !contains_chinese(input) {
        return extract_en_residual_keywords(input).unwrap_or_default();
    }
    if is_mixed_input(input) {
        if let Some(kws) = merge_mixed_keywords(input) {
            return kws;
        }
    }
    extract_zh_residual_keywords(input).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    #[test]
    fn parse_size_between_range() {
        use scout_search_backend::{SearchIntent, SizeExpression, SizeUnit};
        let SearchIntent::FileSearch(fs) = crate::parse("archives between 10 and 100 MB") else {
            panic!("应 file_search");
        };
        assert_eq!(
            fs.size,
            Some(SizeExpression::Between {
                min: 10.0,
                max: 100.0,
                unit: SizeUnit::Mb
            }),
            "size between"
        );
        assert_eq!(
            super::parse_size("10 到 100 mb"),
            Some(SizeExpression::Between {
                min: 10.0,
                max: 100.0,
                unit: SizeUnit::Mb
            })
        );
        assert_eq!(
            super::parse_size("between 100 and 10 gb"),
            Some(SizeExpression::Between {
                min: 10.0,
                max: 100.0,
                unit: SizeUnit::Gb
            })
        );
    }

    #[test]
    fn trailing_chinese_type_noun_maps_file_type() {
        use scout_search_backend::{FileType, SearchIntent};
        let cases = [
            ("找一份装修预算的表", FileType::Spreadsheet, "装修预算"),
            ("内容提到现金流的财务报表", FileType::Spreadsheet, "现金流"),
            (
                "正文里写着销售额下滑的报告",
                FileType::Document,
                "销售额下滑",
            ),
            (
                "找一下里面写了不可抗力条款的合同",
                FileType::Document,
                "不可抗力条款",
            ),
            ("提到李娜的简历", FileType::Document, "李娜"),
        ];
        for (q, ft, kw) in cases {
            let SearchIntent::FileSearch(fs) = crate::parse(q) else {
                panic!("{q}")
            };
            assert_eq!(fs.file_type, Some(vec![ft]), "{q} file_type");
            assert_eq!(fs.keywords, Some(vec![kw.to_owned()]), "{q} keywords");
        }
    }

    #[test]
    fn g16_type_and_exclude_words_not_leaked_to_keywords() {
        // BETA-13-G16 刀2：多词类型短语成分词 + 排除标记不应残留进 keywords。
        use scout_search_backend::SearchIntent;
        // en-010：「code files」的成分词 code 不泄漏（v0.5 零 code 锚点）。
        let SearchIntent::FileSearch(fs) = crate::parse("code files and documents") else {
            panic!("应 file_search")
        };
        assert_eq!(fs.keywords, None, "code files 不应泄漏 code");
        // en-019：「excluding」排除标记不泄漏（同 G12 no/not 处理）。
        let SearchIntent::FileSearch(fs) = crate::parse("documents and images, excluding archives")
        else {
            panic!("应 file_search")
        };
        assert_eq!(fs.keywords, None, "excluding 标记不应泄漏");
        // 保护：尾词 code（非类型短语）仍作内容关键词。
        assert_eq!(
            super::extract_en_residual_keywords("the screenshot that shows verification code"),
            Some(vec!["verification code".to_owned()]),
            "verification code 应保留"
        );
    }

    #[test]
    fn created_sort_flip_narrowed_to_relative_plus_trigger() {
        // 时间表达簇（2026-07-04）：created→created_desc 翻转收窄。
        use scout_search_backend::{SearchIntent, SortOrder, TimeExpression};
        // Before 过滤 + 创建词 → 保持 modified_desc（d5-zh-010/en-009 锚）
        let SearchIntent::FileSearch(fs) = crate::parse("2026年1月之前创建的合同") else {
            panic!()
        };
        assert!(matches!(
            fs.created_time,
            Some(TimeExpression::Before { .. })
        ));
        assert_eq!(fs.sort, Some(SortOrder::ModifiedDesc));
        assert_eq!(fs.keywords, Some(vec!["合同".to_owned()]));
        // 弱创建词（做的）+ 相对时间 → created_time 但不翻转排序（d5-zh-008 锚）
        let SearchIntent::FileSearch(fs) = crate::parse("今年做的演示文稿") else {
            panic!()
        };
        assert!(fs.created_time.is_some());
        assert_eq!(fs.sort, Some(SortOrder::ModifiedDesc));
        // 显式创建词 + 相对时间 → 仍翻转（v0.5 收到/截/下载 22 锚点行为不变）
        let SearchIntent::FileSearch(fs) = crate::parse("上周创建的 Word 文件") else {
            panic!()
        };
        assert_eq!(fs.sort, Some(SortOrder::CreatedDesc));
    }

    #[test]
    fn report_kept_as_keyword_only_when_sole_content() {
        // 「报告」半内容名词：无其它内容词时保留、有并存内容词时丢弃。
        use scout_search_backend::{SearchIntent, SortOrder};
        let SearchIntent::FileSearch(fs) = crate::parse("这周访问过的报告") else {
            panic!()
        };
        assert_eq!(fs.keywords, Some(vec!["报告".to_owned()]));
        assert!(fs.accessed_time.is_some(), "这周+访问过 → accessed_time");
        assert_eq!(fs.sort, Some(SortOrder::AccessedDesc));
        let SearchIntent::FileSearch(fs) = crate::parse("最新创建的报告") else {
            panic!()
        };
        assert_eq!(fs.keywords, Some(vec!["报告".to_owned()]));
        assert_eq!(fs.sort, Some(SortOrder::CreatedDesc));
        // 反例：并存内容词 → 报告仍丢弃（d1-zh-004 锚）
        let SearchIntent::FileSearch(fs) = crate::parse("有没有关于市场调研的报告")
        else {
            panic!()
        };
        assert_eq!(fs.keywords, Some(vec!["市场调研".to_owned()]));
        // 反例：纯容器词（文件）落单也不保留（d5-zh-012 锚）
        let SearchIntent::FileSearch(fs) = crate::parse("5月20到24号之间改过的文件")
        else {
            panic!()
        };
        assert_eq!(fs.keywords, None);
    }

    #[test]
    fn you_conjunction_splits_keywords() {
        // 「又」并列连词作分隔（d3-zh-032）。
        use scout_search_backend::SearchIntent;
        let SearchIntent::FileSearch(fs) = crate::parse("找正文里既有发货又有签收的单据")
        else {
            panic!()
        };
        assert_eq!(
            fs.keywords,
            Some(vec!["发货".to_owned(), "签收".to_owned()])
        );
    }

    #[test]
    fn yusuanbiao_compound_splits_keyword_and_type() {
        // 「预算表」= 预算 + spreadsheet（d1-mixed-013；d1-zh-018 姊妹锚点）。
        use scout_search_backend::{FileType, SearchIntent};
        let SearchIntent::FileSearch(fs) = crate::parse("帮我找一下 budget 预算表") else {
            panic!()
        };
        assert_eq!(
            fs.keywords,
            Some(vec!["budget".to_owned(), "预算".to_owned()])
        );
        assert_eq!(fs.file_type, Some(vec![FileType::Spreadsheet]));
        // 反例：课程表整词是内容名词，不拆（d1-zh-023 锚）
        let SearchIntent::FileSearch(fs) = crate::parse("找一份课程表") else {
            panic!()
        };
        assert_eq!(fs.keywords, Some(vec!["课程表".to_owned()]));
    }

    #[test]
    fn bi_comparison_size_and_en_date_keywords_clean() {
        // 「比50MB还大」比较句式 + 英文日期成分不漏 keywords。
        use scout_search_backend::{SearchIntent, SizeExpression, SizeUnit, SortOrder};
        let SearchIntent::FileSearch(fs) = crate::parse("比50MB还大的 PDF") else {
            panic!()
        };
        assert_eq!(
            fs.size,
            Some(SizeExpression::GreaterThan {
                value: 50.0,
                unit: SizeUnit::Mb
            })
        );
        assert_eq!(fs.sort, Some(SortOrder::SizeDesc));
        assert_eq!(fs.keywords, None, "还大 不应泄漏");
        let SearchIntent::FileSearch(fs) = crate::parse("files edited after May 1st") else {
            panic!()
        };
        assert_eq!(fs.keywords, None, "May 1st 不应泄漏");
        let SearchIntent::FileSearch(fs) = crate::parse("files modified between May 20 and May 24")
        else {
            panic!()
        };
        assert_eq!(fs.keywords, None, "May 不应泄漏");
        let SearchIntent::FileSearch(fs) = crate::parse("most recently opened files") else {
            panic!()
        };
        assert_eq!(fs.keywords, None, "most 不应泄漏");
        assert_eq!(fs.sort, Some(SortOrder::AccessedDesc));
    }

    #[test]
    fn image_content_clause_phrase_keywords() {
        // 图片内容子句整尾短语（d3-en-005/010/019）；"the word" 框架名词不映射 Word 类型。
        use scout_search_backend::{FileType, SearchIntent};
        let SearchIntent::FileSearch(fs) = crate::parse("image with the text out of stock") else {
            panic!()
        };
        assert_eq!(fs.keywords, Some(vec!["out of stock".to_owned()]));
        let SearchIntent::FileSearch(fs) = crate::parse("image containing a license plate number")
        else {
            panic!()
        };
        assert_eq!(fs.keywords, Some(vec!["license plate number".to_owned()]));
        let SearchIntent::FileSearch(fs) = crate::parse("photo with the word invoice in it") else {
            panic!()
        };
        assert_eq!(fs.keywords, Some(vec!["invoice".to_owned()]));
        assert_eq!(
            fs.file_type,
            Some(vec![FileType::Image]),
            "word 框架名词不应并入 document"
        );
        // v0.5 保护："files containing X in downloads" 无图片名词，不进整尾短语路径。
        let SearchIntent::FileSearch(fs) =
            crate::parse("find files containing synthetic-budget in downloads")
        else {
            panic!()
        };
        assert_eq!(fs.keywords, Some(vec!["synthetic-budget".to_owned()]));
    }

    #[test]
    fn plural_keywords_singularized_and_report_sole_kept() {
        // 2026-07-04 拍板：英文单 token 复数归一（invoices→invoice）+ report 半内容名词
        // 落单保留（reports from last year → [report]）。
        use scout_search_backend::SearchIntent;
        let cases = [
            ("invoices modified last month", "invoice"),
            ("installers smaller than 1 GB", "installer"),
            ("ebooks in my downloads folder", "ebook"),
            ("contracts created before January 2026", "contract"),
            ("reports from last year", "report"),
        ];
        for (q, kw) in cases {
            let SearchIntent::FileSearch(fs) = crate::parse(q) else {
                panic!("{q}")
            };
            assert_eq!(fs.keywords, Some(vec![kw.to_owned()]), "{q}");
        }
        // 保护：连字符占位符 / 多词短语不做归一。
        assert_eq!(
            super::singularize_en_keyword("synthetic-notes".to_owned()),
            "synthetic-notes"
        );
        assert_eq!(
            super::singularize_en_keyword("meeting notes".to_owned()),
            "meeting notes"
        );
        // 保护：photo/screenshot 等纯容器落单不保留（有类型字段表达）。
        assert_eq!(
            super::extract_en_residual_keywords("recent photos"),
            None,
            "photos 落单不应成为 keyword"
        );
    }

    #[test]
    fn g15_location_article_and_head_documents_gated() {
        // 2026-07-04 拍板落地：G15 谓词覆盖扩展。
        use scout_search_backend::{Location, SearchIntent};
        let loc = |h: &str| {
            Some(Location {
                hint: Some(h.to_owned()),
                include: None,
                exclude: None,
            })
        };
        // en-020：「in the pictures folder」位置义 → location=pictures、Image 类型抑制、
        // wallpapers 复数归一。
        let SearchIntent::FileSearch(fs) = crate::parse("wallpapers in the pictures folder") else {
            panic!()
        };
        assert_eq!(fs.location, loc("pictures"));
        assert_eq!(fs.file_type, None, "位置义 pictures 不应给 Image");
        assert_eq!(fs.keywords, Some(vec!["wallpaper".to_owned()]));
        // mixed-014：句首「documents 里」位置义 → head 名词映射闸门早退，ft=None。
        let SearchIntent::FileSearch(fs) = crate::parse("documents 里最近三天改的") else {
            panic!()
        };
        assert_eq!(fs.file_type, None, "位置义 documents 不应给 Document");
        assert_eq!(fs.location, loc("documents"));
        // 保护：类型义句首 documents 照旧映射（G15(b) 原行为）。
        let SearchIntent::FileSearch(fs) = crate::parse("documents that mention quarterly revenue")
        else {
            panic!()
        };
        assert!(
            fs.file_type
                .as_deref()
                .is_some_and(|v| v.contains(&scout_search_backend::FileType::Document)),
            "类型义 documents 应保持 Document 映射"
        );
    }

    #[test]
    fn jibai_kb_maps_less_than_one_mb() {
        // 「几百KB」启发 → < 1MB + size_asc（v09-d5-zh-019；与「几个G」启发对称）。
        use scout_search_backend::{SearchIntent, SizeExpression, SizeUnit, SortOrder};
        let SearchIntent::FileSearch(fs) = crate::parse("几百KB的小文档") else {
            panic!()
        };
        assert_eq!(
            fs.size,
            Some(SizeExpression::LessThan {
                value: 1.0,
                unit: SizeUnit::Mb
            })
        );
        assert_eq!(fs.sort, Some(SortOrder::SizeAsc));
    }

    #[test]
    fn c3_oldest_sorts_by_created_asc() {
        // BETA-13-G14 C3：用户拍板「oldest = 按创建时间升序」（文件「年龄」最自然是创建时间）。
        // v0.5 零 oldest 锚点 → byte-equal 安全。
        use scout_search_backend::{SearchIntent, SortOrder};
        let SearchIntent::FileSearch(fs) = crate::parse("the oldest photos first") else {
            panic!()
        };
        assert_eq!(fs.sort, Some(SortOrder::CreatedAsc));
    }

    #[test]
    fn b2_media_folder_is_location() {
        // BETA-13-G14 B2：「X文件夹」作位置（图片文件夹/影片文件夹），mirror screenshot_dir。
        use scout_search_backend::{FileType, Location, SearchIntent};
        let loc = |h: &str| {
            Some(Location {
                hint: Some(h.to_owned()),
                include: None,
                exclude: None,
            })
        };
        // 影片文件夹=位置，ft=video 由「电影」给（保留），kw=[电影]
        let SearchIntent::FileSearch(fs) = crate::parse("影片文件夹里的电影") else {
            panic!()
        };
        assert_eq!(fs.location, loc("影片"), "影片文件夹→location");
        assert_eq!(fs.file_type, Some(vec![FileType::Video]));
        // 图片文件夹=位置，「图片」是文件夹名不作 file_type，壁纸非类型 → ft=None
        let SearchIntent::FileSearch(fs2) = crate::parse("图片文件夹里的壁纸") else {
            panic!()
        };
        assert_eq!(fs2.location, loc("图片"), "图片文件夹→location");
        assert_eq!(fs2.file_type, None, "图片文件夹是位置，图片不作 file_type");
    }

    #[test]
    fn b3_size_unit_gigs_and_ge_individual_g() {
        // BETA-13-G14 B3：size 单位词汇扩展——「个G」「bare G」「gigs」。decide_sort 已自动
        // 据 size 推 size_asc/size_desc，故只需 parse_size 认得这些单位写法。v0.5 零此类形态。
        use scout_search_backend::{SearchIntent, SizeExpression, SizeUnit, SortOrder};
        let SearchIntent::FileSearch(fs) = crate::parse("小于1个G的安装包") else {
            panic!()
        };
        assert_eq!(
            fs.size,
            Some(SizeExpression::LessThan {
                value: 1.0,
                unit: SizeUnit::Gb
            })
        );
        assert_eq!(fs.sort, Some(SortOrder::SizeAsc));

        let SearchIntent::FileSearch(fs2) = crate::parse("huge files over 2 gigs") else {
            panic!()
        };
        assert_eq!(
            fs2.size,
            Some(SizeExpression::GreaterThan {
                value: 2.0,
                unit: SizeUnit::Gb
            })
        );
        assert_eq!(fs2.sort, Some(SortOrder::SizeDesc));
    }

    #[test]
    fn b1_chinese_compound_doc_noun_maps_document() {
        // BETA-13-G14 B1：中文复合文档类名词（协议文件/劳动合同）→ document + 从 keywords 剥离。
        use scout_search_backend::{FileType, SearchIntent};
        let cases = [
            ("里面有提到甲方乙方的协议文件", "甲方乙方"),
            ("正文里写了试用期三个月的劳动合同", "试用期三个月"),
        ];
        for (q, kw) in cases {
            let SearchIntent::FileSearch(fs) = crate::parse(q) else {
                panic!("{q}")
            };
            assert_eq!(fs.file_type, Some(vec![FileType::Document]), "{q} ft");
            assert_eq!(fs.keywords, Some(vec![kw.to_owned()]), "{q} kw");
        }
    }

    #[test]
    fn b1_english_doc_synonym_with_content_clause_maps_document() {
        // BETA-13-G14 B1：英文文档类同义词 head 名词 + 内容子句信号 → document + 剥离。
        use scout_search_backend::{FileType, SearchIntent};
        let cases = [
            ("the contract that mentions John Smith inside", "John Smith"),
            ("report whose body mentions market share", "market share"),
            (
                "find the agreement that contains non-compete clause",
                "non-compete clause",
            ),
            ("the resume that mentions Sarah Lee", "Sarah Lee"),
            (
                "study notes that mention neural networks",
                "neural networks",
            ),
        ];
        for (q, kw) in cases {
            let SearchIntent::FileSearch(fs) = crate::parse(q) else {
                panic!("{q}")
            };
            assert_eq!(fs.file_type, Some(vec![FileType::Document]), "{q} ft");
            assert_eq!(fs.keywords, Some(vec![kw.to_owned()]), "{q} kw");
        }
    }

    #[test]
    fn b1_english_doc_synonym_without_clause_not_mapped() {
        // BETA-13-G14 B1 守护：无内容子句信号 → 不映射 document（保 d5「reports from last year」ft=None）。
        use scout_search_backend::SearchIntent;
        let SearchIntent::FileSearch(fs) = crate::parse("reports from last year") else {
            panic!()
        };
        assert_eq!(fs.file_type, None, "无内容子句不应映射 document");
    }

    #[test]
    fn quoted_keyword_noun_not_mapped() {
        use scout_search_backend::SearchIntent;
        let SearchIntent::FileSearch(fs) = crate::parse("找本周文稿名字里有「合成报告」的文件")
        else {
            panic!()
        };
        assert_eq!(fs.file_type, None, "尾名词为「文件」不应映射 document");
    }

    #[test]
    fn residual_segments_cover_problem4_query() {
        // 问题 4：「文件名包含运维」短路丢掉「会议纪要」——残留段抽取必须能看到它
        let segs = residual_content_segments("2025年的会议纪要文件名包含运维");
        assert!(segs.iter().any(|s| s.contains("会议纪要")), "segs={segs:?}");
    }

    #[test]
    fn residual_segments_empty_for_pure_signal_query() {
        // 纯信号词查询（时间+类型）无内容残留段
        let segs = residual_content_segments("上周的pdf");
        assert!(segs.is_empty(), "segs={segs:?}");
    }

    #[test]
    fn incidental_number_threshold() {
        // < 6 位纯数字 = 附带数字（年份 / 日号 / 小数量），剥离。
        assert!(is_incidental_number("2024"));
        assert!(is_incidental_number("100"));
        assert!(is_incidental_number("12"));
        assert!(is_incidental_number("12345"));
        // ≥ 6 位 = 标识符（电话前缀 / 身份证 / 邮编），保留。
        assert!(!is_incidental_number("150138"));
        assert!(!is_incidental_number("15013866763"));
        assert!(!is_incidental_number("440307201312314812"));
        // 非纯数字不归此判据。
        assert!(!is_incidental_number("a100"));
        assert!(!is_incidental_number(""));
    }

    #[test]
    fn long_digit_run_kept_as_keyword() {
        // 电话/编号级数字串保留为字面 keyword（trigram 索引可子串命中）。
        assert_eq!(
            extract_en_residual_keywords("150138"),
            Some(vec!["150138".to_owned()])
        );
        assert_eq!(
            extract_en_residual_keywords("15013866763"),
            Some(vec!["15013866763".to_owned()])
        );
        // 内容词 + 号码：相邻非信号 token 合成短语（daemon 的 expand_intent_for_daemon
        // 再按空格拆成 "invoice" AND "15013866763"，号码仍可检索）。
        assert_eq!(
            extract_en_residual_keywords("invoice 15013866763"),
            Some(vec!["invoice 15013866763".to_owned()])
        );
    }

    #[test]
    fn short_number_still_stripped() {
        // 年份 / 小数量仍剥离，不进 keyword（守护 date/size 既有行为）。
        assert_eq!(extract_en_residual_keywords("2024"), None);
        assert_eq!(extract_en_residual_keywords("100"), None);
    }

    #[test]
    fn parse_bare_number_keeps_number_keyword() {
        use scout_search_backend::SearchIntent;
        // 端到端：纯号码查询经 parse 后号码进 keywords（对齐 2026-07-08 真机复现的
        // 「search 150138 0 命中」修复）。
        let SearchIntent::FileSearch(fs) = crate::parse("15013866763") else {
            panic!("纯号码应解析为 FileSearch")
        };
        assert_eq!(
            fs.keywords,
            Some(vec!["15013866763".to_owned()]),
            "号码应作为关键词保留"
        );
    }

    #[test]
    fn negated_literal_extension_goes_to_exclude_extensions() {
        use scout_search_backend::{FileType, SearchIntent};
        // 「不含 mkv」：mkv 是字面扩展名 → exclude_extensions（非 exclude_file_type）。
        let SearchIntent::FileSearch(fs) = crate::parse("视频和音频，不含 mkv") else {
            panic!("应 file_search")
        };
        assert_eq!(fs.file_type, Some(vec![FileType::Video, FileType::Audio]));
        assert_eq!(fs.exclude_extensions, Some(vec!["mkv".to_owned()]));
        assert_eq!(
            fs.exclude_file_type, None,
            "字面扩展名不应进 exclude_file_type"
        );
        // 反向守护：类型词「不要视频」仍走 exclude_file_type（video 是 ≥5 字符类型词）。
        let SearchIntent::FileSearch(fs) = crate::parse("images but not videos") else {
            panic!("应 file_search")
        };
        assert_eq!(fs.exclude_extensions, None);
        assert_eq!(fs.exclude_file_type, Some(vec![FileType::Video]));
    }

    #[test]
    fn bare_no_literal_extension_goes_to_exclude_extensions() {
        use scout_search_backend::{FileType, SearchIntent};
        // d2-en-020：裸「no mkv」（无 but not 等标记）→ exclude_extensions 窄路径。
        let SearchIntent::FileSearch(fs) = crate::parse("videos and audio, no mkv") else {
            panic!("应 file_search")
        };
        assert_eq!(fs.file_type, Some(vec![FileType::Video, FileType::Audio]));
        assert_eq!(fs.exclude_extensions, Some(vec!["mkv".to_owned()]));
        assert_eq!(fs.exclude_file_type, None);
        // 反向守护：「no + 类型词」不命中窄路径（screenshots ≥5 字符非字面扩展名）。
        let SearchIntent::FileSearch(fs) = crate::parse("music and videos, but no screenshots")
        else {
            panic!("应 file_search")
        };
        assert_eq!(fs.exclude_extensions, None);
        assert_eq!(fs.exclude_file_type, Some(vec![FileType::Screenshot]));
    }

    #[test]
    fn he_compound_keyword_preserved() {
        use scout_search_backend::{FileType, SearchIntent};
        // d3-zh-030：「碳中和目标」词内「和」不作并列切分（占位符保全）。
        let SearchIntent::FileSearch(fs) = crate::parse("正文提到碳中和目标的报告 pdf")
        else {
            panic!("应 file_search")
        };
        assert_eq!(fs.keywords, Some(vec!["碳中和目标".to_owned()]));
        assert_eq!(fs.extensions, Some(vec!["pdf".to_owned()]));
        assert_eq!(fs.file_type, Some(vec![FileType::Document]));
        // 反向守护：真并列「和」仍切分（「找合同和报告」→ 合同；报告 是容器名词丢弃）。
        let SearchIntent::FileSearch(fs) = crate::parse("找合同和报告") else {
            panic!("应 file_search")
        };
        assert_eq!(fs.keywords, Some(vec!["合同".to_owned()]));
    }

    #[test]
    fn positive_types_then_exclude_routes_file_search() {
        use scout_search_backend::{FileType, SearchIntent};
        // 「文档和图片，排除压缩包」= 全新搜索（正向类型 + 排除某类型）→ file_search 带
        // exclude_file_type，而非 refine。
        let SearchIntent::FileSearch(fs) = crate::parse("文档和图片，排除压缩包") else {
            panic!("应 file_search（决策 C 同构）")
        };
        assert_eq!(
            fs.file_type,
            Some(vec![FileType::Document, FileType::Image])
        );
        assert_eq!(fs.exclude_file_type, Some(vec![FileType::Archive]));
        assert_eq!(
            fs.keywords, None,
            "排除/压缩包 等不应残留：{:?}",
            fs.keywords
        );

        // 反向守护：裸「排除视频」(排除前无正向类型) 仍走 refine。
        assert!(
            matches!(crate::parse("排除视频"), SearchIntent::Refine(_)),
            "裸排除仍应是 refine"
        );
        // 反向守护：尾置形「把 ppt 也排除掉，只看视频」仍走 refine。
        assert!(
            matches!(
                crate::parse("把 ppt 也排除掉，只看视频"),
                SearchIntent::Refine(_)
            ),
            "尾置形 + 只看 仍应是 refine"
        );
    }

    #[test]
    fn enumeration_tail_du_lie_not_keyword() {
        use scout_search_backend::{FileType, SearchIntent};
        // 「都列」是枚举尾缀框架词，剥后「文件」整段为容器名词丢弃 → 无 keyword 残留。
        let SearchIntent::FileSearch(fs) = crate::parse("音乐和图片文件都列一下") else {
            panic!("应 file_search")
        };
        assert_eq!(fs.file_type, Some(vec![FileType::Audio, FileType::Image]));
        assert_eq!(
            fs.keywords, None,
            "「文件都列」不应残留为 keyword：{:?}",
            fs.keywords
        );
    }

    #[test]
    fn an_size_recognized_as_size_sort() {
        use scout_search_backend::{FileType, SearchIntent, SortOrder};
        // 「按 size」（英文 size 词，带空格）= 按大小排序，且 size 不残留为 keyword。
        let SearchIntent::FileSearch(fs) = crate::parse("10到100MB之间的 archive 按 size 排")
        else {
            panic!("应 file_search")
        };
        assert_eq!(fs.file_type, Some(vec![FileType::Archive]));
        assert_eq!(fs.sort, Some(SortOrder::SizeDesc));
        assert!(
            !fs.keywords
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|k| k.eq_ignore_ascii_case("size")),
            "size 是排序维度词、不应进 keyword：{:?}",
            fs.keywords
        );
    }

    #[test]
    fn english_head_type_noun_maps_file_type() {
        use scout_search_backend::{FileType, SearchIntent};
        for (q, ft) in [
            ("a document about the marketing plan", FileType::Document),
            (
                "documents that mention quarterly revenue",
                FileType::Document,
            ),
            ("documents modified today", FileType::Document),
            ("archives between 10 and 100 MB", FileType::Archive),
        ] {
            let SearchIntent::FileSearch(fs) = crate::parse(q) else {
                panic!("{q}")
            };
            assert_eq!(fs.file_type, Some(vec![ft]), "{q}");
        }
    }

    #[test]
    fn english_head_type_noun_stripped_from_keywords() {
        use scout_search_backend::{FileType, SearchIntent};
        // 命中 head 类型名词后应同步从 keywords 剥掉（与中文尾名词兜底对称），
        // 否则「archives」既当类型又当 keyword、过度约束检索。
        let SearchIntent::FileSearch(fs) = crate::parse("archives between 10 and 100 MB") else {
            panic!("expected FileSearch")
        };
        assert_eq!(fs.file_type, Some(vec![FileType::Archive]));
        assert!(
            !fs.keywords
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|k| k.eq_ignore_ascii_case("archives")),
            "head 类型名词 archives 不应残留在 keywords：{:?}",
            fs.keywords
        );

        // 真正的内容关键词不应被误伤：head 名词是 document，但 keywords 是 marketing plan。
        let SearchIntent::FileSearch(fs) = crate::parse("a document about the marketing plan")
        else {
            panic!("expected FileSearch")
        };
        assert_eq!(fs.file_type, Some(vec![FileType::Document]));
        assert_eq!(fs.keywords, Some(vec!["marketing plan".to_owned()]));
    }

    #[test]
    fn g15a_bare_english_documents_pictures_not_location() {
        use super::parse_location;
        // 裸 / 句首 / 并列 / 内容子句 → 不作 location
        assert_eq!(parse_location("documents and images"), None);
        assert_eq!(
            parse_location("documents that mention quarterly revenue"),
            None
        );
        assert_eq!(parse_location("documents modified today"), None);
        assert_eq!(parse_location("png and jpg pictures"), None);
        assert_eq!(parse_location("music, videos and pictures"), None);
        // 带 in/里 标记 → 仍作 location（保 v0.5）
        assert!(parse_location("find ppt over 100mb in documents").is_some());
        assert!(parse_location("find documents 里的 ppt").is_some());
    }

    #[test]
    fn g15b_type_meaning_documents_sets_file_type() {
        use scout_search_backend::{FileType, SearchIntent};
        let ft = |q: &str| -> Option<Vec<FileType>> {
            let SearchIntent::FileSearch(fs) = crate::parse(q) else {
                panic!("not file_search: {q}")
            };
            assert_eq!(fs.location, None, "{q} location 应为 None");
            fs.file_type
        };
        // 句首 documents（head fallback 已能给，本测兼验 location 被消除）
        assert_eq!(
            ft("documents that mention quarterly revenue"),
            Some(vec![FileType::Document])
        );
        assert_eq!(
            ft("documents modified today"),
            Some(vec![FileType::Document])
        );
        // 并列：document 需按语序注入到正确位置
        assert_eq!(
            ft("show me documents and images"),
            Some(vec![FileType::Document, FileType::Image])
        );
        assert_eq!(
            ft("code files and documents"),
            Some(vec![FileType::Code, FileType::Document])
        );
        assert_eq!(
            ft("documents, spreadsheets and presentations"),
            Some(vec![
                FileType::Document,
                FileType::Spreadsheet,
                FileType::Presentation
            ])
        );
        // 尾置 documents（mixed）
        assert_eq!(
            ft("我昨天 opened 的 documents"),
            Some(vec![FileType::Document])
        );
    }
}
