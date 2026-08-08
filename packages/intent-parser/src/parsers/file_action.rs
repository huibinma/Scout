//! `SearchIntent::FileAction` 规则解析器。
#![allow(clippy::pedantic, clippy::expect_used)]

use scout_search_backend::{
    FileAction, FileActionKind, Language, SchemaVersion, SearchIntent, TargetRef, TargetSelector,
};

use super::common::word_present;

pub(crate) fn try_parse_file_action(
    input: &str,
    lower: &str,
    language: Language,
) -> Option<SearchIntent> {
    let open = lower.contains("打开") || word_present(lower, "open");
    let locate = lower.contains("在访达")
        || lower.contains("show in finder")
        || lower.contains("reveal")
        || lower.contains("in finder")
        // v0.5：mixed/英文 Finder 显示
        || lower.contains("finder 显示")
        || lower.contains("finder显示")
        || lower.contains("在 finder")
        || lower.contains("finder 里")
        // BETA-13-G5：自然定位措辞
        || lower.contains("定位")
        || lower.contains("文件夹里显示")
        || lower.contains("show me where")
        || word_present(lower, "locate");
    let rename =
        lower.contains("改名") || lower.contains("重命名") || word_present(lower, "rename");
    // 英文 copy / move 用 word_present 单独识别；target_ref 提取失败时返 None 保护
    // 不会误触发到普通 file_search query（无"第N个"/"the N result"）
    let copy = lower.contains("复制到") || lower.contains("copy to") || word_present(lower, "copy");
    let move_op =
        lower.contains("移动到") || lower.contains("move to") || word_present(lower, "move");
    // BETA-13-G5：删除 / 移入回收站 → delete（"全部删/delete all" 已在 lib 顶层 clarify 拦截）。
    // 不用裸 "remove"（refine "remove the ... restriction" 走 refine）。
    let delete = lower.contains("删除")
        || lower.contains("删掉")
        || lower.contains("回收站")
        || word_present(lower, "delete");

    if !(open || locate || rename || copy || move_op || delete) {
        return None;
    }

    let (action, requires_confirmation, new_name, destination) = if locate {
        (FileActionKind::Locate, false, None, None)
    } else if rename {
        let name = extract_new_name(input);
        (FileActionKind::Rename, true, name, None)
    } else if delete {
        (FileActionKind::Delete, true, None, None)
    } else if copy {
        let dest = extract_destination(input, lower);
        (FileActionKind::Copy, true, None, dest)
    } else if move_op {
        let dest = extract_destination(input, lower);
        (FileActionKind::Move, true, None, dest)
    } else {
        (FileActionKind::Open, false, None, None)
    };

    // 提取目标引用（序数 / 多序数 / 全部 / 指示代词 / 路径）。
    // BETA-13-G12 ③′：动作 + 目的地/新名已明确但无显式目标（如「移动到文档文件夹」/
    // 「重命名为 终稿」）→ 默认作用于首个结果（last_results Index 1），而非降级 file_search。
    // **门控在「抽到 destination 或 new_name」**：避免裸动作词（如孤立「移动」/英文 "copy"
    // 偶发命中）被误判——无具体祈使目标时仍返 None 落 file_search。
    let target_ref = match extract_target_ref(input, lower) {
        Some(t) => t,
        None if destination.is_some() || new_name.is_some() => TargetRef::LastResults {
            selector: TargetSelector::Index { value: 1 },
        },
        None => return None,
    };

    Some(SearchIntent::FileAction(FileAction {
        schema_version: SchemaVersion::V1,
        language: Some(language),
        action,
        target_ref,
        destination,
        new_name,
        requires_confirmation,
    }))
}

fn extract_target_ref(input: &str, lower: &str) -> Option<TargetRef> {
    use regex::Regex;
    use std::sync::OnceLock;

    // 0. BETA-13-G5：绝对路径目标（"打开 /Users/me/x.pdf" / "open /Users/..."）。
    // 目的地介词（到/去/to/into）引导的路径是 destination 不是 target
    // （「把它们移动到 /Users/me/Pictures/2026」的 target 是 它们）→ 跳过，走后续规则。
    static RE_PATH: OnceLock<Regex> = OnceLock::new();
    let re_path = RE_PATH.get_or_init(|| Regex::new(r"(/[^\s，。、]+)").expect("regex"));
    if let Some(cap) = re_path.captures(input) {
        let before = input[..cap.get(1).expect("group").start()].trim_end();
        let is_destination = before.ends_with('到')
            || before.ends_with('去')
            || before.to_lowercase().ends_with(" to")
            || before.to_lowercase().ends_with(" into")
            || before.to_lowercase().ends_with("into");
        if !is_destination {
            return Some(TargetRef::Path {
                value: cap[1].to_string(),
            });
        }
    }

    // 1. BETA-13-G5：多序数 → Indices（"第2和第4个" / "the 2nd and 4th" / "第1、3、5个"）。
    // BETA-13-G12 ③′：先处理「第N、M、K个」顿号/逗号列表——第 引导一次，后续数字省略 第。
    // 旧 `第\s*(\d+)` 只抓到首个「第1」，漏掉「、3、5」。
    static RE_ZH_MULTI: OnceLock<Regex> = OnceLock::new();
    let re_zh_multi =
        RE_ZH_MULTI.get_or_init(|| Regex::new(r"第\s*\d+(?:\s*[、,]\s*\d+)+").expect("regex"));
    if let Some(m) = re_zh_multi.find(input) {
        static RE_DIGIT: OnceLock<Regex> = OnceLock::new();
        let re_digit = RE_DIGIT.get_or_init(|| Regex::new(r"\d+").expect("regex"));
        let values: Vec<u32> = re_digit
            .find_iter(m.as_str())
            .filter_map(|n| n.as_str().parse::<u32>().ok())
            .collect();
        if values.len() > 1 {
            return Some(TargetRef::LastResults {
                selector: TargetSelector::Indices { values },
            });
        }
    }
    let mut indices: Vec<u32> = Vec::new();
    static RE_ZH_ORD: OnceLock<Regex> = OnceLock::new();
    let re_zh_ord = RE_ZH_ORD.get_or_init(|| Regex::new(r"第\s*(\d+)").expect("regex"));
    for cap in re_zh_ord.captures_iter(input) {
        if let Ok(v) = cap[1].parse::<u32>() {
            indices.push(v);
        }
    }
    if indices.is_empty() {
        // 英文 "the 2nd and 4th" / "items 1, 3 and 5"
        if lower.contains(" and ") || lower.contains("items ") || lower.contains(", ") {
            static RE_EN_ORD: OnceLock<Regex> = OnceLock::new();
            let re_en_ord =
                RE_EN_ORD.get_or_init(|| Regex::new(r"\b(\d+)(?:st|nd|rd|th)\b").expect("regex"));
            for cap in re_en_ord.captures_iter(lower) {
                if let Ok(v) = cap[1].parse::<u32>() {
                    indices.push(v);
                }
            }
            // "items 1, 3 and 5" — 裸数字列表
            if indices.is_empty() && lower.contains("items ") {
                static RE_EN_NUMS: OnceLock<Regex> = OnceLock::new();
                let re_nums = RE_EN_NUMS
                    .get_or_init(|| Regex::new(r"items\s+([\d,\s and]+)").expect("regex"));
                if let Some(cap) = re_nums.captures(lower) {
                    for n in Regex::new(r"\d+").expect("regex").find_iter(&cap[1]) {
                        if let Ok(v) = n.as_str().parse::<u32>() {
                            indices.push(v);
                        }
                    }
                }
            }
        }
    }
    if indices.len() > 1 {
        // 范围 "第1个到第3个" → 展开
        if (input.contains("到") || lower.contains(" to ")) && indices.len() == 2 {
            let (a, b) = (indices[0], indices[1]);
            if a < b && b - a <= 50 {
                indices = (a..=b).collect();
            }
        }
        return Some(TargetRef::LastResults {
            selector: TargetSelector::Indices { values: indices },
        });
    }

    // 2. 全部 → All（中文 这些/这几个/全都/它们/全部；英文 these/them/all of them/all of these）
    if input.contains("这些")
        || input.contains("这几个")
        || input.contains("全都")
        || input.contains("它们")
        || input.contains("全部")
        || lower.contains("these")
        || lower.contains("them")
        || lower.contains("all of them")
        || lower.contains("all of these")
    {
        return Some(TargetRef::LastResults {
            selector: TargetSelector::All,
        });
    }

    // 3. 单序数："第1个" / "第2张" / "第 1 个"
    static RE_ZH_DIGIT: OnceLock<Regex> = OnceLock::new();
    let re_zh_digit =
        RE_ZH_DIGIT.get_or_init(|| Regex::new(r"第\s*(\d+)\s*[个张份]").expect("regex"));
    if let Some(cap) = re_zh_digit.captures(input) {
        if let Ok(value) = cap[1].parse::<u32>() {
            return Some(TargetRef::LastResults {
                selector: TargetSelector::Index { value },
            });
        }
    }

    // 4. 中文 ordinal word（含"张"量词）
    let cn_ord = match () {
        () if input.contains("第一个") || input.contains("第一张") => Some(1u32),
        () if input.contains("第二个") || input.contains("第二张") => Some(2),
        () if input.contains("第三个") || input.contains("第三张") => Some(3),
        () if input.contains("第四个") || input.contains("第四张") => Some(4),
        () if input.contains("第五个") || input.contains("第五张") => Some(5),
        () => None,
    };
    if let Some(value) = cn_ord {
        return Some(TargetRef::LastResults {
            selector: TargetSelector::Index { value },
        });
    }

    // 5. 英文 "the N result" / "the N-th"
    static RE_EN_DIGIT: OnceLock<Regex> = OnceLock::new();
    let re_en_digit = RE_EN_DIGIT.get_or_init(|| {
        Regex::new(r"\bthe\s+(\d+)(?:st|nd|rd|th)?\s+(?:result|one|item|file)\b").expect("regex")
    });
    if let Some(cap) = re_en_digit.captures(lower) {
        if let Ok(value) = cap[1].parse::<u32>() {
            return Some(TargetRef::LastResults {
                selector: TargetSelector::Index { value },
            });
        }
    }

    // 6. 英文 ordinal word
    let en_ord = match () {
        () if lower.contains("first one") || lower.contains("the first") => Some(1u32),
        () if lower.contains("second one") || lower.contains("the second") => Some(2),
        () if lower.contains("third one") || lower.contains("the third") => Some(3),
        () if lower.contains("fourth one") || lower.contains("the fourth") => Some(4),
        () if lower.contains("fifth one") || lower.contains("the fifth") => Some(5),
        () => None,
    };
    if let Some(value) = en_ord {
        return Some(TargetRef::LastResults {
            selector: TargetSelector::Index { value },
        });
    }

    // 7. BETA-13-G5：指示代词 → 默认首个（"这个文件" / "这张" / "最后那个" / "this file" / "it"）。
    if input.contains("这个")
        || input.contains("这张")
        || input.contains("最后那个")
        || lower.contains("this file")
        || lower.contains("this one")
        || lower.contains(" it ")
        || lower.ends_with(" it")
        || lower.contains("it to ")
        || lower.contains("it in ")
        || input.contains("把它")
    {
        return Some(TargetRef::LastResults {
            selector: TargetSelector::Index { value: 1 },
        });
    }

    None
}

fn extract_new_name(input: &str) -> Option<String> {
    // 优先 "改名为 X" / "重命名为 X" / "改成 X" / "rename to X" 这种紧贴的明确介词
    // （含中英混排介词：「rename 为 X」「rename 成 X」「重命名成 X」「改名成 X」）
    for phrase in [
        "改名为",
        "重命名为",
        "重命名成",
        "改名成",
        "改成",
        "rename to",
        "rename 为",
        "rename 成",
    ] {
        if let Some(pos) = input.find(phrase) {
            let after = input[pos + phrase.len()..].trim_start();
            let name: String = after
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '。' && *c != '，')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    // v0.5："rename X to Y" 这种 X / Y 之间隔了字段（如"rename the 5 result to synthetic-final"）。
    // 取 "rename" 之后**最后一个** " to " 之后的 token。
    let lower = input.to_lowercase();
    if lower.contains("rename") {
        if let Some(idx) = lower.rfind(" to ") {
            let after = input[idx + 4..].trim_start();
            let name: String = after
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '。' && *c != '，')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

fn extract_destination(input: &str, lower: &str) -> Option<String> {
    // 显式路径目的地（「移动到 /Users/me/Pictures/2026」/ "into /Users/me/Documents/backup"）
    // 原样返回（大小写保留），优先于关键词映射——路径串里的 pictures/documents 不参与家目录映射。
    if let Some(path) = extract_destination_path(input) {
        return Some(path);
    }
    // BETA-13-G12 ③′：U盘/优盘/USB/移动硬盘 = 外接卷，无 home(~) 形态 → /Volumes/USB。
    if lower.contains("u盘") || lower.contains("优盘") || lower.contains("usb") {
        return Some("/Volumes/USB".into());
    }
    if lower.contains("external drive") || lower.contains("外接硬盘") || lower.contains("移动硬盘")
    {
        return Some("/Volumes/External".into());
    }
    if lower.contains("桌面") || lower.contains("desktop") {
        return Some("~/Desktop".into());
    }
    if lower.contains("下载") || lower.contains("downloads") {
        return Some("~/Downloads".into());
    }
    if lower.contains("文稿") || lower.contains("documents") || lower.contains("文档") {
        return Some("~/Documents".into());
    }
    if lower.contains("图片") || lower.contains("pictures") {
        return Some("~/Pictures".into());
    }
    None
}

/// 目的地介词（到/去/to/into）之后紧跟的绝对/家目录路径，大小写保留。
fn extract_destination_path(input: &str) -> Option<String> {
    use regex::Regex;
    use std::sync::OnceLock;
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?:到|去|\bto\s+|\binto\s+)\s*((?:/|~/)[^\s，。、]+)").expect("regex")
    });
    Some(re.captures(input)?[1].to_string())
}

#[cfg(test)]
mod tests_v05 {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use crate::parse;
    use scout_search_backend::{FileActionKind, SearchIntent, TargetRef, TargetSelector};

    fn assert_file_action(intent: &SearchIntent) -> &scout_search_backend::FileAction {
        match intent {
            SearchIntent::FileAction(fa) => fa,
            other => panic!("expected FileAction, got {other:?}"),
        }
    }

    #[test]
    fn v05_ba_zhexie_copy_to_desktop_routes_to_file_action() {
        // v05-schema-39b-040
        let intent = parse("把这些 pdf 复制到桌面");
        let fa = assert_file_action(&intent);
        assert_eq!(fa.action, FileActionKind::Copy);
        assert_eq!(fa.destination.as_deref(), Some("~/Desktop"));
        assert!(matches!(
            fa.target_ref,
            TargetRef::LastResults {
                selector: TargetSelector::All
            }
        ));
    }

    // ===== BETA-13-G12 ③′：无上下文动作命令误路由到 file_search 的修复 =====

    #[test]
    fn g12_move_to_folder_defaults_to_first_result() {
        // v09-d6-zh-004：「移动到文档文件夹」动作明确 + 目的地明确但无显式序数 →
        // 默认作用于首个结果（Index 1），而非降级 file_search。
        let intent = parse("移动到文档文件夹");
        let fa = assert_file_action(&intent);
        assert_eq!(fa.action, FileActionKind::Move);
        assert_eq!(fa.destination.as_deref(), Some("~/Documents"));
        assert!(fa.requires_confirmation);
        assert!(matches!(
            fa.target_ref,
            TargetRef::LastResults {
                selector: TargetSelector::Index { value: 1 }
            }
        ));
    }

    #[test]
    fn g12_rename_to_defaults_to_first_result() {
        // v09-d6-zh-005：「重命名为 终稿」→ rename + new_name=终稿 + 默认首个结果。
        let intent = parse("重命名为 终稿");
        let fa = assert_file_action(&intent);
        assert_eq!(fa.action, FileActionKind::Rename);
        assert_eq!(fa.new_name.as_deref(), Some("终稿"));
        assert!(fa.requires_confirmation);
        assert!(matches!(
            fa.target_ref,
            TargetRef::LastResults {
                selector: TargetSelector::Index { value: 1 }
            }
        ));
    }

    #[test]
    fn g12_multi_ordinal_comma_list_to_usb() {
        // v09-d6-zh-008：「把第1、3、5个复制到U盘」→ copy + indices[1,3,5] + /Volumes/USB。
        let intent = parse("把第1、3、5个复制到U盘");
        let fa = assert_file_action(&intent);
        assert_eq!(fa.action, FileActionKind::Copy);
        assert_eq!(fa.destination.as_deref(), Some("/Volumes/USB"));
        assert!(matches!(
            &fa.target_ref,
            TargetRef::LastResults {
                selector: TargetSelector::Indices { values }
            } if values == &[1, 3, 5]
        ));
    }

    #[test]
    fn g12_incidental_action_word_without_dest_stays_file_search() {
        // 守护：动作词但无目的地/新名/序数（无强祈使）→ 不应误判 file_action（默认目标只在
        // 抽到 destination/new_name 时触发）。
        assert!(
            !matches!(parse("移动"), SearchIntent::FileAction(_)),
            "裸「移动」不应路由 file_action"
        );
    }

    #[test]
    fn v05_zai_finder_xianshi_di_2_ge_routes_to_locate() {
        // v05-action-template-379
        let intent = parse("在 Finder 显示第2个");
        let fa = assert_file_action(&intent);
        assert_eq!(fa.action, FileActionKind::Locate);
        assert!(matches!(
            fa.target_ref,
            TargetRef::LastResults {
                selector: TargetSelector::Index { value: 2 }
            }
        ));
    }
}

#[cfg(test)]
mod tests_beta14_gap_cut3 {
    //! BETA-14 缺口盘点第 3 刀：rename 混排介词 / 显式路径目的地 / external drive。
    #![allow(clippy::unwrap_used, clippy::panic)]
    use crate::parse;
    use scout_search_backend::{FileActionKind, SearchIntent, TargetRef, TargetSelector};

    fn action_of(q: &str) -> scout_search_backend::FileAction {
        match parse(q) {
            SearchIntent::FileAction(fa) => fa,
            other => panic!("{q} 应 file_action，实际 {other:?}"),
        }
    }

    #[test]
    fn rename_mixed_prepositions() {
        // 「rename 为/成」「重命名成」混排介词形态
        for (q, name) in [
            ("把第5个 rename 为 synthetic-final", "synthetic-final"),
            ("把第二个 rename 成 final稿", "final稿"),
            ("把第二个重命名成 会议纪要", "会议纪要"),
        ] {
            let fa = action_of(q);
            assert_eq!(fa.action, FileActionKind::Rename, "{q}");
            assert_eq!(fa.new_name.as_deref(), Some(name), "{q}");
        }
    }

    #[test]
    fn explicit_path_is_destination_not_target() {
        // 目的地介词引导的路径 → destination（原样大小写），target 归指代/序数
        let fa = action_of("把它们移动到 /Users/me/Pictures/2026");
        assert_eq!(fa.action, FileActionKind::Move);
        assert_eq!(fa.destination.as_deref(), Some("/Users/me/Pictures/2026"));
        assert!(matches!(
            fa.target_ref,
            TargetRef::LastResults {
                selector: TargetSelector::All
            }
        ));

        let fa = action_of("move the fifth file to /Users/me/Pictures/2026");
        assert_eq!(fa.destination.as_deref(), Some("/Users/me/Pictures/2026"));
        assert!(matches!(
            fa.target_ref,
            TargetRef::LastResults {
                selector: TargetSelector::Index { value: 5 }
            }
        ));

        let fa = action_of("copy the third one into /Users/me/Documents/backup");
        assert_eq!(
            fa.destination.as_deref(),
            Some("/Users/me/Documents/backup")
        );
        assert!(matches!(
            fa.target_ref,
            TargetRef::LastResults {
                selector: TargetSelector::Index { value: 3 }
            }
        ));
    }

    #[test]
    fn bare_path_target_still_works() {
        // 无目的地介词的路径仍是 target（G5 原语义）
        let fa = action_of("打开 /Users/me/x.pdf");
        assert!(matches!(
            &fa.target_ref,
            TargetRef::Path { value } if value == "/Users/me/x.pdf"
        ));
    }

    #[test]
    fn external_drive_destination() {
        let fa = action_of("copy items 1, 3 and 5 to my external drive");
        assert_eq!(fa.destination.as_deref(), Some("/Volumes/External"));
        assert!(matches!(
            &fa.target_ref,
            TargetRef::LastResults {
                selector: TargetSelector::Indices { values }
            } if values == &[1, 3, 5]
        ));
    }
}
