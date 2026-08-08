//! 文档嵌入抽象。indexer 不依赖具体模型运行时——桌面层注入实现。
//!
//! 截断遵 BETA-26 §4.5：整篇首 1200 字、不分块（分块在该语料 wash-to-略负且成本 7.5×）。

use crate::IndexError;

/// 句向量生成器（由桌面层用 `model-runtime::embed()` 实现）。
pub trait TextEmbedder: Send + Sync {
    /// 嵌入一段文本，返回向量。
    fn embed(&self, text: &str) -> Result<Vec<f32>, IndexError>;
    /// 模型标识（写入 `document_vectors.embed_model`，换模型→旧向量陈旧）。
    fn model_id(&self) -> &str;
    /// 廉价探测：当前 `embed()` 是否有望成功（不触发模型加载）。
    ///
    /// 查询路由用它决定语义臂是否参与 fan-out——`false` 时语义臂整体退出、
    /// 整链降级到其余后端（BETA-33 cycle 9：feature 关 / 模型缺失 / 加载失败时
    /// 不再让必败的语义臂进入查询链并把 embed 错误冒充全链错误）。
    /// 默认 `true`（测试 / 评测 stub 恒可用，零行为变化）。
    fn is_ready(&self) -> bool {
        true
    }
}

/// 截断到首 `max_chars` 个**字符**（非字节，CJK 安全）。
#[must_use]
pub fn truncate_chars(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &text[..byte_idx],
        None => text,
    }
}

/// 嵌入用的正文截断上限。
///
/// **历史**：BETA-26 锁定 1200（基于 qwen3-0.6b context 8192、安全有余）。
/// BETA-15B-7-v2 切到 bge-m3（context 8192）也安全。
///
/// **BETA-31-v3 cycle 5（2026-06-30，v0.8.6 hotfix）：1200 → 600**
/// BETA-15B-11-v2 切到 embeddinggemma-300m、context 仅 2048 token。
/// 中文 SentencePiece tokenizer typical char-to-token ratio ≈ 1.5-2.0、
/// 1200 中文字符 → ~2000-2400 token → **溢出 2048 context** → llama-cpp ggml
/// 内部 abort/__fastfail → 进程被 ucrtbase 0xc0000409 强制终止（Rust catch_unwind
/// 兜不住 native abort）。v0.8.5 用户真机首次启用 SCOUT_ENABLE_EMBED=1 时
/// 第 1 篇文档（`2下53单元归类复习（语文）.pdf` body_len_chars=1200）即 crash 暴露。
///
/// 降到 600 字符：中文 ~1000-1200 token、留 800+ token buffer（prefix / EOS / 边界），
/// 比 context 2048 留 60%+ 安全裕度。
///
/// **影响**：
/// - 历史 369 残留向量保留（vector_is_current 检查 source_hash 不变、不重嵌）
/// - 新文档用前 600 字符嵌入、召回质量轻微下降（前 600 字常含主题词、docx 影响小、
///   长 PDF 章节中后文影响较大、留 cycle 5b token-aware truncate 精修）
/// - parser-only evals byte-equal 不受影响（不调 embed pipeline）
pub const EMBED_TRUNCATE_CHARS: usize = 600;

/// 嵌入用的正文**下限**（BETA-31-v3 cycle 3、2026-06-30）：`body.trim()` 后字符数
/// 不足此值的文档不入嵌入 pass。**Why**：BETA-15B-1 以来 `embed_pending` 对 documents
/// 表所有条目一视同仁、但 Windows 平台 BETA-03 OCR 路径对图片仅 `tesseract` best-effort、
/// 多数图片 body 为空字符串（v0.8.3 用户真机 SQLite inspect：2554/2670 PNG body_len=0、
/// 98% document_vectors 是图片空向量）。空字符串 / 极短文本经 embedding 模型产出的
/// "neutral" 向量与任意 query 的 cosine 都接近模型 mean similarity（~0.5-0.7）、远高于
/// floor=0.30、把 ranker top-N 全部挤占。用户搜「读后感」/ 任意中文短词全部返 Tencent /
/// Rockstar 缓存图片即此 bug。20 char 阈值经验值：单句中英文 ≥ 20 字符、嵌入价值才稳定。
pub const MIN_EMBED_TEXT_CHARS: usize = 20;

/// 嵌入正文的**有意义字符占比下限**（BETA-33 cycle 4、2026-07-01）。
///
/// `body.trim()` 中 CJK 表意字符 + 拉丁字母占非空白字符的比例 < 此值即视为乱码 / 低信号、
/// 不入嵌入 pass。**Why**：cycle 3 的 20 字下限只挡住空 body / 超短文本，
/// 但 Windows tesseract 对 QQ/微信缓存图（表情包、emoji sprite sheet 等）常吐 40-100 字
/// 的"看似像中文实则乱码"OCR 文本（大量数字/@符号/全角标点夹带少量真汉字）。
/// 这种文本经 embeddinggemma-300m 后仍落在"中文均值方向"、与任意短中文 query cosine 达
/// 0.5-0.7、把 ranker top-N 挤占（v0.9.4 用户搜「作文」发现 face-3-efdc54.png 表情包命中
/// 段落级 cosine 0.62 即此 bug 的段落级延伸）。
///
/// 0.6 门槛的经验依据：真中文正文 meaningful_ratio 通常 > 0.75、英文正文 > 0.90、
/// OCR 乱码 case 实测 0.55-0.60；0.6 卡在乱码上界、真文档零误伤。
pub const MEANINGFUL_CHAR_RATIO_FLOOR: f32 = 0.6;

/// 图片 OCR 文本的**有意义字符占比下限**（BETA-39、2026-07-03）。
///
/// 图片 doc_type 的 opt-in 语义嵌入专属门槛，比通用 [`MEANINGFUL_CHAR_RATIO_FLOOR`]（0.6）
/// 更严。**Why**：cycle 4 一刀切的根因是已知污染 case（QQ 表情包 OCR「动 @ 河的…」）
/// ratio ≈ 0.63、通用 0.6 挡不住；解除一刀切必须配更严门槛，否则污染复现。
/// 0.75 的依据：真中文正文 > 0.75、英文正文 > 0.90、真文字截图通常 > 0.8，
/// 已知乱码 case 实测 0.55-0.63——0.75 卡在乱码上界之上、真文字图片零误伤。
pub const IMAGE_MEANINGFUL_RATIO_FLOOR: f32 = 0.75;

/// 判断文本是否值得嵌入：先过字数门槛 [`MIN_EMBED_TEXT_CHARS`]、再过有意义字符占比
/// [`MEANINGFUL_CHAR_RATIO_FLOOR`]。
///
/// 用于 3 处：
/// - `embed_pending`：过滤入嵌候选
/// - `purge_short_body_vectors`：启动期数据清理判定同款口径
/// - `explain_passages`：段落级 embed 前置门槛（cycle 4 新加、否则乱码段照样虚高）
#[must_use]
pub fn is_embed_worthy(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.chars().count() < MIN_EMBED_TEXT_CHARS {
        return false;
    }
    meaningful_char_ratio(trimmed) >= MEANINGFUL_CHAR_RATIO_FLOOR
}

/// 图片 OCR 文本是否值得嵌入（BETA-39 opt-in 路径专用）：字数门槛沿用
/// [`MIN_EMBED_TEXT_CHARS`]、ratio 用更严的 [`IMAGE_MEANINGFUL_RATIO_FLOOR`]。
///
/// 用于 3 处（与 `is_embed_worthy` 平行、图片分支）：
/// - `embed_pending(embed_images=true)`：图片入嵌候选门槛
/// - `purge_short_body_vectors(keep_worthy_images=true)`：保留判定同款口径
/// - 桌面段落级 explain 的图片分支（段级 ratio 同 0.75）
#[must_use]
pub fn is_image_embed_worthy(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.chars().count() < MIN_EMBED_TEXT_CHARS {
        return false;
    }
    meaningful_char_ratio(trimmed) >= IMAGE_MEANINGFUL_RATIO_FLOOR
}

/// 有意义字符（CJK 表意字 + 拉丁字母）占非空白字符的比例。全空白 / 空 → 0。
///
/// **不算**"有意义"的：数字、标点、全角/半角符号、emoji、控制字符、其他 script（暂）。
/// 之所以拉丁字母算：英文正文本来就靠字母组词、`this is` 阈值必须过；至于中英混排文档
/// 只要中文段自己够密就必然过。
///
/// 覆盖 CJK Unified Ideographs 主区（U+4E00–U+9FFF）+ Extension A（U+3400–U+4DBF），
/// 覆盖 GB2312 一二级 + GB18030 常用扩展；进一步扩展区（B/C/D/E/F）多为罕见古字、
/// 真文档命中极低、忽略。
#[must_use]
pub fn meaningful_char_ratio(text: &str) -> f32 {
    let mut meaningful = 0usize;
    let mut non_ws = 0usize;
    for c in text.chars() {
        if c.is_whitespace() {
            continue;
        }
        non_ws += 1;
        if is_cjk_ideograph(c) || c.is_ascii_alphabetic() {
            meaningful += 1;
        }
    }
    if non_ws == 0 {
        0.0
    } else {
        // 允许 clippy: as 转换：usize→f32 精度损失可接受，body 已被截断到 ≤600 字符。
        #[allow(clippy::cast_precision_loss)]
        let r = meaningful as f32 / non_ws as f32;
        r
    }
}

fn is_cjk_ideograph(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF)
}

/// FNV-1a 64bit 偏移基准（`content_hash` 与 [`file_identity_hash`] 共用同一算法）。
const FNV1A_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a 64bit 增量喂入一段字节（原地更新 `state`）。
fn fnv1a_update(state: &mut u64, bytes: &[u8]) {
    let mut h = *state;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    *state = h;
}

/// 稳定内容指纹（写入 `source_hash`；正文没变→跳过重嵌）。
/// 用 FNV-1a 64bit，零依赖、确定性。
#[must_use]
pub fn content_hash(text: &str) -> String {
    let mut state = FNV1A_OFFSET;
    fnv1a_update(&mut state, text.as_bytes());
    format!("{state:016x}")
}

/// BETA-38 doc identity：**文件原始全字节内容指纹**（FNV-1a 64bit，零依赖、确定性）。
///
/// 与 [`content_hash`]（截断后正文指纹，会因格式/提取差异漂移）不同：本函数对**磁盘上文件的
/// 原始字节**流式喂入，故同一份材料的多份完全相同副本（判决书存多盘 / 迁移盘 / 压缩包展开）
/// 得到相同身份。**流式读取**（8KB 缓冲）不把整文件读进内存，适配大归档文件。
///
/// 读取失败（权限 / 占位符 / 文件消失）→ `Err`；调用方应降级为 `content_hash=None`（不阻断索引）。
pub fn file_identity_hash(path: &std::path::Path) -> Result<String, IndexError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| IndexError::Tag {
        path: path.to_string_lossy().into_owned(),
        detail: format!("身份 hash 打开失败: {e}"),
    })?;
    let mut state = FNV1A_OFFSET;
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).map_err(|e| IndexError::Tag {
            path: path.to_string_lossy().into_owned(),
            detail: format!("身份 hash 读取失败: {e}"),
        })?;
        if n == 0 {
            break;
        }
        fnv1a_update(&mut state, &buf[..n]);
    }
    Ok(format!("{state:016x}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn truncate_is_char_safe_for_cjk() {
        let s = "季度预算分析报告";
        assert_eq!(truncate_chars(s, 4), "季度预算");
        assert_eq!(truncate_chars(s, 100), s);
        assert_eq!(truncate_chars(s, 0), "");
    }

    #[test]
    fn content_hash_is_stable_and_sensitive() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
        assert_eq!(content_hash("").len(), 16);
    }

    /// BETA-38：文件身份指纹——相同字节同 hash（副本判等）、不同字节异 hash、跨缓冲边界稳定。
    #[test]
    fn file_identity_hash_matches_across_copies_and_buffer_boundary() {
        let dir = tempfile::tempdir().unwrap();
        // 大于 8KB 缓冲的正文，验证流式分块喂入与一次性喂入结果一致。
        let big = "判决书正文".repeat(4000); // 远超 8192 字节
        let a = dir.path().join("a.txt");
        let b_copy = dir.path().join("b_copy.txt");
        let c_diff = dir.path().join("c_diff.txt");
        std::fs::write(&a, &big).unwrap();
        std::fs::write(&b_copy, &big).unwrap(); // 完全相同副本
        std::fs::write(&c_diff, format!("{big}X")).unwrap(); // 差一字节

        let ha = file_identity_hash(&a).unwrap();
        let hb = file_identity_hash(&b_copy).unwrap();
        let hc = file_identity_hash(&c_diff).unwrap();
        assert_eq!(ha, hb, "相同字节副本应同身份 hash");
        assert_ne!(ha, hc, "差一字节应异 hash");
        assert_eq!(ha.len(), 16, "16 位十六进制");
        // 流式（分块）= 一次性喂入等价：与 content_hash 对同一 UTF-8 字节串结果一致。
        assert_eq!(ha, content_hash(&big), "流式身份 hash 与整串 FNV-1a 等价");
    }

    /// BETA-38：不存在的路径 → Err（调用方降级 None，不阻断索引）。
    #[test]
    fn file_identity_hash_missing_path_errors() {
        let missing = std::path::Path::new("/nonexistent/never/here.bin");
        assert!(file_identity_hash(missing).is_err());
    }

    /// is_embed_worthy 边界：空 / 全空白 / < 20 / = 20 / > 20。
    #[test]
    fn is_embed_worthy_threshold() {
        assert!(!is_embed_worthy(""), "空字符串");
        assert!(!is_embed_worthy("   \n\t"), "全空白");
        assert!(!is_embed_worthy("短"), "1 字符");
        // 19 char 不过
        let s19 = "a".repeat(19);
        assert!(!is_embed_worthy(&s19), "19 字符不过");
        // 20 char 恰好过
        let s20 = "a".repeat(20);
        assert!(is_embed_worthy(&s20), "20 字符纯拉丁恰好过");
        // 中文 20 字符过
        let cn20 = "文".repeat(20);
        assert!(is_embed_worthy(&cn20), "中文 20 字符过");
        // 前后空白被 trim、有效 20 字符过
        let padded = format!("  {}  ", "a".repeat(20));
        assert!(is_embed_worthy(&padded), "trim 后 20 字符过");
    }

    /// BETA-33 cycle 4：`meaningful_char_ratio` 基本行为。
    #[test]
    fn meaningful_char_ratio_classification() {
        // 全 CJK → 100%
        assert!((meaningful_char_ratio("我今天写作业") - 1.0).abs() < 1e-6);
        // 全英文字母 → 100%
        assert!((meaningful_char_ratio("hello world") - 1.0).abs() < 1e-6);
        // 全数字 → 0%（数字不算有意义）
        assert!(meaningful_char_ratio("1234567890").abs() < 1e-6);
        // 空白全跳过、其余算
        assert!((meaningful_char_ratio("  a  ") - 1.0).abs() < 1e-6);
        // 空 / 全空白 → 0
        assert!(meaningful_char_ratio("").abs() < 1e-6);
        assert!(meaningful_char_ratio("   \n\t").abs() < 1e-6);
    }

    /// BETA-33 cycle 4：真中文正文过、明显噪声（数字/符号占大头）不过。
    ///
    /// **注**：`is_embed_worthy` 是 A 层门槛、挡"数字/符号大头"的明显噪声。
    /// 用户 v0.9.4 遇到的 QQ 表情包 OCR case「动 @ 河的...」CJK 占比 63%、A 层挡不住；
    /// 那种"CJK-heavy 乱码"由 B 层（`embed_pending` 里图片 doc_type 直接跳过语义索引）兜底。
    #[test]
    fn is_embed_worthy_rejects_obvious_noise_and_keeps_real_text() {
        // 真中文短笔记（20 字纯汉字）：过
        assert!(
            is_embed_worthy("我今天写了一篇关于春天的作文老师说写得很好"),
            "真中文正文应过"
        );
        // 真英文正文：过
        assert!(
            is_embed_worthy("This is a technical document about deep learning."),
            "真英文正文应过"
        );
        // 中英混排：过
        assert!(
            is_embed_worthy("2026 年 Q1 财报：revenue up 12% vs 去年同期。"),
            "中英混排（少量数字/标点）应过"
        );
        // 数字/符号大头（拉丁字母 6/38 ≈ 16%）：不过
        assert!(
            !is_embed_worthy("12345 6789 ab 12345 6789 cd 12345 6789"),
            "数字符号大头的伪文本应被 A 层挡下"
        );
        // 全数字 + 少量汉字（4/24 ≈ 17%）：不过
        assert!(
            !is_embed_worthy("12345678901234567890年月日周"),
            "数字占大头的伪文本不过"
        );
    }

    /// BETA-39：图片专属门槛 0.75——通用 0.6 过但 0.75 不过的「CJK-heavy 乱码」被挡。
    /// 已知污染 case（QQ 表情包 OCR）ratio ≈ 0.63 落在 (0.6, 0.75) 区间、正是本门槛的靶。
    #[test]
    fn is_image_embed_worthy_blocks_cjk_heavy_noise() {
        // 构造 ratio = 0.65 的 20 字文本（13 汉字 + 7 数字）：通用门槛过、图片门槛不过。
        let noise = format!(
            "{}{}",
            "动河的天写在有里上作文好看"
                .chars()
                .take(13)
                .collect::<String>(),
            "1234567"
        );
        assert_eq!(noise.chars().count(), 20);
        assert!(is_embed_worthy(&noise), "ratio 0.65 应过通用 0.6 门槛");
        assert!(
            !is_image_embed_worthy(&noise),
            "ratio 0.65 的 CJK-heavy 乱码应被图片 0.75 门槛挡下"
        );
        // 真文字截图（纯中文、ratio 1.0）：图片门槛过。
        assert!(
            is_image_embed_worthy("我今天写了一篇关于春天的作文老师说写得很好"),
            "真中文截图 OCR 应过图片门槛"
        );
        // 英文截图：过。
        assert!(
            is_image_embed_worthy("Meeting notes from the quarterly planning session."),
            "真英文截图 OCR 应过图片门槛"
        );
        // 字数门槛沿用 20：19 字纯汉字不过。
        assert!(
            !is_image_embed_worthy(&"文".repeat(19)),
            "19 字不过字数门槛"
        );
        // 边界：恰好 0.75（15 字母 + 5 数字）过、0.7（14+6）不过。
        let exact = format!("{}{}", "a".repeat(15), "0".repeat(5));
        assert!(is_image_embed_worthy(&exact), "恰好 0.75 应过");
        let below = format!("{}{}", "a".repeat(14), "0".repeat(6));
        assert!(!is_image_embed_worthy(&below), "0.70 不过 0.75 门槛");
    }

    /// `is_embed_worthy` 与 `MIN_EMBED_TEXT_CHARS` 门槛先后关系：≥20 字但有意义比 <60% 不过。
    #[test]
    fn is_embed_worthy_requires_both_gates() {
        // ≥20 字但 60% 是数字：不过
        assert!(!is_embed_worthy("a0000000000000000000")); // 1 字母 + 19 数字
                                                           // 恰好 60%：过
        let s = format!("{}{}", "a".repeat(12), "0".repeat(8)); // 12/20 = 60%
        assert!(is_embed_worthy(&s));
        // 59%：不过
        let s2 = format!("{}{}", "a".repeat(11), "0".repeat(9)); // 11/20 = 55%
        assert!(!is_embed_worthy(&s2));
    }
}
