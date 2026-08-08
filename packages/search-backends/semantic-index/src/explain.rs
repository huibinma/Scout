//! BETA-15B-5：语义命中段落高亮的纯逻辑（切句 + 排序 + 编排）。
//! 展示时按需算：不落库、不动索引。desktop 命令 `explain_semantic_hit` 调用。

use scout_indexer::embed::{meaningful_char_ratio, TextEmbedder, MEANINGFUL_CHAR_RATIO_FLOOR};
use scout_indexer::vectors::cosine;

/// 单个候选段落。`start`/`end` 为 body 的**字符**偏移（Unicode scalar，`end` 不含）。
#[derive(Debug, Clone, PartialEq)]
pub struct Passage {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// 每段目标字符数：到达即在下个句界/硬界收口（控制 embed 次数）。
const PASSAGE_TARGET_CHARS: usize = 280;
/// 段落数上限（body 已截断到 4000 字符，280×16≈4480 足够覆盖；兜底防极端长正文）。
const MAX_PASSAGES: usize = 16;
/// 高亮取相似度前 N 段。
const EXPLAIN_TOP_N: usize = 2;
/// 段落相似度下限：低于此不高亮（避免"硬凑"高亮误导）。
///
/// BETA-33 cycle 4（2026-07-01）：0.30 → 0.45。
/// **Why**：embeddinggemma-300m 对中文短文本 cosine baseline ≈ 0.40-0.50、0.30 阈值等于
/// "任何段落都能被展示"。用户 v0.9.4 搜「作文」踩到 face-3-efdc54.png QQ 表情包 OCR 乱码段
/// cosine 0.62 → 预览面板显示「强相关」严重误导（表格文档级仅 0.16）。上调到 0.45 让
/// "至少要显著高于 baseline"才展示；配合 [`passage_worth_embedding`] 段落级门槛（cycle 4
/// 新加）、乱码段直接跳过 embed，双重防线。
const EXPLAIN_MIN_SCORE: f32 = 0.45;

/// 段落级最小字符门槛（比 `MIN_EMBED_TEXT_CHARS=20` 宽松，因段自然比全文短）。
/// 段落经 `segment_passages` 切完常有真句 8-30 字（如"我有一只猫。"6 字），
/// 若沿用 20 字门槛会误挡真段。
const PASSAGE_MIN_CHARS: usize = 8;

/// 段落是否值得 embed：字数下限 [`PASSAGE_MIN_CHARS`] + 有意义字符占比下限 `min_ratio`。
///
/// 与 `is_embed_worthy` 分离：文档级用后者（20 字下限）、段落级用前者（8 字下限）。
/// `min_ratio` 由调用方按 doc_type 给：普通文档 [`MEANINGFUL_CHAR_RATIO_FLOOR`]（0.6）、
/// 图片 OCR（BETA-39 opt-in）用更严的 `IMAGE_MEANINGFUL_RATIO_FLOOR`（0.75）——
/// 防 v0.9.4「作文」段级 0.62 虚高在图片路径复现。
fn passage_worth_embedding(text: &str, min_ratio: f32) -> bool {
    let trimmed = text.trim();
    if trimmed.chars().count() < PASSAGE_MIN_CHARS {
        return false;
    }
    meaningful_char_ratio(trimmed) >= min_ratio
}

fn is_sentence_end(c: char) -> bool {
    // 只取 CJK 句末标点 + 换行 + 英文感叹/问号；英文句号易撞小数/缩写，
    // 不作句界，靠 `PASSAGE_TARGET_CHARS` 硬界兜住长英文段。
    matches!(c, '。' | '！' | '？' | '!' | '?' | '\n')
}

fn push_passage(out: &mut Vec<Passage>, start: usize, end: usize, buf: &str) {
    if !buf.trim().is_empty() {
        out.push(Passage {
            start,
            end,
            text: buf.to_owned(),
        });
    }
}

/// 把 body 切成有序、字符偏移连续的段落；跳过纯空白段；段数封顶 `MAX_PASSAGES`。
#[must_use]
pub fn segment_passages(body: &str) -> Vec<Passage> {
    let mut passages = Vec::new();
    let mut start = 0usize;
    let mut buf = String::new();
    let mut cur_len = 0usize;
    let mut idx = 0usize;
    for c in body.chars() {
        buf.push(c);
        idx += 1;
        cur_len += 1;
        if is_sentence_end(c) || cur_len >= PASSAGE_TARGET_CHARS {
            push_passage(&mut passages, start, idx, &buf);
            if passages.len() >= MAX_PASSAGES {
                return passages;
            }
            start = idx;
            buf.clear();
            cur_len = 0;
        }
    }
    push_passage(&mut passages, start, idx, &buf);
    passages
}

/// 过滤掉 < `min_score` 的段、按相似度降序、截断到 `top_n`，
/// 返回 `(start, end, score)`（字符偏移 + 真 cosine）。
fn rank_passages(
    passages: &[Passage],
    mut scored: Vec<(usize, f32)>,
    top_n: usize,
    min_score: f32,
) -> Vec<(usize, usize, f32)> {
    scored.retain(|(_, s)| *s >= min_score);
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(top_n);
    scored
        .into_iter()
        .map(|(i, s)| (passages[i].start, passages[i].end, s))
        .collect()
}

/// 展示时按需算：切句 → 各段 embed → 与 query 向量 cosine → 取前 N 高于下限的段。
/// 返回 `(start, end, score)`（字符偏移 + 真 cosine）。无段落 / embed 失败 → 空。
/// **不落库、不读原文件**：`body` 由调用方从已索引正文取得。
///
/// BETA-33 cycle 4：新加 [`passage_worth_embedding`] 段落级门槛——不合格段（<8 字或
/// 有意义字符占比 <60%）直接 skip、不参与 embed 或 scoring。段级字数下限 8（比文档级
/// 20 宽松）保留真短句"我有一只猫。"通过、同时挡住 OCR 乱码段。
#[must_use]
pub fn explain_passages(
    body: &str,
    query: &str,
    embedder: &dyn TextEmbedder,
) -> Vec<(usize, usize, f32)> {
    explain_passages_with_ratio(body, query, embedder, MEANINGFUL_CHAR_RATIO_FLOOR)
}

/// [`explain_passages`] 的带段级 ratio 门槛变体（BETA-39）：图片 OCR 正文（opt-in 开启后）
/// 传 `IMAGE_MEANINGFUL_RATIO_FLOOR`（0.75），其余调用与 `explain_passages` 等价。
#[must_use]
pub fn explain_passages_with_ratio(
    body: &str,
    query: &str,
    embedder: &dyn TextEmbedder,
    min_ratio: f32,
) -> Vec<(usize, usize, f32)> {
    let passages = segment_passages(body);
    if passages.is_empty() {
        return Vec::new();
    }
    let Ok(query_vec) = embedder.embed(query) else {
        return Vec::new();
    };
    let mut scored = Vec::with_capacity(passages.len());
    for (i, p) in passages.iter().enumerate() {
        // 乱码 / 极短段直接跳过：避免中文噪声段与 query 落在中文均值方向、
        // 硬凑出虚高 cosine（v0.9.4 用户搜「作文」踩到 QQ 表情包 OCR 段 0.62 即此 bug）。
        if !passage_worth_embedding(&p.text, min_ratio) {
            continue;
        }
        let Ok(v) = embedder.embed(&p.text) else {
            return Vec::new();
        };
        scored.push((i, cosine(&query_vec, &v)));
    }
    rank_passages(&passages, scored, EXPLAIN_TOP_N, EXPLAIN_MIN_SCORE)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// 含「猫」→ x 轴 [1,0]；含「狗」→ y 轴 [0,1]；否则 [0,0]。
    #[derive(Debug)]
    struct AxisEmbedder;
    impl TextEmbedder for AxisEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>, scout_indexer::IndexError> {
            Ok(vec![
                if text.contains('猫') { 1.0 } else { 0.0 },
                if text.contains('狗') { 1.0 } else { 0.0 },
            ])
        }
        fn model_id(&self) -> &'static str {
            "axis"
        }
    }

    #[derive(Debug)]
    struct FailEmbedder;
    impl TextEmbedder for FailEmbedder {
        fn embed(&self, _text: &str) -> Result<Vec<f32>, scout_indexer::IndexError> {
            Err(scout_indexer::IndexError::Tag {
                path: String::new(),
                detail: "no model".into(),
            })
        }
        fn model_id(&self) -> &'static str {
            "fail"
        }
    }

    #[test]
    fn segments_on_sentence_ends_with_char_offsets() {
        let p = segment_passages("猫在叫。狗在跑！");
        assert_eq!(p.len(), 2);
        assert_eq!((p[0].start, p[0].end), (0, 4)); // 猫在叫。
        assert_eq!(p[0].text, "猫在叫。");
        assert_eq!((p[1].start, p[1].end), (4, 8)); // 狗在跑！
    }

    #[test]
    fn empty_body_yields_no_passages() {
        assert!(segment_passages("").is_empty());
        assert!(segment_passages("   \n  ").is_empty());
    }

    #[test]
    fn long_unpunctuated_text_is_hard_split_by_target_len() {
        let body: String = "a".repeat(600);
        let p = segment_passages(&body);
        assert_eq!(p.len(), 3); // 280 + 280 + 40
        assert_eq!((p[0].start, p[0].end), (0, 280));
        assert_eq!((p[1].start, p[1].end), (280, 560));
        assert_eq!((p[2].start, p[2].end), (560, 600));
    }

    #[test]
    fn rank_filters_below_floor_sorts_desc_truncates() {
        let passages = vec![
            Passage {
                start: 0,
                end: 4,
                text: "a".into(),
            },
            Passage {
                start: 4,
                end: 8,
                text: "b".into(),
            },
            Passage {
                start: 8,
                end: 12,
                text: "c".into(),
            },
        ];
        let scored = vec![(0usize, 0.9f32), (1, 0.1), (2, 0.5)];
        let out = rank_passages(&passages, scored, 2, 0.30);
        assert_eq!(out, vec![(0, 4, 0.9), (8, 12, 0.5)]);
    }

    #[test]
    fn rank_all_below_floor_is_empty() {
        let passages = vec![Passage {
            start: 0,
            end: 4,
            text: "a".into(),
        }];
        let out = rank_passages(&passages, vec![(0usize, 0.05f32)], 2, 0.30);
        assert!(out.is_empty());
    }

    #[test]
    fn explain_highlights_semantically_matching_passage() {
        // 段级 8 字下限（cycle 4 新加）：段需 ≥8 字才 embed；单段扩到 14/9 字。
        let body = "我今天在家有一只很可爱的猫。外面有一条大黄狗。";
        let out = explain_passages(body, "猫", &AxisEmbedder);
        assert_eq!(out.len(), 1); // 狗段 cosine=0 < EXPLAIN_MIN_SCORE=0.45、只留猫段
        let (start, end, score) = out[0];
        assert_eq!((start, end), (0, 14)); // 「我今天在家有一只很可爱的猫。」14 字
        assert!((score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn explain_returns_empty_when_embedder_fails() {
        // 用 ≥8 字段确保能触发 FailEmbedder（否则先被门槛挡下也返空、测不到真失败路径）。
        let out = explain_passages("我有一只很可爱的黑猫。", "猫", &FailEmbedder);
        assert!(out.is_empty());
    }

    /// BETA-33 cycle 4：`passage_worth_embedding` 段级门槛的关键边界。
    #[test]
    fn passage_worth_embedding_gates_length_and_ratio() {
        // 8 字全 CJK：过
        assert!(passage_worth_embedding(
            "我今天写作业很好",
            MEANINGFUL_CHAR_RATIO_FLOOR
        ));
        // 7 字：不过
        assert!(!passage_worth_embedding(
            "我今天写作业",
            MEANINGFUL_CHAR_RATIO_FLOOR
        ));
        // 8 字但一半是数字：不过（meaningful ratio 50% < 60%）
        assert!(!passage_worth_embedding(
            "我1今2天3写4",
            MEANINGFUL_CHAR_RATIO_FLOOR
        ));
        // 全空白：不过
        assert!(!passage_worth_embedding(
            "        ",
            MEANINGFUL_CHAR_RATIO_FLOOR
        ));
        // 前后空白 + 8 字有效：过
        assert!(passage_worth_embedding(
            "  我今天写作业很好  ",
            MEANINGFUL_CHAR_RATIO_FLOOR
        ));
    }

    /// BETA-39：段级图片门槛（0.75）——ratio 落在 (0.6, 0.75) 的 CJK-heavy 乱码段被挡、
    /// 真文字段照常参与打分；默认门槛（0.6）下同一段仍能过（图片专属加严不外溢）。
    #[test]
    fn explain_with_image_ratio_blocks_cjk_heavy_noise_segment() {
        use scout_indexer::embed::IMAGE_MEANINGFUL_RATIO_FLOOR;
        // 含「猫」的 CJK-heavy 乱码段：8 汉字 + 4 数字 = ratio 0.67。
        let noisy = "猫动河的天写在有1234";
        assert!(
            passage_worth_embedding(noisy, MEANINGFUL_CHAR_RATIO_FLOOR),
            "0.67 过默认 0.6"
        );
        assert!(
            !passage_worth_embedding(noisy, IMAGE_MEANINGFUL_RATIO_FLOOR),
            "0.67 不过图片 0.75"
        );
        // 走 explain 全链路：图片门槛下该段被 skip、返空；默认门槛下命中。
        let out_img =
            explain_passages_with_ratio(noisy, "猫", &AxisEmbedder, IMAGE_MEANINGFUL_RATIO_FLOOR);
        assert!(out_img.is_empty(), "图片段级门槛应挡下 CJK-heavy 乱码段");
        let out_default = explain_passages(noisy, "猫", &AxisEmbedder);
        assert_eq!(out_default.len(), 1, "默认门槛下同段照常参与打分");
        // 真文字段在图片门槛下照常命中。
        let out_real = explain_passages_with_ratio(
            "我有一只很可爱的黑猫。",
            "猫",
            &AxisEmbedder,
            IMAGE_MEANINGFUL_RATIO_FLOOR,
        );
        assert_eq!(out_real.len(), 1, "真文字段过图片门槛、照常高亮");
    }

    /// BETA-33 cycle 4：明显噪声段（数字/符号大头）被 explain_passages 段级门槛挡下。
    ///
    /// **注**：这里测的是段级 A 层门槛。用户 v0.9.4 QQ 表情包 case（CJK 63%）走 A 层挡不住、
    /// 由 B 层（`explain_semantic_hit_impl` 里图片 doc_type 直接返空）兜底。
    #[test]
    fn explain_skips_obvious_noise_segments() {
        // 数字/符号大头（meaningful ratio 极低）：门槛挡下、explain 返空。
        let obvious_noise = "12345 6789 ab 12345 6789 cd 12345 6789";
        let out = explain_passages(obvious_noise, "作文", &AxisEmbedder);
        assert!(
            out.is_empty(),
            "数字/符号大头的段应被段级 meaningful_char_ratio 门槛挡下"
        );
    }
}
