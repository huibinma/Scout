//! llama.cpp 后端运行时。
//!
//! BETA-17：`LlamaContext` 是 `!Send + !Sync`（含裸 `NonNull`），无法放进跨线程共享的
//! `Mutex`。而 `LlamaModelRuntime` 必须 `Send + Sync`（`SharedModelDaemon = Arc<..>` 跨线程
//! 共享）。因此本模块用**专用推理线程**：worker 独占 model + 常驻 context + prefix KV 缓存，
//! 结构体只持 `Mutex<Sender<Request>>`。!Send 的 context 永不跨线程，prefix 的 KV 在多次
//! `generate_cached_prefix` 调用间复用（固定指令前缀只 prefill 一次），大幅降低弱硬件延迟。

#[cfg(feature = "llama-cpp")]
use crate::{
    first_json_object_complete, GenerateParams, LlamaModelRuntime, ModelError, ModelLoadParams,
    ModelLoader,
};
#[cfg(feature = "llama-cpp")]
use llama_cpp_4::context::params::{LlamaContextParams, LlamaPoolingType};
#[cfg(feature = "llama-cpp")]
use llama_cpp_4::context::LlamaContext;
#[cfg(feature = "llama-cpp")]
use llama_cpp_4::llama_backend::LlamaBackend;
#[cfg(feature = "llama-cpp")]
use llama_cpp_4::llama_batch::LlamaBatch;
#[cfg(feature = "llama-cpp")]
use llama_cpp_4::model::params::LlamaModelParams;
#[cfg(feature = "llama-cpp")]
use llama_cpp_4::model::{AddBos, LlamaModel, Special};
#[cfg(feature = "llama-cpp")]
use llama_cpp_4::sampling::LlamaSampler;
#[cfg(feature = "llama-cpp")]
use llama_cpp_4::token::LlamaToken;
#[cfg(feature = "llama-cpp")]
use std::path::{Path, PathBuf};
#[cfg(feature = "llama-cpp")]
use std::sync::mpsc::{channel, Receiver, Sender};
#[cfg(feature = "llama-cpp")]
use std::sync::{Arc, Mutex};
#[cfg(feature = "llama-cpp")]
use std::thread::JoinHandle;

#[cfg(feature = "llama-cpp")]
#[derive(Debug)]
pub struct LlamaLoader {
    backend: Arc<LlamaBackend>,
}

#[cfg(feature = "llama-cpp")]
impl LlamaLoader {
    pub fn new() -> Result<Self, ModelError> {
        let backend = LlamaBackend::init()
            .map_err(|e| ModelError::BackendError(format!("Failed to init llama backend: {e}")))?;
        Ok(Self {
            backend: Arc::new(backend),
        })
    }
}

#[cfg(feature = "llama-cpp")]
impl ModelLoader for LlamaLoader {
    fn load(
        &self,
        path: &Path,
        params: &ModelLoadParams,
    ) -> Result<Box<dyn LlamaModelRuntime>, ModelError> {
        let context_size = if params.context_size > 0 {
            params.context_size
        } else {
            2048
        };
        let runtime =
            LlamaModelImpl::spawn(self.backend.clone(), path, params.gpu_layers, context_size)?;
        Ok(Box::new(runtime))
    }
}

/// 发给 worker 线程的请求。`reply` 用一次性 channel 回送结果。
#[cfg(feature = "llama-cpp")]
enum Request {
    Generate {
        prompt: String,
        params: GenerateParams,
        reply: Sender<Result<String, ModelError>>,
    },
    GenerateCached {
        prefix: String,
        suffix: String,
        params: GenerateParams,
        reply: Sender<Result<String, ModelError>>,
    },
    /// BETA-26：embedding 模式，回送句向量。
    Embed {
        text: String,
        reply: Sender<Result<Vec<f32>, ModelError>>,
    },
}

/// 运行时句柄：只持 `Mutex<Option<Sender>>`（`Send + Sync`），真正的 model/context 在
/// worker 线程。`req_tx` 用 `Option` 包一层（同 `handle` 字段）：`Drop` 里 `take()` 直接
/// 拿走真正的 `Sender` 扔掉即可关闭 channel，不必再像早期实现那样为了"换出一个可扔的
/// 值"临时造一对根本用不到的 dummy channel。
#[cfg(feature = "llama-cpp")]
#[derive(Debug)]
pub struct LlamaModelImpl {
    req_tx: Mutex<Option<Sender<Request>>>,
    // worker 线程句柄；`Drop` 显式 join（见下），故用 `Option` 供 `drop(&mut self)` 里
    // `take()` 出来消费。
    handle: Option<JoinHandle<()>>,
}

/// BETA-64 T9a（2026-07-25）：显式 join worker 线程，不能再像此前那样让 `JoinHandle`
/// 静默 drop（`JoinHandle` 的 `Drop` 既不 join 也不 detach——线程继续在后台跑，drop 本身
/// 不等它）。**Why**：T9a 前 `run_embed` 每次调用都在函数体内创建、使用、drop 同一个
/// embed context，drop 早在 `reply.send()` 之前就已经跑完，context 的生命周期被同步
/// 请求/响应边界天然框住，进程退出时不会有残留的 GPU 资源在等释放。T9a 起 embed
/// context 挪到 `worker_main` 里跨调用常驻（省掉每次创建/销毁的固定开销），只有当 worker
/// 线程真正退出主循环时才释放——但若不 join，主线程/进程可以在 worker 线程释放完
/// Metal 资源之前就继续往下跑甚至进程退出，与 ggml-metal 全局设备表在 `exit()` 时的
/// 静态析构产生竞态：真机复现为 `ggml_metal_device_free` 里
/// `GGML_ASSERT([rsets->data count] == 0)` 断言失败、进程 SIGABRT
/// （用真实 Qwen3-Embedding-0.6B gguf + Metal 后端复现：同一份三次 `embed()` 调用测试，
/// 加 T9a 前无此崩溃、加 T9a 后稳定复现；显式 join 后复现消失）。`drop` 里 join 阻塞到
/// worker 线程完整退出（含释放常驻 embed context/session/model），之后调用方（`Drop::drop`
/// 返回后）才继续，杜绝这个竞态窗口。
#[cfg(feature = "llama-cpp")]
impl Drop for LlamaModelImpl {
    fn drop(&mut self) {
        // 显式关闭 channel（哪怕 `req_tx` 字段本身随本结构体一起 drop，也要在这里先手动
        // 关掉——`Drop::drop` 内 `self` 字段尚未被自动 drop，若不主动 `take()` 出去 drop
        // 掉真正的 `Sender`，worker 线程的 `req_rx.recv()` 会一直阻塞、`join()` 永不返回）。
        // `unwrap_or_else(PoisonError::into_inner)` 而非 `if let Ok(...)`：与 `dispatch`/
        // `embed` 对同一把锁的取法一致——若锁曾因某次 panic 被污染，仍要拿到内部数据把
        // channel 关掉，不能因为"锁曾经脏过"就放弃关闭，那样下面的 `join()` 会永久阻塞
        // （worker 线程的 `recv()` 永远等不到 channel 关闭信号）。
        let mut guard = self
            .req_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(guard.take());
        drop(guard);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// worker 线程栈大小。llama.cpp 推理本身是逐层循环、无已知无界递归，但 worker 跑的是
/// FFI 到 llama.cpp C++ 代码（不在本仓库可审计范围内）——GBNF grammar 递归下降解析、
/// 未来切换 GPU 后端等都可能引入意外的深栈使用。默认 Rust 线程栈仅约 2MiB，这里加宽
/// 到 16MiB 做纵深防御，避免重蹈 [`crate`]（参见 packages/indexer 的 pdf-extract 栈
/// 溢出修复）覆辙：真实 `STATUS_STACK_OVERFLOW`（0xc00000fd）在 Windows 上无法被
/// `catch_unwind` 拦截，唯一防线是给够栈空间。
#[cfg(feature = "llama-cpp")]
const LLAMA_WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

#[cfg(feature = "llama-cpp")]
impl LlamaModelImpl {
    /// 启动 worker 线程并在其中加载模型；阻塞直到加载成功或失败。
    fn spawn(
        backend: Arc<LlamaBackend>,
        path: &Path,
        gpu_layers: u32,
        context_size: u32,
    ) -> Result<Self, ModelError> {
        let (req_tx, req_rx) = channel::<Request>();
        let (ready_tx, ready_rx) = channel::<Result<(), ModelError>>();
        let path_buf: PathBuf = path.to_path_buf();

        let handle = std::thread::Builder::new()
            .stack_size(LLAMA_WORKER_STACK_SIZE)
            .spawn(move || {
                worker_main(
                    &backend,
                    &path_buf,
                    gpu_layers,
                    context_size,
                    &ready_tx,
                    &req_rx,
                );
            })
            .map_err(|e| {
                ModelError::LoadError(format!("Failed to spawn llama worker thread: {e}"))
            })?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                req_tx: Mutex::new(Some(req_tx)),
                handle: Some(handle),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(ModelError::LoadError(
                "worker thread exited before model load completed".to_owned(),
            )),
        }
    }

    /// 把请求发给 worker 并阻塞等待回复。
    fn dispatch(
        &self,
        make_req: impl FnOnce(Sender<Result<String, ModelError>>) -> Request,
    ) -> Result<String, ModelError> {
        let (reply_tx, reply_rx) = channel::<Result<String, ModelError>>();
        {
            let guard = self.req_tx.lock().map_err(|_| {
                ModelError::InferenceError("worker request channel poisoned".to_owned())
            })?;
            let tx = guard.as_ref().ok_or_else(|| {
                ModelError::InferenceError("worker request channel already closed".to_owned())
            })?;
            tx.send(make_req(reply_tx))
                .map_err(|_| ModelError::InferenceError("worker thread unavailable".to_owned()))?;
        }
        reply_rx
            .recv()
            .map_err(|_| ModelError::InferenceError("worker thread dropped reply".to_owned()))?
    }
}

#[cfg(feature = "llama-cpp")]
impl LlamaModelRuntime for LlamaModelImpl {
    fn generate(&self, prompt: &str, params: &GenerateParams) -> Result<String, ModelError> {
        let prompt = prompt.to_owned();
        let params = params.clone();
        self.dispatch(move |reply| Request::Generate {
            prompt,
            params,
            reply,
        })
    }

    fn generate_cached_prefix(
        &self,
        prefix: &str,
        suffix: &str,
        params: &GenerateParams,
    ) -> Result<String, ModelError> {
        let prefix = prefix.to_owned();
        let suffix = suffix.to_owned();
        let params = params.clone();
        self.dispatch(move |reply| Request::GenerateCached {
            prefix,
            suffix,
            params,
            reply,
        })
    }

    /// BETA-26：把 embedding 请求发给 worker 并阻塞等待句向量。
    fn embed(&self, text: &str) -> Result<Vec<f32>, ModelError> {
        let text = text.to_owned();
        let (reply_tx, reply_rx) = channel::<Result<Vec<f32>, ModelError>>();
        {
            let guard = self.req_tx.lock().map_err(|_| {
                ModelError::InferenceError("worker request channel poisoned".to_owned())
            })?;
            let tx = guard.as_ref().ok_or_else(|| {
                ModelError::InferenceError("worker request channel already closed".to_owned())
            })?;
            tx.send(Request::Embed {
                text,
                reply: reply_tx,
            })
            .map_err(|_| ModelError::InferenceError("worker thread unavailable".to_owned()))?;
        }
        reply_rx
            .recv()
            .map_err(|_| ModelError::InferenceError("worker thread dropped reply".to_owned()))?
    }
}

/// worker 线程主体：加载模型，然后循环处理请求。`session` 保存常驻 context 与已 prefill
/// 的固定前缀，跨 `GenerateCached` 调用复用其 KV。`model` 先于 `session` 声明，故 `session`
/// 内借用 `model` 的 context 合法（非自引用结构体，只是后声明的局部借用先声明的局部）。
///
/// BETA-15B-8：model 加载成功后立刻 `detect_model_pooling`、`pooling` 存为函数局部变量、
/// 失败时 `ready_tx.send(Err)` 早退（与 `load_from_file` 失败同款路径）。
#[cfg(feature = "llama-cpp")]
fn worker_main(
    backend: &LlamaBackend,
    path: &Path,
    gpu_layers: u32,
    context_size: u32,
    ready_tx: &Sender<Result<(), ModelError>>,
    req_rx: &Receiver<Request>,
) {
    let mut model_params = LlamaModelParams::default();
    if gpu_layers > 0 {
        model_params = model_params.with_n_gpu_layers(gpu_layers);
    }

    let model = match LlamaModel::load_from_file(backend, path, &model_params) {
        Ok(m) => m,
        Err(e) => {
            let _ = ready_tx.send(Err(ModelError::LoadError(format!(
                "Failed to load model: {e}"
            ))));
            return;
        }
    };

    let pooling = match detect_model_pooling(&model) {
        Ok(p) => p,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    if ready_tx.send(Ok(())).is_err() {
        return; // 调用方已放弃
    }

    // 常驻前缀会话（懒初始化）。
    let mut session: Option<PrefixSession> = None;
    // BETA-64 T9a：常驻 embedding context（懒初始化，跨 `Request::Embed` 调用复用，
    // 见 `run_embed` 文档注释）。
    let mut embed_ctx: Option<LlamaContext> = None;

    while let Ok(req) = req_rx.recv() {
        match req {
            Request::Generate {
                prompt,
                params,
                reply,
            } => {
                let res = run_plain(backend, &model, context_size, &prompt, &params);
                let _ = reply.send(res);
            }
            Request::GenerateCached {
                prefix,
                suffix,
                params,
                reply,
            } => {
                let res = run_cached(
                    backend,
                    &model,
                    context_size,
                    &mut session,
                    &prefix,
                    &suffix,
                    &params,
                );
                let _ = reply.send(res);
            }
            Request::Embed { text, reply } => {
                let res = run_embed(
                    backend,
                    &model,
                    context_size,
                    pooling,
                    &mut embed_ctx,
                    &text,
                );
                let _ = reply.send(res);
            }
        }
    }
}

/// 常驻前缀会话：一个已 prefill 了固定前缀的 context。
#[cfg(feature = "llama-cpp")]
struct PrefixSession<'m> {
    ctx: LlamaContext<'m>,
    /// 已缓存的前缀文本，用于判断下次调用是否命中同一前缀。
    prefix: String,
    /// 前缀占用的 token 数（= KV 中 [0, `n_prefix`) 的位置）。
    n_prefix: i32,
}

// context_size 由调用方保证非零（`spawn` 里已将 0 替换为 2048），此处 expect 是构造期不变量。
#[cfg(feature = "llama-cpp")]
#[allow(clippy::expect_used)]
fn make_ctx_params(context_size: u32) -> LlamaContextParams {
    LlamaContextParams::default().with_n_ctx(Some(
        std::num::NonZeroU32::new(context_size).expect("context_size 非零（调用方已保证）"),
    ))
}

/// 解码一段 token 到 `ctx`，位置从 `start_pos` 起；返回该 batch（供采样读取最后一个 token 的
/// logits）。最后一个 token 置 logits=true。
#[cfg(feature = "llama-cpp")]
fn decode_segment(
    ctx: &mut LlamaContext,
    tokens: &[LlamaToken],
    start_pos: i32,
) -> Result<LlamaBatch, ModelError> {
    let mut batch = LlamaBatch::new(tokens.len().max(1), 1);
    let n = tokens.len();
    for (i, &token) in tokens.iter().enumerate() {
        let is_last = i == n - 1;
        // i 是 token 序列内的偏移，受 context_size（最大约 8 K）约束，不会超出 i32 范围。
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let pos = start_pos + i as i32;
        batch
            .add(token, pos, &[0], is_last)
            .map_err(|e| ModelError::InferenceError(format!("Failed to add token: {e}")))?;
    }
    ctx.decode(&mut batch)
        .map_err(|e| ModelError::InferenceError(format!("Failed to decode: {e}")))?;
    Ok(batch)
}

/// 普通生成：每次新建 context，prefill 整个 prompt 后采样。无前缀复用。
#[cfg(feature = "llama-cpp")]
fn run_plain(
    backend: &LlamaBackend,
    model: &LlamaModel,
    context_size: u32,
    prompt: &str,
    params: &GenerateParams,
) -> Result<String, ModelError> {
    let mut ctx = model
        .new_context(backend, make_ctx_params(context_size))
        .map_err(|e| ModelError::InferenceError(format!("Failed to create context: {e}")))?;

    let tokens = model
        .str_to_token(prompt, AddBos::Always)
        .map_err(|e| ModelError::InferenceError(format!("Failed to tokenize: {e}")))?;
    let n_tokens = tokens.len();
    let mut batch = decode_segment(&mut ctx, &tokens, 0)?;

    // n_tokens 受 context_size 约束，不会超出 i32 范围。
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let n_tokens_i32 = n_tokens as i32;
    sample_loop(&mut ctx, model, &mut batch, n_tokens_i32, params)
}

/// BETA-15B-8：从 GGUF metadata 读 `general.architecture` + `<arch>.pooling_type`，
/// 调 `pooling::detect_pooling_type` 拿正确的 `LlamaPoolingType`。
///
/// metadata 缺失时走 `pooling::default_pooling_for_arch` 启发式；未知 arch fail-fast。
/// 由 `worker_main` 在 model 加载后调一次、结果存为函数局部变量传给 `run_embed`。
///
/// `meta_val_str` 需要 `buf_size` —— `general.architecture` 值是短字符串（如 `qwen3`/`bert`/
/// `jina-bert-v2`）、`pooling_type` 是 `"0"`..=`"3"`，256 字节足够（与 llama-cpp-4 `metadata()`
/// 默认 key buf 同款）。
#[cfg(feature = "llama-cpp")]
fn detect_model_pooling(model: &LlamaModel) -> Result<LlamaPoolingType, ModelError> {
    const META_BUF: usize = 256;
    let arch = model
        .meta_val_str("general.architecture", META_BUF)
        .map_err(|e| {
            ModelError::LoadError(format!("missing GGUF metadata `general.architecture`: {e}"))
        })?;
    let key = format!("{arch}.pooling_type");
    let pooling_meta = match model.meta_val_str(&key, META_BUF) {
        Ok(s) => Some(s.parse::<i64>().map_err(|e| {
            ModelError::LoadError(format!("invalid GGUF metadata `{key}` = `{s}`: {e}"))
        })?),
        Err(_) => None,
    };
    crate::pooling::detect_pooling_type(&arch, pooling_meta)
}

/// BETA-26：embedding 模式，prefill 整段文本后取池化后的句向量。`pooling` 由
/// `worker_main` 在 model 加载后通过 `detect_model_pooling` 一次性确定（BETA-15B-8，
/// 替代之前硬编码 `LlamaPoolingType::Last` 错配 BERT 系 arch 的 bug）。llama.cpp 在
/// decode 后把池化结果写入 seq 0 的 embedding 槽，经 `embeddings_seq_ith(0)` 读取。
/// 最后做 L2 归一化，方便上层直接用点积当 cosine。
///
/// **BETA-64 T9a（2026-07-25）：常驻 context 复用**——此前每次调用都新建一个启用
/// embeddings 的专用 context、用完即弃；`embed_pending` 一轮索引通常是几百到几千篇
/// 文档，等价于几百到几千次 context 创建/销毁，是 B5 记录的固定开销主因之一。
/// 现在 context 由调用方（`worker_main`）持有的 `embed_ctx` 跨调用复用——首次调用
/// 懒创建，后续复用同一个；每次 decode **前**显式 `clear_kv_cache()`，因为 KV cache
/// 是跨 decode 累积的（`generate_cached_prefix` 的 `PrefixSession` 正是靠这个特性做
/// 前缀复用），若不清会让本次的池化结果掺进上一次调用残留在 KV cache 里的 token——
/// embedding 语义上每次都是独立短文本、不需要也不能共享 KV 状态，与生成路径的
/// `PrefixSession`（故意跨调用保留 KV）刻意相反。`model.new_context` 的 `ctx_params`
/// （`n_batch`/`n_ubatch` 提到 `context_size`）在 context 创建时一次性生效，复用同一个
/// context 意味着这组参数对整个 embedding 阶段稳定——本来就该如此（同一模型同一
/// `context_size`，不存在需要中途变化的场景）。
#[cfg(feature = "llama-cpp")]
fn run_embed<'m>(
    backend: &LlamaBackend,
    model: &'m LlamaModel,
    context_size: u32,
    pooling: LlamaPoolingType,
    embed_ctx: &mut Option<LlamaContext<'m>>,
    text: &str,
) -> Result<Vec<f32>, ModelError> {
    if embed_ctx.is_none() {
        // BETA-15B-7-v2 hotfix：BERT-arch（bge-m3 等）embedding 走 encode 路径、要求
        // `n_ubatch >= n_tokens`；llama-cpp 默认 `n_ubatch=512`，若文档 tokenize 后 > 512
        // 直接触发 `GGML_ASSERT(cparams.n_ubatch >= n_tokens)` panic。把 n_batch / n_ubatch
        // 提到 n_ctx（2048）= 与 BERT 训练 max_seq_length 同档，覆盖绝大多数真实 doc；
        // 超 n_ctx 的会在 n_ctx 处被截断、不再 panic。decoder-only（qwen3）embedding 不受影响
        // —— 它走 decode 路径不依赖 n_ubatch，但 batch/ubatch 提到 n_ctx 不改变行为，
        // qwen3-0.6b vectors.json byte-equal 已 evals 端到端验过。
        let ctx_params = make_ctx_params(context_size)
            .with_embeddings(true)
            .with_pooling_type(pooling)
            .with_n_batch(context_size)
            .with_n_ubatch(context_size);
        let ctx = model.new_context(backend, ctx_params).map_err(|e| {
            ModelError::InferenceError(format!("Failed to create embed context: {e}"))
        })?;
        *embed_ctx = Some(ctx);
    }
    // 上面刚确保过 Some，`expect` 不会命中。
    #[allow(clippy::expect_used)]
    let ctx = embed_ctx
        .as_mut()
        .expect("embed_ctx just ensured Some above");
    // 清掉上一次调用残留的 KV cache（见函数文档注释）；首次创建的 context 本就是空的，
    // clear 是幂等 no-op，不必额外分支跳过。
    ctx.clear_kv_cache();

    let tokens = model
        .str_to_token(text, AddBos::Always)
        .map_err(|e| ModelError::InferenceError(format!("Failed to tokenize: {e}")))?;
    if tokens.is_empty() {
        return Err(ModelError::InferenceError(
            "empty token sequence for embedding".to_owned(),
        ));
    }

    // 池化模式下需要每个 token 都标记 output（logits=true），llama.cpp 才会对整段做池化。
    let mut batch = LlamaBatch::new(tokens.len(), 1);
    for (i, &token) in tokens.iter().enumerate() {
        // i 不会接近 i32 上限；超长输入会在 decode 处被 n_ctx 拒绝。
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let pos = i as i32;
        batch
            .add(token, pos, &[0], true)
            .map_err(|e| ModelError::InferenceError(format!("Failed to add token: {e}")))?;
    }
    ctx.decode(&mut batch)
        .map_err(|e| ModelError::InferenceError(format!("Failed to decode: {e}")))?;

    let emb = ctx
        .embeddings_seq_ith(0)
        .map_err(|e| ModelError::InferenceError(format!("Failed to read embeddings: {e}")))?;
    let mut v = emb.to_vec();
    if v.is_empty() {
        return Err(ModelError::InferenceError(
            "empty embedding vector".to_owned(),
        ));
    }

    // L2 归一化。
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    Ok(v)
}

/// BETA-17 带前缀缓存的生成：`prefix` 的 KV 在多次调用间复用，仅 decode 变化的 `suffix`。
///
/// - 首次或 `prefix` 变化：新建 context，prefill 前缀（`AddBos::Always`），记 `n_prefix`。
/// - 命中同一前缀：`clear_kv_cache_seq` 删掉上一条 suffix 的 KV（保留 [0, `n_prefix`)），
///   再 decode 本次 suffix（`AddBos::Never`，前缀已含 BOS）。
#[cfg(feature = "llama-cpp")]
fn run_cached<'m>(
    backend: &LlamaBackend,
    model: &'m LlamaModel,
    context_size: u32,
    session: &mut Option<PrefixSession<'m>>,
    prefix: &str,
    suffix: &str,
    params: &GenerateParams,
) -> Result<String, ModelError> {
    let prefix_hit = session.as_ref().is_some_and(|s| s.prefix == prefix);

    if prefix_hit {
        // 复用前缀 KV：仅清掉上一条 suffix（位置 >= n_prefix）。
        // prefix_hit 为 true 保证 session 是 Some，此处 expect 是构造期不变量。
        #[allow(clippy::expect_used)]
        let s = session.as_mut().expect("prefix_hit 保证 session 存在");
        let keep = u32::try_from(s.n_prefix)
            .map_err(|e| ModelError::InferenceError(format!("invalid n_prefix: {e}")))?;
        s.ctx
            .clear_kv_cache_seq(Some(0), Some(keep), None)
            .map_err(|e| ModelError::InferenceError(format!("Failed to clear suffix KV: {e}")))?;
    } else {
        // 新前缀：新建 context 并 prefill 前缀。
        let mut ctx = model
            .new_context(backend, make_ctx_params(context_size))
            .map_err(|e| ModelError::InferenceError(format!("Failed to create context: {e}")))?;
        let prefix_tokens = model
            .str_to_token(prefix, AddBos::Always)
            .map_err(|e| ModelError::InferenceError(format!("Failed to tokenize prefix: {e}")))?;
        let _ = decode_segment(&mut ctx, &prefix_tokens, 0)?;
        // prefix_tokens.len() 受 context_size 约束，不会超出 i32 范围。
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let n_prefix_i32 = prefix_tokens.len() as i32;
        *session = Some(PrefixSession {
            ctx,
            prefix: prefix.to_owned(),
            n_prefix: n_prefix_i32,
        });
    }

    // 上面两个分支都确保了 session 是 Some，此处 expect 是构造期不变量。
    #[allow(clippy::expect_used)]
    let s = session.as_mut().expect("session 已在上面赋值");
    let n_prefix = s.n_prefix;

    let suffix_tokens = model
        .str_to_token(suffix, AddBos::Never)
        .map_err(|e| ModelError::InferenceError(format!("Failed to tokenize suffix: {e}")))?;
    let mut batch = decode_segment(&mut s.ctx, &suffix_tokens, n_prefix)?;
    // suffix_tokens.len() 受 context_size 约束，不会超出 i32 范围。
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let n_cur = n_prefix + suffix_tokens.len() as i32;

    sample_loop(&mut s.ctx, model, &mut batch, n_cur, params)
}

/// 自回归采样循环。`batch` 是最后一次 decode 用的 batch（其末位 token 的 logits 供首个采样）；
/// `n_cur` 是下一个生成 token 的位置。逐 token 采样直到 EOG / stop / `max_tokens`。
#[cfg(feature = "llama-cpp")]
fn sample_loop(
    ctx: &mut LlamaContext,
    model: &LlamaModel,
    batch: &mut LlamaBatch,
    mut n_cur: i32,
    params: &GenerateParams,
) -> Result<String, ModelError> {
    let mut output = String::new();
    // CJK 字符常跨多 token；每 token 可能是不完整 UTF-8 序列。用 byte buffer 累积，
    // 在完整 UTF-8 prefix 就绪时才追加到 output。
    let mut byte_buf: Vec<u8> = Vec::new();
    let mut current_token_count = 0;

    // seed 在 sampler 上（0.3.0）。GBNF grammar 作为第一个 sampler，先把非法 token 的
    // logit 置为 -inf。
    let mut samplers: Vec<LlamaSampler> = Vec::new();
    if let Some(gbnf) = &params.grammar {
        samplers.push(LlamaSampler::grammar(model, gbnf, "root"));
    }
    samplers.push(LlamaSampler::top_p(params.top_p, 1));
    samplers.push(LlamaSampler::temp(params.temperature));
    samplers.push(LlamaSampler::dist(params.seed));
    let mut sampler = LlamaSampler::chain_simple(samplers);

    while current_token_count < params.max_tokens {
        let token = sampler.sample(ctx, batch.n_tokens() - 1);
        sampler.accept(token);

        if model.is_eog_token(token) {
            break;
        }

        let bytes = model
            .token_to_bytes(token, Special::Plaintext)
            .map_err(|e| {
                ModelError::InferenceError(format!("Failed to convert token to bytes: {e}"))
            })?;
        byte_buf.extend(bytes);

        // 把 byte_buf 里能确定的合法 UTF-8 prefix flush 到 output。
        match std::str::from_utf8(&byte_buf) {
            Ok(s) => {
                output.push_str(s);
                byte_buf.clear();
            }
            Err(err) => {
                let valid_up_to = err.valid_up_to();
                if valid_up_to > 0 {
                    // Utf8Error::valid_up_to() 保证 [0, valid_up_to) 是合法 UTF-8，
                    // 此处 expect 是对该标准库契约的断言，不存在运行时失败路径。
                    #[allow(clippy::expect_used)]
                    let s = std::str::from_utf8(&byte_buf[..valid_up_to])
                        .expect("valid_up_to 保证此前缀是合法 UTF-8");
                    output.push_str(s);
                    byte_buf.drain(..valid_up_to);
                }
                // 剩余字节是不完整序列，等下个 token 补全。
            }
        }

        if !params.stop_sequences.is_empty() {
            let mut found_stop = false;
            for stop in &params.stop_sequences {
                if output.contains(stop) {
                    found_stop = true;
                    if let Some(pos) = output.find(stop) {
                        output.truncate(pos);
                    }
                    break;
                }
            }
            if found_stop {
                break;
            }
        }

        // BETA-17：首个完整 JSON 对象闭合即停。调用方（hybrid/full 路径）都只取第一个
        // JSON 对象，停在此处不影响结果，只省掉小模型"复读"产生的无效 decode —— 弱核显
        // 上这正是单次推理延迟的主因。output 很短（停得早），每轮重扫开销可忽略。
        if params.stop_at_json && first_json_object_complete(&output) {
            break;
        }

        current_token_count += 1;

        batch.clear();
        batch
            .add(token, n_cur, &[0], true)
            .map_err(|e| ModelError::InferenceError(format!("Failed to add token: {e}")))?;
        n_cur += 1;

        ctx.decode(batch)
            .map_err(|e| ModelError::InferenceError(format!("Failed to decode next token: {e}")))?;
    }

    Ok(output)
}
