//! query 语种检测（CJK ratio 三态二阈、纯 std、零依赖）。
//! BETA-15B-3 A-4 引入作 wrapper 路由信号；A-5 路由信号换 cosine 后，本函数仅供
//! 生产 wiring（`harness::fanout_merge::run_fanout_merge_rrf`）后置覆写
//! `RouteVerdict.query_lang` 字段填可观测元数据（供 BETA-15B-5 badge 槽位消费）。

/// query 语种三态。A-5 起仅作 `RouteVerdict.query_lang` 可观测元数据；不再驱动路由动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
    Mixed,
}

/// CJK ratio 三态二阈检测：>0.6=Zh、<0.05=En、之间=Mixed。
/// 分母 = CJK chars + ASCII alphanumeric chars 总数；分母为 0 → Mixed（保守降级）。
/// CJK 覆盖范围：Unified Ideographs (U+4E00–U+9FFF)
/// + Compatibility (U+F900–U+FAFF) + Ext-A (U+3400–U+4DBF)。
#[must_use]
pub fn detect_lang(text: &str) -> Lang {
    let mut cjk = 0_usize;
    let mut alnum = 0_usize;
    for c in text.chars() {
        if is_cjk(c) {
            cjk += 1;
        } else if c.is_ascii_alphanumeric() {
            alnum += 1;
        }
    }
    let total = cjk + alnum;
    if total == 0 {
        return Lang::Mixed;
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = cjk as f64 / total as f64;
    if ratio > 0.6 {
        Lang::Zh
    } else if ratio < 0.05 {
        Lang::En
    } else {
        Lang::Mixed
    }
}

const fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF
        | 0xF900..=0xFAFF
        | 0x3400..=0x4DBF
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn pure_chinese_query_is_zh() {
        assert_eq!(detect_lang("年假规定与远程办公细则"), Lang::Zh);
    }

    #[test]
    fn pure_english_query_is_en() {
        assert_eq!(detect_lang("annual leave policy"), Lang::En);
    }

    #[test]
    fn mostly_chinese_with_one_english_term_is_zh() {
        // 「iOS 备份指南手册说明」3 ASCII + 8 CJK = ratio 8/11 ≈ 0.727 > 0.6 → Zh
        assert_eq!(detect_lang("iOS 备份指南手册说明"), Lang::Zh);
    }

    #[test]
    fn english_query_with_small_punctuation_is_en() {
        // 小于 0.05 CJK → En
        assert_eq!(detect_lang("git push origin main"), Lang::En);
    }

    #[test]
    fn balanced_mix_is_mixed() {
        // 「qwen 调优」3 ASCII + 2 CJK = ratio 2/5 = 0.4 → Mixed
        assert_eq!(detect_lang("qwen 调优"), Lang::Mixed);
    }

    #[test]
    fn empty_query_is_mixed() {
        // 分母 0 → 保守降级
        assert_eq!(detect_lang(""), Lang::Mixed);
    }

    #[test]
    fn whitespace_and_punct_only_is_mixed() {
        // 既无 CJK 也无 alnum → 分母 0 → 保守降级
        assert_eq!(detect_lang("   ... !!"), Lang::Mixed);
    }

    #[test]
    fn cjk_ext_a_is_zh() {
        // U+3400 Ext-A 区
        let s: String = std::iter::once(char::from_u32(0x3400).unwrap()).collect();
        assert_eq!(detect_lang(&s), Lang::Zh);
    }
}
