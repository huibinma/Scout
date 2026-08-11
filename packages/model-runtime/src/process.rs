//! Windows llama.cpp 故障隔离进程。
//!
//! `llama.cpp` 是 C/C++ FFI：访问违规、`abort()`、fail-fast/SEH 都会绕过 Rust
//! `Result`/`catch_unwind`，若直接运行在 `scout-desktop.exe` 内就会带走整个 UI。这里复用
//! 当前可执行文件作为一个长驻 helper，并以 JSON-lines 传输 load/generate/embed 请求。
//! 模型仍只加载一次；helper 若崩溃，父进程把 EOF 转成 `ModelError` 并走现有降级路径。

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::llama::LlamaLoader;
use crate::{GenerateParams, LlamaModelRuntime, ModelError, ModelLoadParams, ModelLoader};

const MODEL_WORKER_ARG: &str = "--scout-model-worker";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum WorkerRequest {
    Load {
        model_path: PathBuf,
        params: ModelLoadParams,
    },
    Generate {
        prompt: String,
        params: GenerateParams,
    },
    GenerateCached {
        prefix: String,
        suffix: String,
        params: GenerateParams,
    },
    Embed {
        text: String,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
enum WorkerResponse {
    Ready,
    Text(String),
    Embedding(Vec<f32>),
    Error(String),
}

/// Windows production loader。构造本身不触碰 llama.cpp；真正的 backend init/load 在
/// helper 内完成，避免初始化阶段的 native crash 落到桌面进程。
#[derive(Debug, Default)]
pub(crate) struct ProcessLlamaLoader;

impl ProcessLlamaLoader {
    pub(crate) const fn new() -> Self {
        Self
    }
}

impl ModelLoader for ProcessLlamaLoader {
    fn load(
        &self,
        path: &Path,
        params: &ModelLoadParams,
    ) -> Result<Box<dyn LlamaModelRuntime>, ModelError> {
        Ok(Box::new(ProcessLlamaRuntime::spawn(path, *params)?))
    }
}

#[derive(Debug)]
struct ProcessLlamaRuntime {
    session: Mutex<ChildSession>,
}

impl ProcessLlamaRuntime {
    fn spawn(path: &Path, params: ModelLoadParams) -> Result<Self, ModelError> {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let exe = std::env::current_exe()
            .map_err(|error| ModelError::LoadError(format!("定位模型隔离 helper 失败: {error}")))?;
        let mut child = Command::new(exe)
            .arg(MODEL_WORKER_ARG)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // llama.cpp 的诊断通常写 stderr；继承父句柄既不会堵塞 pipe，也尽量保留线索。
            .stderr(Stdio::inherit())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|error| {
                ModelError::LoadError(format!("启动模型隔离 helper 失败: {error}"))
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ModelError::LoadError("模型隔离 helper 缺少 stdin 管道".to_owned()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ModelError::LoadError("模型隔离 helper 缺少 stdout 管道".to_owned()))?;
        let mut session = ChildSession {
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
        };
        match session.exchange(&WorkerRequest::Load {
            model_path: path.to_path_buf(),
            params,
        })? {
            WorkerResponse::Ready => Ok(Self {
                session: Mutex::new(session),
            }),
            WorkerResponse::Error(message) => Err(ModelError::LoadError(message)),
            response => Err(ModelError::LoadError(format!(
                "模型隔离 helper 返回了错误的加载响应: {response:?}"
            ))),
        }
    }

    fn request(&self, request: &WorkerRequest) -> Result<WorkerResponse, ModelError> {
        self.session
            .lock()
            .map_err(|_| ModelError::InferenceError("模型隔离 helper 会话锁已损坏".to_owned()))?
            .exchange(request)
    }
}

impl LlamaModelRuntime for ProcessLlamaRuntime {
    fn generate(&self, prompt: &str, params: &GenerateParams) -> Result<String, ModelError> {
        response_text(self.request(&WorkerRequest::Generate {
            prompt: prompt.to_owned(),
            params: params.clone(),
        })?)
    }

    fn generate_cached_prefix(
        &self,
        prefix: &str,
        suffix: &str,
        params: &GenerateParams,
    ) -> Result<String, ModelError> {
        response_text(self.request(&WorkerRequest::GenerateCached {
            prefix: prefix.to_owned(),
            suffix: suffix.to_owned(),
            params: params.clone(),
        })?)
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>, ModelError> {
        match self.request(&WorkerRequest::Embed {
            text: text.to_owned(),
        })? {
            WorkerResponse::Embedding(value) => Ok(value),
            WorkerResponse::Error(message) => Err(ModelError::InferenceError(message)),
            response => Err(ModelError::InferenceError(format!(
                "模型隔离 helper 返回了错误的 embedding 响应: {response:?}"
            ))),
        }
    }
}

fn response_text(response: WorkerResponse) -> Result<String, ModelError> {
    match response {
        WorkerResponse::Text(value) => Ok(value),
        WorkerResponse::Error(message) => Err(ModelError::InferenceError(message)),
        other => Err(ModelError::InferenceError(format!(
            "模型隔离 helper 返回了错误的生成响应: {other:?}"
        ))),
    }
}

#[derive(Debug)]
struct ChildSession {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl ChildSession {
    fn exchange(&mut self, request: &WorkerRequest) -> Result<WorkerResponse, ModelError> {
        serde_json::to_writer(&mut self.stdin, request).map_err(|error| {
            ModelError::InferenceError(format!("编码模型 helper 请求失败: {error}"))
        })?;
        self.stdin.write_all(b"\n").map_err(|error| {
            ModelError::InferenceError(format!("写入模型 helper 请求失败: {error}"))
        })?;
        self.stdin.flush().map_err(|error| {
            ModelError::InferenceError(format!("刷新模型 helper 请求失败: {error}"))
        })?;

        let mut line = String::new();
        let read = self.stdout.read_line(&mut line).map_err(|error| {
            ModelError::InferenceError(format!("读取模型 helper 响应失败: {error}"))
        })?;
        if read == 0 {
            let exit = self
                .child
                .try_wait()
                .ok()
                .flatten()
                .map_or_else(|| "未知（管道已关闭）".to_owned(), |s| s.to_string());
            return Err(ModelError::InferenceError(format!(
                "模型隔离 helper 意外退出（{exit}）；已阻止原生崩溃波及 Scout Desktop"
            )));
        }
        serde_json::from_str(line.trim_end()).map_err(|error| {
            ModelError::InferenceError(format!("解析模型 helper 响应失败: {error}"))
        })
    }
}

impl Drop for ChildSession {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        if serde_json::to_writer(&mut self.stdin, &WorkerRequest::Shutdown).is_ok() {
            let _ = self.stdin.write_all(b"\n");
            let _ = self.stdin.flush();
        }
        let _ = self.child.wait();
    }
}

fn write_response(
    stdout: &mut BufWriter<std::io::StdoutLock<'_>>,
    response: &WorkerResponse,
) -> bool {
    serde_json::to_writer(&mut *stdout, response).is_ok()
        && stdout.write_all(b"\n").is_ok()
        && stdout.flush().is_ok()
}

fn inference_response<T>(
    result: Result<T, ModelError>,
    wrap: impl FnOnce(T) -> WorkerResponse,
) -> WorkerResponse {
    match result {
        Ok(value) => wrap(value),
        Err(error) => WorkerResponse::Error(error.to_string()),
    }
}

/// 命中内部参数时进入 helper loop 并直接退出进程；普通启动立即返回。
pub(crate) fn run_if_requested() {
    let mut args = std::env::args_os();
    let _exe = args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new(MODEL_WORKER_ARG)) {
        return;
    }

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = BufReader::new(stdin.lock());
    let mut output = BufWriter::new(stdout.lock());
    let mut line = String::new();
    if input.read_line(&mut line).ok().filter(|n| *n > 0).is_none() {
        std::process::exit(2);
    }
    let load = serde_json::from_str::<WorkerRequest>(line.trim_end());
    let (model_path, params) = match load {
        Ok(WorkerRequest::Load { model_path, params }) => (model_path, params),
        Ok(_) => {
            let _ = write_response(
                &mut output,
                &WorkerResponse::Error("模型 helper 首条请求必须是 load".to_owned()),
            );
            std::process::exit(3);
        }
        Err(error) => {
            let _ = write_response(
                &mut output,
                &WorkerResponse::Error(format!("解析模型 load 请求失败: {error}")),
            );
            std::process::exit(3);
        }
    };

    // 只在隔离进程内初始化 C++ backend。这里的 native abort 最多杀掉 helper。
    let runtime = LlamaLoader::new().and_then(|loader| loader.load(&model_path, &params));
    let runtime = match runtime {
        Ok(runtime) => {
            if !write_response(&mut output, &WorkerResponse::Ready) {
                std::process::exit(4);
            }
            runtime
        }
        Err(error) => {
            let _ = write_response(&mut output, &WorkerResponse::Error(error.to_string()));
            std::process::exit(5);
        }
    };

    loop {
        line.clear();
        if input.read_line(&mut line).ok().filter(|n| *n > 0).is_none() {
            break;
        }
        let request = match serde_json::from_str::<WorkerRequest>(line.trim_end()) {
            Ok(request) => request,
            Err(error) => {
                if !write_response(
                    &mut output,
                    &WorkerResponse::Error(format!("解析模型请求失败: {error}")),
                ) {
                    break;
                }
                continue;
            }
        };
        let response = match request {
            WorkerRequest::Generate { prompt, params } => {
                inference_response(runtime.generate(&prompt, &params), WorkerResponse::Text)
            }
            WorkerRequest::GenerateCached {
                prefix,
                suffix,
                params,
            } => inference_response(
                runtime.generate_cached_prefix(&prefix, &suffix, &params),
                WorkerResponse::Text,
            ),
            WorkerRequest::Embed { text } => {
                inference_response(runtime.embed(&text), WorkerResponse::Embedding)
            }
            WorkerRequest::Shutdown => break,
            WorkerRequest::Load { .. } => {
                WorkerResponse::Error("模型 helper 已加载，拒绝重复 load".to_owned())
            }
        };
        if !write_response(&mut output, &response) {
            break;
        }
    }
    // runtime 在 helper 内完整 drop；即使 C++ 静态析构 abort，也不会影响父进程。
    drop(runtime);
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn protocol_roundtrip_preserves_unicode_and_params() {
        let request = WorkerRequest::GenerateCached {
            prefix: "查找：".to_owned(),
            suffix: r"C:\\资料\\计划.gguf".to_owned(),
            params: GenerateParams::default(),
        };
        let json = serde_json::to_string(&request).unwrap();
        let decoded: WorkerRequest = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, WorkerRequest::GenerateCached { .. }));
    }

    #[test]
    fn response_roundtrip_preserves_embedding() {
        let response = WorkerResponse::Embedding(vec![0.25, -0.5, 1.0]);
        let json = serde_json::to_string(&response).unwrap();
        let decoded: WorkerResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, WorkerResponse::Embedding(v) if v.len() == 3));
    }
}
