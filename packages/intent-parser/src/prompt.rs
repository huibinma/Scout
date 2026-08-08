use scout_search_backend::SearchIntent;

/// 示例对 (Few-Shot)
#[derive(Debug, Clone)]
pub struct FewShot {
    pub query: String,
    pub expected_intent: String,
}

/// Prompt 构建器
#[derive(Debug, Clone)]
pub struct PromptBuilder {
    few_shots: Vec<FewShot>,
}

impl PromptBuilder {
    /// 创建一个新的 PromptBuilder 并初始化 few-shots
    pub fn new() -> Self {
        let few_shots = vec![
            // 1. file_search (中文)
            FewShot {
                query: "查找昨天编辑过的 ppt".to_string(),
                expected_intent: r#"{"schema_version":"1.0","intent":"file_search","language":"zh","extensions":["ppt","pptx"],"file_type":"presentation","modified_time":{"type":"relative","value":"yesterday"},"sort":"modified_desc"}"#.to_string(),
            },
            // 2. file_search (中文，带位置和大小)
            FewShot {
                query: "找下载目录中大于 100MB 的视频".to_string(),
                expected_intent: r#"{"schema_version":"1.0","intent":"file_search","language":"zh","file_type":"video","location":{"hint":"下载"},"size":{"type":"greater_than","value":100,"unit":"MB"},"sort":"size_desc"}"#.to_string(),
            },
            // 3. media_search (音乐)
            FewShot {
                query: "找一首周华健的歌".to_string(),
                expected_intent: r#"{"schema_version":"1.0","intent":"media_search","language":"zh","media_type":"audio","artist":"周华健","extensions":["mp3","flac","wav","m4a","ape","ogg","aac","wma","aiff"],"sort":"relevance_desc"}"#.to_string(),
            },
            // 4. media_search (截图)
            FewShot {
                query: "找我昨天截的付款二维码".to_string(),
                expected_intent: r#"{"schema_version":"1.0","intent":"media_search","language":"zh","media_type":"screenshot","keywords":["付款","二维码"],"created_time":{"type":"relative","value":"yesterday"},"sort":"created_desc"}"#.to_string(),
            },
            // 5. file_action (打开)
            FewShot {
                query: "打开第三个".to_string(),
                expected_intent: r#"{"schema_version":"1.0","intent":"file_action","language":"zh","action":"open","target_ref":{"source":"last_results","selector":{"type":"index","value":3}},"requires_confirmation":false}"#.to_string(),
            },
            // 6. file_action (重命名)
            FewShot {
                query: "把第三个改名为 final".to_string(),
                expected_intent: r#"{"schema_version":"1.0","intent":"file_action","language":"zh","action":"rename","target_ref":{"source":"last_results","selector":{"type":"index","value":3}},"new_name":"final","requires_confirmation":true}"#.to_string(),
            },
            // 7. refine (只看)
            FewShot {
                query: "只看 pdf".to_string(),
                expected_intent: r#"{"schema_version":"1.0","intent":"refine","language":"zh","base_ref":"last_intent","delta":{"extensions":["pdf"],"file_type":"document"}}"#.to_string(),
            },
            // 8. refine (排除)
            FewShot {
                query: "排除视频".to_string(),
                expected_intent: r#"{"schema_version":"1.0","intent":"refine","language":"zh","base_ref":"last_intent","delta":{"exclude_file_type":["video"]}}"#.to_string(),
            },
            // 9. clarify (模糊)
            FewShot {
                query: "找最近的".to_string(),
                expected_intent: r#"{"schema_version":"1.0","intent":"clarify","language":"zh","reason":"ambiguous_time","question":"你说的「最近」是指最近几天？","options":["今天","过去 3 天","过去一周","过去一个月"]}"#.to_string(),
            },
            // 10. file_search (英文)
            FewShot {
                query: "find Excel modified in the past 7 days".to_string(),
                expected_intent: r#"{"schema_version":"1.0","intent":"file_search","language":"en","extensions":["xls","xlsx"],"file_type":"spreadsheet","modified_time":{"type":"relative","value":"last_7_days"},"sort":"modified_desc"}"#.to_string(),
            },
        ];
        Self { few_shots }
    }

    /// 获取系统提示词
    pub fn system_prompt(&self) -> String {
        r#"你是一个本地搜索意图解析专家。你的任务是将用户的自然语言输入转换为符合特定 JSON Schema 的 SearchIntent 对象。

约束规则：
1. 必须输出且仅输出合法的 JSON 字符串，不要包含任何 Markdown 格式（如 ```json）。
2. schema_version 必须固定为 "1.0"。
3. intent 必须是 "file_search", "media_search", "file_action", "refine", "clarify" 之一。
4. 语言识别：中文识别为 "zh"，英文识别为 "en"，混合识别为 "mixed"。
5. 时间表达：使用相对时间语义（如 "yesterday", "last_7_days"），不要计算具体日期。
6. 媒体搜索：如果是关于音乐、视频、截图的搜索，请使用 "media_search"。
7. 多轮对话：如果用户是在上一轮结果基础上进行筛选（如“只看...”、“排除...”），请使用 "refine"。
8. 模糊意图：如果输入过于模糊无法解析，请使用 "clarify"。
9. 安全：对于高风险操作（如删除、批量移动），必须触发 "clarify" 或设置 requires_confirmation 为 true。

输出格式示例：
{"schema_version":"1.0","intent":"file_search",...}
"#.to_string()
    }

    /// 获取用户提示词
    pub fn user_prompt(&self, query: &str) -> String {
        let mut prompt = String::new();
        prompt.push_str("以下是一些示例：\n\n");
        for shot in &self.few_shots {
            prompt.push_str(&format!(
                "输入：{}\n输出：{}\n\n",
                shot.query, shot.expected_intent
            ));
        }
        prompt.push_str(&format!("现在请解析以下输入：\n\n输入：{}\n输出：", query));
        prompt
    }

    /// 获取所有 few-shots
    pub fn few_shots(&self) -> &[FewShot] {
        &self.few_shots
    }
}

impl Default for PromptBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    #[test]
    fn test_few_shots_deserialization() {
        let builder = PromptBuilder::new();
        for shot in builder.few_shots() {
            let res: Result<SearchIntent, _> = serde_json::from_str(&shot.expected_intent);
            assert!(
                res.is_ok(),
                "Few-shot JSON 无法反序列化: {} -> {}",
                shot.query,
                shot.expected_intent
            );
        }
    }

    #[test]
    fn test_few_shots_coverage() {
        let builder = PromptBuilder::new();
        let shots = builder.few_shots();

        let has_file_search = shots
            .iter()
            .any(|s| s.expected_intent.contains("\"intent\":\"file_search\""));
        let has_media_search = shots
            .iter()
            .any(|s| s.expected_intent.contains("\"intent\":\"media_search\""));
        let has_file_action = shots
            .iter()
            .any(|s| s.expected_intent.contains("\"intent\":\"file_action\""));
        let has_refine = shots
            .iter()
            .any(|s| s.expected_intent.contains("\"intent\":\"refine\""));
        let has_clarify = shots
            .iter()
            .any(|s| s.expected_intent.contains("\"intent\":\"clarify\""));

        assert!(has_file_search);
        assert!(has_media_search);
        assert!(has_file_action);
        assert!(has_refine);
        assert!(has_clarify);
    }

    #[test]
    fn test_system_prompt_length() {
        let builder = PromptBuilder::new();
        let prompt = builder.system_prompt();
        assert!(prompt.len() < 2000);
    }
}
