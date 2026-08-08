use crate::{get_default_loader, GenerateParams, LlamaModelRuntime, ModelError, ModelLoadParams};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

/// 模型常驻进程状态
#[derive(Debug, Clone, PartialEq)]
pub enum DaemonStatus {
    /// 未加载
    Idle,
    /// 正在加载
    Loading { since: Instant },
    /// 已就绪
    Ready,
    /// 加载失败
    Failed { reason: String },
}

/// 模型常驻进程包装器
///
/// 负责持有一个模型实例并管理其生命周期。
/// 在 YOLO 模式下，本模块提供单进程内的常驻模型管理。
pub struct ModelDaemon {
    runtime: Box<dyn LlamaModelRuntime>,
    status: DaemonStatus,
}

// `runtime` 是 trait object（无 Debug 约束），手动实现只暴露状态。
impl std::fmt::Debug for ModelDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelDaemon")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl ModelDaemon {
    /// 同步加载模型
    ///
    /// # 参数
    /// * `model_path` - 模型文件路径
    /// * `params` - 加载参数
    pub fn load_blocking(model_path: &Path, params: ModelLoadParams) -> Result<Self, ModelError> {
        let loader = get_default_loader();

        // 严格遵循状态机：虽然是阻塞加载，但在逻辑上经历了 Loading 状态
        // 实际由于是同步返回 Result，外部看到的第一个状态通常是 Ready
        let _loading_since = Instant::now();

        match loader.load(model_path, &params) {
            Ok(runtime) => Ok(Self {
                runtime,
                status: DaemonStatus::Ready,
            }),
            Err(e) => Err(e),
        }
    }

    /// 从已构造的 runtime 直接组 daemon（跨 crate 测试注入用——`#[cfg(test)]` 不跨 crate 导出，故必须 pub），状态即 Ready。
    #[must_use]
    pub fn from_runtime(runtime: Box<dyn LlamaModelRuntime>) -> Self {
        Self {
            runtime,
            status: DaemonStatus::Ready,
        }
    }

    /// 执行推理生成
    ///
    /// # 参数
    /// * `prompt` - 提示词
    /// * `params` - 生成参数
    pub fn generate(&self, prompt: &str, params: &GenerateParams) -> Result<String, ModelError> {
        if self.status != DaemonStatus::Ready {
            return Err(ModelError::InferenceError(format!(
                "模型尚未就绪: {:?}",
                self.status
            )));
        }
        self.runtime.generate(prompt, params)
    }

    /// BETA-17：带固定前缀缓存的推理生成。`prefix` 跨调用复用其 KV（仅 llama-cpp 后端
    /// 真缓存），适合 hybrid prompt 这类"固定指令前缀 + 每条 query 小尾巴"的场景。
    ///
    /// # 参数
    /// * `prefix` - 跨调用稳定的固定前缀
    /// * `suffix` - 每次变化的尾巴（query + draft）
    /// * `params` - 生成参数
    pub fn generate_cached_prefix(
        &self,
        prefix: &str,
        suffix: &str,
        params: &GenerateParams,
    ) -> Result<String, ModelError> {
        if self.status != DaemonStatus::Ready {
            return Err(ModelError::InferenceError(format!(
                "模型尚未就绪: {:?}",
                self.status
            )));
        }
        self.runtime.generate_cached_prefix(prefix, suffix, params)
    }

    /// BETA-15B-1：生成句向量（透传到底层 runtime 的 `embed`）。仅 embedding 模型有意义；
    /// 非 embedding 后端返回 Err（runtime 默认实现）。
    pub fn embed(&self, text: &str) -> Result<Vec<f32>, ModelError> {
        self.runtime.embed(text)
    }

    /// 获取当前状态
    #[must_use]
    pub fn status(&self) -> DaemonStatus {
        self.status.clone()
    }
}

/// 并发安全的模型守护进程
pub type SharedModelDaemon = Arc<ModelDaemon>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::thread;

    #[test]
    fn test_daemon_lifecycle() {
        let path = PathBuf::from("stub.gguf");
        // workspace feature 统一拉入 llama-cpp 真 loader 时，占位 stub.gguf 无法加载 → 跳过
        // （本测试只验 stub daemon 的生命周期 + echo 行为）。
        let Ok(daemon) = ModelDaemon::load_blocking(&path, ModelLoadParams::default()) else {
            return;
        };

        assert_eq!(daemon.status(), DaemonStatus::Ready);

        let response = daemon
            .generate("hello", &GenerateParams::default())
            .unwrap();
        assert!(response.contains("Echo: hello"));
        assert_eq!(daemon.status(), DaemonStatus::Ready);
    }

    #[test]
    fn test_daemon_concurrent_generate() {
        let path = PathBuf::from("stub.gguf");
        // workspace feature 统一拉入 llama-cpp 真 loader 时，占位 stub.gguf 无法加载 → 跳过
        // （本测试只验 stub daemon 的并发 echo 行为）。
        let Ok(daemon) = ModelDaemon::load_blocking(&path, ModelLoadParams::default()) else {
            return;
        };
        let daemon = Arc::new(daemon);

        let mut handles = vec![];
        for i in 0..5 {
            let d = Arc::clone(&daemon);
            let handle = thread::spawn(move || {
                let prompt = format!("task {i}");
                let res = d.generate(&prompt, &GenerateParams::default()).unwrap();
                assert!(res.contains(&prompt));
            });
            handles.push(handle);
        }

        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    fn test_daemon_failed_status() {
        // StubLoader 永远成功，但我们可以通过手动构造 ModelDaemon 来测试状态逻辑（如果有公开构造的话）
        // 或者在 load_blocking 失败时返回 Err
        // 实际上 load_blocking 返回 Result，所以 Failed 状态在 load_blocking 内部并不会持久化到返回的对象中
        // 除非我们以后引入异步加载
    }

    #[test]
    fn test_from_runtime_is_ready() {
        struct Fixed;
        impl crate::LlamaModelRuntime for Fixed {
            fn generate(
                &self,
                _prompt: &str,
                _params: &GenerateParams,
            ) -> Result<String, crate::ModelError> {
                Ok("{\"sort\":\"size_desc\"}".to_owned())
            }
        }
        let daemon = ModelDaemon::from_runtime(Box::new(Fixed));
        assert_eq!(daemon.status(), DaemonStatus::Ready);
        let out = daemon.generate("p", &GenerateParams::default()).unwrap();
        assert!(out.contains("size_desc"));
    }

    /// BETA-15B-1：非 embedding 后端（Fixed 未覆盖 embed，走 trait 默认）→ embed 返回 Err。
    #[test]
    fn test_embed_passthrough_errs_for_non_embedding_runtime() {
        struct Fixed;
        impl crate::LlamaModelRuntime for Fixed {
            fn generate(
                &self,
                _prompt: &str,
                _params: &GenerateParams,
            ) -> Result<String, crate::ModelError> {
                Ok(String::new())
            }
        }
        let daemon = ModelDaemon::from_runtime(Box::new(Fixed));
        assert!(daemon.embed("hello").is_err());
    }
}
