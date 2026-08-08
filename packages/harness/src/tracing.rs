use crate::{SupportedIntent, ToolKind};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 工具调用事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallEvent {
    /// 工具唯一标识。
    pub tool_id: String,
    /// 工具种类。
    pub tool_kind: ToolKind,
    /// 对应的 Intent 变体（轻量化）。
    pub intent_variant: SupportedIntent,
}

/// 工具返回结果事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultEvent {
    /// 工具唯一标识。
    pub tool_id: String,
    /// 调用总耗时。
    pub duration: Duration,
    /// 结果数量（不含结果正文）。
    pub result_count: usize,
}

/// 工具调用错误事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolErrorEvent {
    /// 工具唯一标识。
    pub tool_id: String,
    /// 调用总耗时。
    pub duration: Duration,
    /// 错误分类（如 "`SearchError::Timeout`"）。
    pub error_type: String,
}

/// 同义词扩展事件：记录单次关键词扩展的输入、产出及来源。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SynonymExpandEvent {
    /// 被扩展的原始词头（用户关键词）。
    pub head: String,
    /// 扩展后的词组（head 在 [0]，其余为同义词）。
    pub group: Vec<String>,
    /// 词典来源，如 "zh.yaml" / "en.yaml" / "noop"。
    pub source: String,
    /// 运行期 cap 是否触发（超出上限被截断）。
    pub truncated: bool,
}

/// Tracing 钩子接口。
pub trait TracingHook: Send + Sync {
    /// 工具开始调用时触发。
    fn on_tool_call(&self, event: &ToolCallEvent);
    /// 工具调用成功并返回结果时触发。
    fn on_tool_result(&self, event: &ToolResultEvent);
    /// 工具调用发生错误时触发。
    fn on_error(&self, event: &ToolErrorEvent);
    /// 同义词扩展事件。默认空实现，老 hook 零修改。
    fn on_synonym_expand(&self, _event: &SynonymExpandEvent) {}
}

/// Tracing 服务入口，负责分发事件到多个挂载的钩子。
pub struct Tracer {
    hooks: Vec<Box<dyn TracingHook>>,
}

impl Tracer {
    /// 使用一组钩子创建 Tracer。
    #[must_use]
    pub fn with_hooks(hooks: Vec<Box<dyn TracingHook>>) -> Self {
        Self { hooks }
    }

    /// 触发工具调用开始事件。
    pub fn on_tool_call(&self, event: &ToolCallEvent) {
        for hook in &self.hooks {
            hook.on_tool_call(event);
        }
    }

    /// 触发工具成功返回事件。
    pub fn on_tool_result(&self, event: &ToolResultEvent) {
        for hook in &self.hooks {
            hook.on_tool_result(event);
        }
    }

    /// 触发工具错误事件。
    pub fn on_error(&self, event: &ToolErrorEvent) {
        for hook in &self.hooks {
            hook.on_error(event);
        }
    }

    /// 触发同义词扩展事件。
    pub fn on_synonym_expand(&self, event: &SynonymExpandEvent) {
        for hook in &self.hooks {
            hook.on_synonym_expand(event);
        }
    }
}

impl std::fmt::Debug for Tracer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracer")
            .field("hook_count", &self.hooks.len())
            .finish()
    }
}

/// 空操作钩子（测试或禁用追踪用）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopHook;

impl TracingHook for NoopHook {
    fn on_tool_call(&self, _event: &ToolCallEvent) {}
    fn on_tool_result(&self, _event: &ToolResultEvent) {}
    fn on_error(&self, _event: &ToolErrorEvent) {}
    fn on_synonym_expand(&self, _event: &SynonymExpandEvent) {}
}

/// 将追踪记录为 JSON Lines 格式的钩子。
pub struct JsonLinesHook<W: Write + Send> {
    writer: Arc<Mutex<W>>,
}

impl<W: Write + Send> JsonLinesHook<W> {
    /// 创建一个新的 `JsonLinesHook`。
    pub fn new(writer: W) -> Self {
        Self {
            writer: Arc::new(Mutex::new(writer)),
        }
    }

    fn log<T: Serialize>(&self, tag: &str, data: &T) {
        if let Ok(mut w) = self.writer.lock() {
            let entry = serde_json::json!({
                "tag": tag,
                "data": data,
                "timestamp": Utc::now(),
            });
            if let Ok(line) = serde_json::to_string(&entry) {
                let _ = writeln!(w, "{line}");
            }
        }
    }
}

impl<W: Write + Send> TracingHook for JsonLinesHook<W> {
    fn on_tool_call(&self, event: &ToolCallEvent) {
        self.log("tool_call", event);
    }
    fn on_tool_result(&self, event: &ToolResultEvent) {
        self.log("tool_result", event);
    }
    fn on_error(&self, event: &ToolErrorEvent) {
        self.log("tool_error", event);
    }
    fn on_synonym_expand(&self, event: &SynonymExpandEvent) {
        self.log("synonym_expand", event);
    }
}

impl<W: Write + Send> std::fmt::Debug for JsonLinesHook<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonLinesHook").finish()
    }
}

/// 路径脱敏工具：仅保留最后两级目录/文件。
/// 符合 CONVENTIONS §7：不记录完整路径。
#[must_use]
pub fn anonymize_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        normalized
    } else {
        format!(".../{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::print_stdout
    )]
    use super::*;
    use std::sync::Arc;

    #[derive(Default)]
    struct MockHook {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl TracingHook for MockHook {
        fn on_tool_call(&self, event: &ToolCallEvent) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("call:{}", event.tool_id));
        }
        fn on_tool_result(&self, event: &ToolResultEvent) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("result:{}", event.tool_id));
        }
        fn on_error(&self, event: &ToolErrorEvent) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("error:{}", event.tool_id));
        }
    }

    #[test]
    fn tracer_dispatches_to_hooks() {
        let mock = MockHook::default();
        let calls = Arc::clone(&mock.calls);
        let tracer = Tracer::with_hooks(vec![Box::new(mock)]);

        let call_evt = ToolCallEvent {
            tool_id: "test_tool".into(),
            tool_kind: ToolKind::Search,
            intent_variant: SupportedIntent::FileSearch,
        };
        tracer.on_tool_call(&call_evt);

        let result_evt = ToolResultEvent {
            tool_id: "test_tool".into(),
            duration: Duration::from_millis(100),
            result_count: 5,
        };
        tracer.on_tool_result(&result_evt);

        let err_evt = ToolErrorEvent {
            tool_id: "test_tool".into(),
            duration: Duration::from_millis(50),
            error_type: "Timeout".into(),
        };
        tracer.on_error(&err_evt);

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 3);
        assert_eq!(recorded[0], "call:test_tool");
        assert_eq!(recorded[1], "result:test_tool");
        assert_eq!(recorded[2], "error:test_tool");
    }

    #[test]
    fn json_lines_hook_output_is_valid() {
        let buf = Vec::new();
        let hook = JsonLinesHook::new(buf);
        let event = ToolCallEvent {
            tool_id: "search.spotlight".into(),
            tool_kind: ToolKind::Search,
            intent_variant: SupportedIntent::FileSearch,
        };
        hook.on_tool_call(&event);

        let output = String::from_utf8(hook.writer.lock().unwrap().clone()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["tag"], "tool_call");
        assert_eq!(parsed["data"]["tool_id"], "search.spotlight");
        assert!(parsed["timestamp"].is_string());
    }

    #[test]
    fn tracer_dispatches_synonym_expand_to_hooks() {
        #[derive(Default)]
        struct MockSynonymHook {
            events: Arc<Mutex<Vec<SynonymExpandEvent>>>,
        }
        impl TracingHook for MockSynonymHook {
            fn on_tool_call(&self, _: &ToolCallEvent) {}
            fn on_tool_result(&self, _: &ToolResultEvent) {}
            fn on_error(&self, _: &ToolErrorEvent) {}
            fn on_synonym_expand(&self, event: &SynonymExpandEvent) {
                self.events.lock().unwrap().push(event.clone());
            }
        }
        let mock = MockSynonymHook::default();
        let events = Arc::clone(&mock.events);
        let tracer = Tracer::with_hooks(vec![Box::new(mock)]);

        tracer.on_synonym_expand(&SynonymExpandEvent {
            head: "工作汇报".into(),
            group: vec!["工作汇报".into(), "述职".into()],
            source: "zh.yaml".into(),
            truncated: false,
        });
        let recorded = events.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].head, "工作汇报");
        assert_eq!(recorded[0].group, vec!["工作汇报", "述职"]);
    }

    #[test]
    fn noop_hook_skips_synonym_expand() {
        // 不 panic 即可，验证 NoopHook 实现存在
        NoopHook.on_synonym_expand(&SynonymExpandEvent {
            head: "x".into(),
            group: vec!["x".into()],
            source: "zh.yaml".into(),
            truncated: false,
        });
    }

    #[test]
    fn json_lines_hook_writes_synonym_expand_event() {
        let buf: Vec<u8> = Vec::new();
        let hook = JsonLinesHook::new(buf);
        hook.on_synonym_expand(&SynonymExpandEvent {
            head: "工作汇报".into(),
            group: vec!["工作汇报".into(), "述职".into()],
            source: "zh.yaml".into(),
            truncated: false,
        });
        // writer 通过 Arc<Mutex<W>> 可读回
        let output = String::from_utf8(hook.writer.lock().unwrap().clone()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(parsed["tag"], "synonym_expand");
        assert_eq!(parsed["data"]["head"], "工作汇报");
        assert_eq!(parsed["data"]["source"], "zh.yaml");
        assert_eq!(parsed["data"]["truncated"], false);
    }

    #[test]
    fn path_anonymization_works() {
        // Unix style
        assert_eq!(
            anonymize_path("/Users/alice/Work/project/src/lib.rs"),
            ".../src/lib.rs"
        );
        // Windows style
        assert_eq!(
            anonymize_path(r"C:\Users\alice\Documents\test.txt"),
            ".../Documents/test.txt"
        );
        // Short path
        assert_eq!(anonymize_path("/etc/hosts"), "/etc/hosts");
        // Mixed style
        assert_eq!(
            anonymize_path("/Users/alice\\Desktop/file.png"),
            ".../Desktop/file.png"
        );

        // Verify it doesn't contain home/user if enough segments exist
        let secret_path = "/Users/secret_user/Work/private/data.csv";
        let anon = anonymize_path(secret_path);
        assert!(!anon.contains("secret_user"));
        assert!(!anon.contains("Users"));
        assert_eq!(anon, ".../private/data.csv");
    }
}
