//! 图片 OCR 引擎层（BETA-03）。
//!
//! 在 `unsafe_code = forbid` 约束下，原生 OCR API（Windows.Media.Ocr = WinRT / macOS Vision
//! = Obj-C FFI）不能直接调用 → 沿用项目 **shell-out 拿结构化输出** 套路（WindowsSearch 的
//! ADODB、Everything 的 es.exe、Spotlight 的 mdfind）：
//! - [`WindowsOcrEngine`]：`powershell` 调内嵌 `.ps1` 经 WinRT 识别（图片路径走环境变量传入，
//!   脚本不插值用户数据 → 杜绝注入）；
//! - [`TesseractOcrEngine`]：shell-out `tesseract` 兜底（跨平台，需用户装）；
//! - macOS Vision 留后续（trait 已抽象）。

use std::path::Path;
use std::process::{Command, Stdio};
#[cfg(windows)]
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::IndexError;

/// 单图 OCR 进程超时（大图 WinRT/Tesseract 识别可能数秒）。
const OCR_TIMEOUT: Duration = Duration::from_secs(30);

/// 单图 OCR 引擎。跨平台 + 跨实现（Windows WinRT / Tesseract / 后续 macOS Vision）。
pub trait OcrEngine: Send + Sync + std::fmt::Debug {
    /// 识别单张图片的全部文字（已做 CJK 空格折叠 + 数字校正变体追加，
    /// 见 [`finalize_ocr_text`]）。
    ///
    /// 失败（解码错 / 引擎错 / 超时 / 进程缺失）返回 [`IndexError::Tag`]，由增量循环计
    /// failed、跳过、不中断整轮。
    fn recognize(&self, image: &Path) -> Result<String, IndexError>;

    /// 引擎名（trace / 诊断用）。
    fn name(&self) -> &'static str;
}

/// 构造 [`IndexError::Tag`]（OCR 是按文件粒度的提取失败语义）。
fn tag_err(path: &Path, detail: impl Into<String>) -> IndexError {
    IndexError::Tag {
        path: path.to_string_lossy().into_owned(),
        detail: detail.into(),
    }
}

/// 折叠 OCR 文字里 **相邻 CJK 表意字符之间** 的空白；拉丁词间空格保留。
///
/// Windows.Media.Ocr 在 CJK 字符间插空格（`会 议 纪 要`），不折叠会破坏 trigram FTS 对
/// `会议` 的匹配。拉丁文 `Hello World` 的词间空格必须保留。
#[must_use]
pub fn normalize_ocr_text(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == ' ' || c == '\t' {
            // 找到这段连续空白的下一个非空白字符。
            let mut j = i + 1;
            while j < chars.len() && (chars[j] == ' ' || chars[j] == '\t') {
                j += 1;
            }
            let prev = out.chars().last();
            let next = chars.get(j).copied();
            // 两侧都是 CJK → 丢弃整段空白；否则保留单个空格。
            if matches!(prev, Some(p) if is_cjk(p)) && matches!(next, Some(n) if is_cjk(n)) {
                // skip
            } else {
                out.push(' ');
            }
            i = j;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// 是否 CJK 表意字符（统一表意 + 扩展 A + 兼容表意），用于空格折叠判定。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x4DBF   // 扩展 A
        | 0x4E00..=0x9FFF // 统一表意
        | 0xF900..=0xFAFF // 兼容表意
    )
}

/// OCR 数字上下文里的易错字母 → 对应数字（2026-07-06 真机实锤：准考证 PNG 手机号
/// `15013866763` 被 Windows OCR 识成 `1 S013866763`、`123456` 识成 `1234S6`）。
/// 只收经典五对，扩展前先确认误杀风险。
const fn confusable_digit(c: char) -> Option<char> {
    match c {
        'S' | 's' => Some('5'),
        'O' | 'o' => Some('0'),
        'I' | 'l' => Some('1'),
        'B' => Some('8'),
        'Z' | 'z' => Some('2'),
        _ => None,
    }
}

/// 数字或数字易错字母（数字链扫描的成员判定）。
fn is_digitish(c: char) -> bool {
    c.is_ascii_digit() || confusable_digit(c).is_some()
}

/// 从 OCR 文本提取「数字校正变体」：对疑似数字串做易错字母→数字校正 + 单空格分组合并，
/// 返回与原文不同的候选串（去重、上限 16 条）。**不改原文**——变体由
/// [`finalize_ocr_text`] 追加到正文尾部，原样与校正样都可被 trigram FTS 子串命中。
///
/// 数字链 = 连续 digitish run 序列、run 间恰一个 ASCII 空格（OCR 常把一个号码拆成
/// `1 S013866763`，也有 `789 803 810` 这类原本就分组展示的号码）。产出规则（保守，
/// 宁漏勿误）：
/// - 含易错字母：真数字 ≥ 4 且易错字母 ≤ 2（少数派）→ 校正 + 合并；
/// - 纯数字多组：组数 ≥ 2 且总数字 ≥ 6 → 仅合并（`789 803 810` → `789803810`）；
/// - 合并后 > 64 字符的病态链不产出。
#[must_use]
pub fn digit_correction_variants(text: &str) -> Vec<String> {
    let mut variants: Vec<String> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if !is_digitish(chars[i]) {
            i += 1;
            continue;
        }
        // 数字链起点：吃 run、跨单空格续链。
        let mut raw = String::new();
        let mut corrected = String::new();
        let mut digits = 0usize;
        let mut conf = 0usize;
        let mut groups = 1usize;
        let mut j = i;
        loop {
            while j < chars.len() && is_digitish(chars[j]) {
                let c = chars[j];
                raw.push(c);
                if let Some(d) = confusable_digit(c) {
                    corrected.push(d);
                    conf += 1;
                } else {
                    corrected.push(c);
                    digits += 1;
                }
                j += 1;
            }
            if j + 1 < chars.len() && chars[j] == ' ' && is_digitish(chars[j + 1]) {
                raw.push(' ');
                groups += 1;
                j += 1;
            } else {
                break;
            }
        }
        let emit = corrected.chars().count() <= 64
            && (((1..=2).contains(&conf) && digits >= 4)
                || (conf == 0 && groups >= 2 && digits >= 6));
        if emit && corrected != raw && !variants.contains(&corrected) {
            variants.push(corrected);
        }
        i = j.max(i + 1);
    }
    variants.truncate(16);
    variants
}

/// OCR 引擎输出的统一收尾：[`normalize_ocr_text`] 归一化 + 数字校正变体追加。
/// 变体以「〔OCR数字校正〕」标记行附在正文尾部——预览可见（顺带解释"为什么命中"）、
/// trigram FTS 可搜（用户按正确号码搜、命中被 OCR 误识的图/扫描页）。
/// 两个引擎（Windows.Media.Ocr / Tesseract）与扫描版 PDF 逐页管线共用此收口。
#[must_use]
pub fn finalize_ocr_text(raw: &str) -> String {
    let normalized = normalize_ocr_text(raw);
    let variants = digit_correction_variants(&normalized);
    if variants.is_empty() {
        normalized
    } else {
        format!("{normalized}\n〔OCR数字校正〕{}", variants.join(" "))
    }
}

/// spawn 外部 OCR 进程、超时 kill、成功返回 stdout（按 UTF-8 lossy 解码）。
/// 失败统一映射为按图片粒度的 [`IndexError::Tag`]（计 failed，不中断整轮）。
fn spawn_capture_stdout(mut cmd: Command, image: &Path) -> Result<String, IndexError> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    no_window(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| tag_err(image, format!("spawn OCR 进程失败: {e}")))?;
    let start = Instant::now();

    loop {
        if child
            .try_wait()
            .map_err(|e| tag_err(image, e.to_string()))?
            .is_some()
        {
            let output = child
                .wait_with_output()
                .map_err(|e| tag_err(image, e.to_string()))?;
            if output.status.success() {
                return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
            }
            return Err(tag_err(
                image,
                format!(
                    "OCR 进程失败: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        if start.elapsed() >= OCR_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(tag_err(image, "OCR 超时"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 给 `Command` 加 `CREATE_NO_WINDOW`（Windows）避免 spawn 时闪现控制台黑框；其他平台 no-op。
fn no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

// ===== 引擎选择 =====

/// 引擎优先级判定结果（纯逻辑，便于单测，不真调系统）。
#[derive(Debug, PartialEq, Eq)]
enum EnginePick {
    Windows,
    Tesseract,
    None,
}

/// 纯优先级逻辑：Windows 原生优先 → Tesseract 兜底 → 都无则 None。
fn pick_engine(win_available: bool, tess_available: bool) -> EnginePick {
    if win_available {
        EnginePick::Windows
    } else if tess_available {
        EnginePick::Tesseract
    } else {
        EnginePick::None
    }
}

/// 选默认 OCR 引擎：Windows.Media.Ocr 可用 → [`WindowsOcrEngine`]；
/// 否则 PATH 上有 `tesseract` → [`TesseractOcrEngine`]；都没有 → `None`（图片索引优雅跳过）。
#[must_use]
pub fn default_ocr_engine() -> Option<Box<dyn OcrEngine>> {
    let win_available = windows_ocr_available();
    let tess_available = TesseractOcrEngine::detect();
    match pick_engine(win_available, tess_available) {
        #[cfg(windows)]
        EnginePick::Windows => Some(Box::new(WindowsOcrEngine::new())),
        // 非 Windows 永不选 Windows（`windows_ocr_available` 恒 false），但 match 需穷尽。
        #[cfg(not(windows))]
        EnginePick::Windows => None,
        EnginePick::Tesseract => Some(Box::new(TesseractOcrEngine::new())),
        EnginePick::None => None,
    }
}

/// 是否有可用的 Windows.Media.Ocr 识别语言（非 Windows 恒 false）。
fn windows_ocr_available() -> bool {
    #[cfg(windows)]
    {
        WindowsOcrEngine::detect()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

// ===== Windows.Media.Ocr（经 PowerShell WinRT）=====

/// 内嵌 OCR 脚本：一次性单图版本（spike 验证过的 WinRT 路径）；仅在常驻 worker
/// **本身**起不来（spawn / 管道建立失败）时兜底，见 [`WindowsOcrEngine::recognize`]。
#[cfg(windows)]
const WIN_OCR_SCRIPT: &str = include_str!("ocr/win_ocr.ps1");

/// BETA-64 T8：常驻 worker 脚本——与 [`WIN_OCR_SCRIPT`] 做同样的 WinRT 类型加载 +
/// OCR，但类型加载只在进程启动时做一次，随后循环处理逐行请求（协议见脚本头注释）。
#[cfg(windows)]
const WIN_OCR_WORKER_SCRIPT: &str = include_str!("ocr/win_ocr_worker.ps1");

/// 单图 OCR 超时（[`OCR_TIMEOUT`]）同样用作常驻 worker 单次请求的等待上限——语义一致，
/// 都是"识别一张图最多等多久"，只是实现从"等进程退出"变成"等一行响应"。
#[cfg(windows)]
const WORKER_REQUEST_TIMEOUT: Duration = OCR_TIMEOUT;

/// worker stderr 捕获的最大行数，超过截断——脚本异常刷屏时不让诊断缓冲区无界增长。
#[cfg(windows)]
const STDERR_TAIL_MAX_LINES: usize = 20;

/// 常驻 worker 的存活状态：子进程句柄 + stdin 句柄（写请求）+ 响应行 channel
/// （独立读线程把子进程 stdout 逐行转发过来，见 [`WindowsOcrEngine::spawn_worker`]）。
#[cfg(windows)]
struct ResidentOcrWorker {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    lines_rx: std::sync::mpsc::Receiver<String>,
    /// worker 进程 stderr 的最新内容——脚本顶层 `trap` 在启动失败（如 WinRT 类型加载
    /// 失败、无可用识别语言）时把真实原因写到这里再 `exit 1`。独立读线程持续追加
    /// （见 [`WindowsOcrEngine::spawn_worker`]），基础设施失败分支据此把泛化错误
    /// （"OCR worker 超时或已退出"）换成真实原因，不再像 stderr 被 `Stdio::null()`
    /// 丢弃时那样只能瞎猜。
    stderr_tail: Arc<Mutex<String>>,
}

/// `recognize_via_worker` 的失败分类，供 [`WindowsOcrEngine::recognize`] 直接据此决定
/// 是否回退一次性进程，不必再事后重新查询共享的 `self.worker` 状态。**Why 不用后者**：
/// 早期实现让 `recognize` 在 `recognize_via_worker` 返回后单独再 `self.worker.lock()`
/// 一次、靠"锁里现在是不是 `None`"反推失败类别——但那是对共享可变状态的一次独立
/// 二次读取，`WindowsOcrEngine` 在多线程提取池下真会被并发调用（[`crate::scan`] 的受限
/// 线程池），线程 A 判完失败类别之前，线程 B 可能已经并发 respawn/kill 了 worker，
/// 使 A 读到的"现在的" `None`/`Some` 与 A 自己那次调用的真实结果对不上——把决策结果
/// 直接放进返回值，从根上消掉这个 TOCTOU 竞态。
#[cfg(windows)]
enum WorkerFailure {
    /// worker 基础设施本身不可信（spawn/管道/写入/超时/协议异常/正文解码失败，
    /// `recognize_via_worker` 已经清空了 `self.worker`）——`recognize` 应回退一次性
    /// 进程，给这张图一次不依赖常驻进程状态的机会。
    Infra(IndexError),
    /// worker 本身健康，只是这张图识别失败（脚本 `try/catch` 捕获后回的 `ERR:` 行，
    /// 如文件不存在/损坏）——一次性进程大概率复现同样结果，不值得回退。
    Recognition(IndexError),
}

/// Windows 原生 OCR 引擎（PowerShell + Windows.Media.Ocr WinRT）。
///
/// **BETA-64 T8（2026-07-25）常驻 worker**：此前每张图片都新起一个 PowerShell 进程、
/// 重新走一遍 WinRT 程序集/类型加载（B4 记录的固定开销主因，几百 ms 级/图）。现在
/// `worker` 懒创建、跨 `recognize` 调用复用同一个常驻进程，类型加载只付一次。
///
/// **降级路径**：`recognize_via_worker` 内部任何一种"worker 基础设施不可信"的失败
/// （spawn 失败 / 拿不到 stdin/stdout 管道 / 写入失败 / 超时 / 响应协议异常 / 响应正文
/// 解码失败）都会把 `self.worker` 清空并返回 `Err`；`recognize` 据此（`worker` 变
/// `None`）为**当次调用**回退到一次性子进程（[`WIN_OCR_SCRIPT`]，T8 之前的实现），
/// 给这张图一次不依赖常驻进程状态的机会，而不是立刻计 `failed` 干等下一轮 reindex。
/// 唯一**不**回退的情形是 worker 本身健康、只是这一张图识别失败（脚本内 `try/catch`
/// 捕获后回的 `ERR:` 行，如文件不存在/损坏）——这种失败一次性进程大概率复现同样的
/// 结果，回退纯属浪费一次子进程开销，直接返回 `Err` 让调用方计 `failed` 更诚实：
/// 提取失败的文件不落库、下一轮增量 reindex 的 mtime 比对必然判定"待处理"，见
/// [`crate::scan`] 回收逻辑，本身就有自愈路径，不需要在这一种失败模式里也抢救。
///
/// 单实例内部持锁串行处理（`Mutex<Option<ResidentOcrWorker>>`）——OCR 协议本身
/// 就是请求/响应一对一，并发调用方多线程共用一个 `WindowsOcrEngine` 时天然排队，
/// 与旧版"每次调用各自开进程、天然可并行"相比是唯一的语义变化；调用方
/// （[`crate::scan`] 的受限提取线程池）已经按重量级子进程负载把并发压到个位数，
/// 排队等待的实际影响可忽略，换来的是免掉 N 次 WinRT 类型加载。
#[cfg(windows)]
pub struct WindowsOcrEngine {
    /// 预编码的一次性脚本 `-EncodedCommand` 实参（fallback 用，构造时算一次，复用）。
    encoded_command: String,
    /// 预编码的常驻 worker 脚本 `-EncodedCommand` 实参。
    encoded_worker_command: String,
    worker: Mutex<Option<ResidentOcrWorker>>,
}

// `Child`/`ChildStdin`/`Receiver` 均非全部实现 `Debug` 友好组合，手写最小实现
// （同 crate 内 `ModelDaemon` 等含非 `Debug` 内部状态的结构体同款做法）。
#[cfg(windows)]
impl std::fmt::Debug for WindowsOcrEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsOcrEngine").finish_non_exhaustive()
    }
}

#[cfg(windows)]
impl WindowsOcrEngine {
    /// 探测：本机是否装有可用的 OCR 识别语言。
    #[must_use]
    pub fn detect() -> bool {
        let mut cmd = Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Windows.Media.Ocr.OcrEngine,Windows.Media.Ocr,ContentType=WindowsRuntime] | Out-Null; \
             if ([Windows.Media.Ocr.OcrEngine]::AvailableRecognizerLanguages.Count -gt 0) { exit 0 } else { exit 1 }",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        no_window(&mut cmd);
        matches!(cmd.status(), Ok(s) if s.success())
    }

    /// 构造（预编码脚本，无 IO；worker 懒创建，构造期不 spawn 任何进程）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            encoded_command: encode_powershell_command(WIN_OCR_SCRIPT),
            encoded_worker_command: encode_powershell_command(WIN_OCR_WORKER_SCRIPT),
            worker: Mutex::new(None),
        }
    }

    /// spawn 一个常驻 worker 进程：起 PowerShell、接管 stdin/stdout、另起一个读线程
    /// 把 stdout 逐行转发进 channel（`recognize` 侧用 `recv_timeout` 等一行响应，
    /// 避免同步 `read_line` 无法设超时的问题）。读线程在 stdout 关闭/出错时自然退出，
    /// 其 `Sender` 随之 drop——之后 `lines_rx.recv()` 会返回 `Disconnected`，
    /// `recognize_via_worker` 据此判定 worker 已死、清空状态。
    fn spawn_worker(&self) -> Result<ResidentOcrWorker, IndexError> {
        let mut cmd = Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
        ])
        .arg(&self.encoded_worker_command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        no_window(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| tag_err(Path::new(""), format!("spawn OCR worker 失败: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| tag_err(Path::new(""), "OCR worker 无 stdin 句柄"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| tag_err(Path::new(""), "OCR worker 无 stdout 句柄"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| tag_err(Path::new(""), "OCR worker 无 stderr 句柄"))?;

        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stdout);
            for line in std::io::BufRead::lines(reader) {
                let Ok(l) = line else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
            // 循环结束（EOF / 读错误 / 接收端已放弃）：`tx` 在此 drop，唤醒等待中的
            // `recv`/`recv_timeout` 返回 `Disconnected`。
        });

        // stderr 读线程：把子进程 stderr 逐行追加进 `stderr_tail`（截断到最近若干行，
        // 避免脚本异常刷屏无界增长）。仅在启动失败等边缘情况下有内容——稳态循环里
        // 脚本不写 stderr。这里不经 channel（没有请求/响应配对需求），直接共享一个
        // `Arc<Mutex<String>>`，失败分支按需读取拼进错误消息。
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        {
            let stderr_tail = stderr_tail.clone();
            std::thread::spawn(move || {
                let reader = std::io::BufReader::new(stderr);
                let mut lines_seen = 0usize;
                for line in std::io::BufRead::lines(reader) {
                    let Ok(l) = line else { break };
                    lines_seen += 1;
                    if lines_seen > STDERR_TAIL_MAX_LINES {
                        continue;
                    }
                    if let Ok(mut tail) = stderr_tail.lock() {
                        if !tail.is_empty() {
                            tail.push_str(" | ");
                        }
                        tail.push_str(&l);
                    }
                }
            });
        }

        Ok(ResidentOcrWorker {
            child,
            stdin,
            lines_rx: rx,
            stderr_tail,
        })
    }

    /// 走常驻 worker 识别一张图：写一行路径请求、等一行响应，解析 `OK:`/`ERR:` 前缀。
    /// 基础设施级失败（写失败 / 超时 / channel 断开 / 响应格式不认得 / 响应正文解码
    /// 失败）都杀掉 worker、清空 `self.worker`（下次调用重新 spawn），返回
    /// [`WorkerFailure::Infra`]；调用方 `recognize` 直接据此（不重新查共享状态，见
    /// [`WorkerFailure`] 文档）决定是否回退一次性进程。`ERR:` 分支是例外——worker 本身
    /// 健康、只是这张图识别失败，不清空 worker，返回 [`WorkerFailure::Recognition`]。
    fn recognize_via_worker(
        &self,
        image: &Path,
        native_path: &str,
    ) -> Result<String, WorkerFailure> {
        use std::io::Write;

        let mut guard = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_none() {
            *guard = Some(self.spawn_worker().map_err(WorkerFailure::Infra)?);
        }
        // 上面刚确保过 Some。
        #[allow(clippy::expect_used)]
        let worker = guard.as_mut().expect("worker just ensured Some above");
        // 先克隆一份共享句柄——`kill_worker` 会把 `ResidentOcrWorker`（含这个字段）从
        // guard 里拿走 drop 掉，之后就没法再读了；`Arc` clone 廉价，读线程仍持有另一份
        // 引用继续往里写，直到它自己也退出。
        let stderr_tail = worker.stderr_tail.clone();

        let write_ok =
            writeln!(worker.stdin, "{native_path}").is_ok() && worker.stdin.flush().is_ok();
        if !write_ok {
            kill_worker(&mut guard);
            return Err(WorkerFailure::Infra(infra_err(
                image,
                "写入 OCR worker stdin 失败（管道已断）",
                &stderr_tail,
            )));
        }

        let Ok(line) = worker.lines_rx.recv_timeout(WORKER_REQUEST_TIMEOUT) else {
            // 超时或 channel 已断开（worker 进程退出/崩溃）——最可能是启动期就失败
            // （WinRT 类型加载出错等），把脚本 trap 写到 stderr 的真实原因带进错误消息，
            // 不再只是一句猜不出原因的"超时或已退出"。
            kill_worker(&mut guard);
            return Err(WorkerFailure::Infra(infra_err(
                image,
                "OCR worker 超时或已退出",
                &stderr_tail,
            )));
        };

        if let Some(b64) = line.strip_prefix("OK:") {
            // base64/UTF-8 解码失败意味着响应正文本身已损坏（不是"这张图识别失败"这种
            // 正常业务失败——那种走 ERR: 分支），协议完整性已不可信，同"响应协议不认得"
            // 一样杀掉 worker 重来，而不是当作单图失败放过。
            let Some(text) = base64_decode(b64).and_then(|bytes| String::from_utf8(bytes).ok())
            else {
                kill_worker(&mut guard);
                return Err(WorkerFailure::Infra(tag_err(
                    image,
                    "OCR worker 响应正文解码失败（协议异常）",
                )));
            };
            Ok(text)
        } else if let Some(msg) = line.strip_prefix("ERR:") {
            Err(WorkerFailure::Recognition(tag_err(
                image,
                format!("OCR worker 识别失败: {msg}"),
            )))
        } else {
            // 协议不认得的一行：worker 状态已不可信，杀掉重来。
            kill_worker(&mut guard);
            Err(WorkerFailure::Infra(tag_err(
                image,
                "OCR worker 响应协议异常",
            )))
        }
    }
}

/// 构造基础设施失败错误，若 worker stderr 里捕获到诊断文本（脚本启动失败时的
/// `trap` 输出）则拼进消息，供 [`WindowsOcrEngine::recognize_via_worker`] 复用。
#[cfg(windows)]
fn infra_err(image: &Path, msg: &str, stderr_tail: &Arc<Mutex<String>>) -> IndexError {
    let tail = stderr_tail.lock().map(|t| t.clone()).unwrap_or_default();
    if tail.is_empty() {
        tag_err(image, msg)
    } else {
        tag_err(image, format!("{msg}（worker stderr: {tail}）"))
    }
}

/// kill 掉（若存在）并清空 slot 里的进程，供 [`WindowsOcrEngine`] 各失败分支复用；
/// `kill`/`wait` 失败静默忽略（进程可能已经自己退出，不是本函数要处理的错误）。
/// 签名只要 `&mut Option<ResidentOcrWorker>`（而非 `&mut MutexGuard<...>`）——调用方
/// 传 `&mut guard` 时 `MutexGuard` 自动解引用强制转换成这个类型，函数本身不需要知道
/// "这是从一把锁里拿出来的"，更准确地表达它的真实契约、也不再要求调用方持锁才能用。
#[cfg(windows)]
fn kill_worker(slot: &mut Option<ResidentOcrWorker>) {
    if let Some(mut w) = slot.take() {
        let _ = w.child.kill();
        let _ = w.child.wait();
    }
}

#[cfg(windows)]
impl Default for WindowsOcrEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
impl Drop for WindowsOcrEngine {
    fn drop(&mut self) {
        // 显式 kill——`ChildStdin` drop 会关闭管道，worker 脚本下次 `ReadLine()` 会
        // 收到 EOF 自行优雅退出，但那依赖脚本正常运转到下一次循环入口；显式 kill
        // 是不依赖脚本配合的兜底，避免脚本卡在某次识别里的边缘情况下留一个孤儿进程。
        // `unwrap_or_else(PoisonError::into_inner)` 而非 `if let Ok(...)`：与
        // `recognize_via_worker` 的锁获取方式一致——若锁曾因某次 panic 被污染，仍要
        // 拿到内部数据继续 kill，不能因为"锁曾经脏过"就放弃清理、让子进程常驻泄漏。
        let mut guard = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        kill_worker(&mut guard);
    }
}

#[cfg(windows)]
impl OcrEngine for WindowsOcrEngine {
    fn recognize(&self, image: &Path) -> Result<String, IndexError> {
        // WinRT `GetFileFromPathAsync` 不接受正斜杠路径（报「指定的路径无效」）——
        // daemon TOML 配置的 roots 常写 `/`，walkdir 拼出混合分隔符 path，图片 OCR
        // 全数失败（BETA-40 排查实锤）。统一归一为 `\` 再传给脚本。
        let native_path = image.to_string_lossy().replace('/', "\\");
        match self.recognize_via_worker(image, &native_path) {
            Ok(text) => Ok(finalize_ocr_text(&text)),
            // 基础设施级失败：先留痕（含 T8 stderr 捕获带回的真实原因），再回退一次性
            // 进程给这张图再一次机会。见结构体文档注释「降级路径」与 [`WorkerFailure`]
            // 文档——决策直接来自返回值，不重新查共享的 `self.worker` 状态，消掉并发
            // 调用下的 TOCTOU 竞态。
            Err(WorkerFailure::Infra(err)) => {
                tracing::warn!(
                    image = %image.display(),
                    error = %err,
                    "OCR 常驻 worker 基础设施失败，回退一次性进程"
                );
                self.recognize_one_shot(image, &native_path)
            }
            Err(WorkerFailure::Recognition(e)) => Err(e),
        }
    }

    fn name(&self) -> &'static str {
        "Windows.Media.Ocr"
    }
}

#[cfg(windows)]
impl WindowsOcrEngine {
    /// 一次性子进程识别（T8 之前的实现，现仅作 worker 基础设施失败时的兜底）。
    fn recognize_one_shot(&self, image: &Path, native_path: &str) -> Result<String, IndexError> {
        let mut cmd = Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
        ])
        .arg(&self.encoded_command)
        .env("SCOUT_OCR_IMAGE", native_path);
        let raw = spawn_capture_stdout(cmd, image)?;
        Ok(finalize_ocr_text(&raw))
    }
}

/// 把脚本编码为 PowerShell `-EncodedCommand` 实参（base64 of UTF-16LE）。
#[cfg(windows)]
fn encode_powershell_command(script: &str) -> String {
    let utf16le: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64_encode(&utf16le)
}

/// 标准 base64 编码（无外部依赖）。纯函数、无平台依赖，故不 `#[cfg(windows)]`——
/// 唯一调用方 [`encode_powershell_command`] 是 Windows-only，非 Windows 平台上本函数
/// 会被判 `dead_code`；`#[allow]` 换来的是这份编解码逻辑能在本仓库全平台 CI（含本机
/// macOS 沙盒）跑单测覆盖，而不是只能靠从未被 CI 执行过的 Windows-only 代码自证正确。
#[cfg_attr(not(windows), allow(dead_code))]
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// 标准 base64 解码（无外部依赖，配 [`base64_encode`] 供 BETA-64 T8 常驻 worker
/// 响应解码用）。非法输入（长度不对齐 4 / 非法字符）返回 `None`——调用方把它当协议
/// 异常处理（杀 worker 重来），不 panic。同 [`base64_encode`]：纯函数不 `#[cfg(windows)]`，
/// 换全平台单测覆盖。
#[cfg_attr(not(windows), allow(dead_code))]
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        // 空串是合法输入（`base64_encode(b"")` 就产出空串）——常驻 worker 对"图片里
        // 没识别出任何文字"这类真实场景会回一个空 body，不能被当成协议异常拒绝。
        return Some(Vec::new());
    }
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        // 4 字节定长 chunk，`bytecount` crate 优化的是大数组场景、这里用不上，
        // 显式 allow 而非引入新依赖。
        #[allow(clippy::naive_bytecount)]
        let pad = chunk.iter().filter(|&&b| b == b'=').count();
        if pad > 2 || chunk[..4 - pad].contains(&b'=') {
            return None;
        }
        let mut n: u32 = 0;
        for &b in chunk {
            n <<= 6;
            n |= if b == b'=' { 0 } else { val(b)? };
        }
        out.push((n >> 16 & 0xFF) as u8);
        if pad < 2 {
            out.push((n >> 8 & 0xFF) as u8);
        }
        if pad < 1 {
            out.push((n & 0xFF) as u8);
        }
    }
    Some(out)
}

// ===== Tesseract 兜底（跨平台 shell-out）=====

/// Tesseract OCR 引擎（shell-out `tesseract`，需用户装 + chi_sim/eng 语言数据）。
#[derive(Debug)]
pub struct TesseractOcrEngine {
    /// 识别语言（`tesseract -l` 参数），默认 `chi_sim+eng`。
    langs: String,
}

impl TesseractOcrEngine {
    /// 探测：PATH 上是否有可执行的 `tesseract`。
    #[must_use]
    pub fn detect() -> bool {
        let mut cmd = Command::new("tesseract");
        cmd.arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        no_window(&mut cmd);
        matches!(cmd.status(), Ok(s) if s.success())
    }

    /// 构造（默认 `chi_sim+eng`）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            langs: "chi_sim+eng".to_string(),
        }
    }
}

impl Default for TesseractOcrEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl OcrEngine for TesseractOcrEngine {
    fn recognize(&self, image: &Path) -> Result<String, IndexError> {
        let mut cmd = Command::new("tesseract");
        cmd.arg(image).arg("stdout").arg("-l").arg(&self.langs);
        let raw = spawn_capture_stdout(cmd, image)?;
        Ok(finalize_ocr_text(&raw))
    }

    fn name(&self) -> &'static str {
        "Tesseract"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn normalize_collapses_cjk_spaces() {
        assert_eq!(normalize_ocr_text("会 议 纪 要"), "会议纪要");
    }

    #[test]
    fn normalize_keeps_latin_word_spaces() {
        assert_eq!(normalize_ocr_text("Hello World"), "Hello World");
    }

    #[test]
    fn normalize_mixed_cjk_and_latin() {
        // CJK 间折叠、拉丁词间保留、CJK 与拉丁交界保留单空格。
        assert_eq!(normalize_ocr_text("图 片 abc 文 字"), "图片 abc 文字");
    }

    #[test]
    fn normalize_collapses_multiple_spaces_between_cjk() {
        assert_eq!(normalize_ocr_text("季   度   预 算"), "季度预算");
    }

    #[test]
    fn normalize_empty_and_no_space() {
        assert_eq!(normalize_ocr_text(""), "");
        assert_eq!(normalize_ocr_text("会议"), "会议");
    }

    #[test]
    fn normalize_digit_between_cjk_keeps_separation() {
        // 数字非 CJK：`年 2024 月` 两侧空格应保留（不与数字粘连）。
        assert_eq!(normalize_ocr_text("年 2024 月"), "年 2024 月");
    }

    /// 2026-07-06 真机实锤 case：准考证 PNG 里 Windows OCR 把 5 识成 S、号码被空格
    /// 拆组——校正变体必须还原出用户会搜的真号码。
    #[test]
    fn digit_variants_real_world_exam_ticket() {
        // 手机号 `15013866763` 被识成 `1 S013866763`（前导 1 被拆 + 5→S）。
        assert_eq!(
            digit_correction_variants("会员手机 1 S013866763"),
            vec!["15013866763".to_string()]
        );
        // 密码 `1234S6`（5→S，单组）。
        assert_eq!(
            digit_correction_variants("密码 1234S6"),
            vec!["123456".to_string()]
        );
        // 会议号 `789 803 810`：纯数字分组展示 → 仅合并。
        assert_eq!(
            digit_correction_variants("会议号 789 803 810"),
            vec!["789803810".to_string()]
        );
        // 身份证号紧邻误识号码（单空格连成一条链）：整链校正合并，子串仍可 trigram 命中。
        assert_eq!(
            digit_correction_variants("440307201312314812 1 S013866763"),
            vec!["44030720131231481215013866763".to_string()]
        );
    }

    /// 保守规则的反例：不该产出变体的输入。
    #[test]
    fn digit_variants_conservative_negatives() {
        // 纯字母词（l/o 是易错字符但无真数字）。
        assert!(digit_correction_variants("Hello World Solo").is_empty());
        // 真数字不足 4 个。
        assert!(digit_correction_variants("S13 B2").is_empty());
        // 易错字母过多（> 2，更像真字母串 / 序列号）。
        assert!(digit_correction_variants("SOS 1234 SOB").is_empty());
        // 单组纯数字（无需校正也无需合并）。
        assert!(digit_correction_variants("电话 15013866763").is_empty());
        // 空串。
        assert!(digit_correction_variants("").is_empty());
    }

    /// finalize：无变体 → 与 normalize 等价；有变体 → 追加标记行、原文保留。
    #[test]
    fn finalize_appends_variants_and_keeps_original() {
        assert_eq!(finalize_ocr_text("会 议 纪 要"), "会议纪要");
        let out = finalize_ocr_text("会员手机 1 S013866763");
        assert!(out.starts_with("会员手机 1 S013866763"), "原文必须保留");
        assert!(
            out.ends_with("〔OCR数字校正〕15013866763"),
            "变体行追加在尾部，实得 {out:?}"
        );
    }

    /// 变体去重 + 上限 16 条（病态 OCR 噪声不撑爆 body）。
    #[test]
    fn digit_variants_dedupe_and_cap() {
        let dup = digit_correction_variants("1234S6 和 1234S6");
        assert_eq!(dup, vec!["123456".to_string()], "重复链只产出一条");
        let many: String = (0..30)
            .map(|i| format!("{i:04}S{i:02}"))
            .collect::<Vec<_>>()
            .join(" 号 ");
        assert!(digit_correction_variants(&many).len() <= 16);
    }

    #[test]
    fn pick_engine_priority() {
        assert_eq!(pick_engine(true, true), EnginePick::Windows);
        assert_eq!(pick_engine(true, false), EnginePick::Windows);
        assert_eq!(pick_engine(false, true), EnginePick::Tesseract);
        assert_eq!(pick_engine(false, false), EnginePick::None);
    }

    #[test]
    fn is_cjk_classifies_correctly() {
        assert!(is_cjk('会'));
        assert!(is_cjk('议'));
        assert!(!is_cjk('a'));
        assert!(!is_cjk('2'));
        assert!(!is_cjk(' '));
    }

    #[test]
    fn tesseract_name() {
        assert_eq!(TesseractOcrEngine::new().name(), "Tesseract");
    }

    // base64_encode/base64_decode 是纯函数（无平台依赖），不 `#[cfg(windows)]`——
    // 下面几条单测因此在本仓库全平台 CI（含本机 macOS 沙盒）都会真的跑起来，
    // 是 BETA-64 T8 常驻 worker 响应解码路径里唯一能在非 Windows 环境获得真实
    // 测试覆盖的部分（其余 worker 相关代码整体 `#[cfg(windows)]`、只能靠 Windows
    // 真机/CI 验证）。
    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"M"), "TQ==");
        assert_eq!(base64_encode(b""), "");
    }

    /// BETA-64 T8：解码是编码的逆运算，标准测试向量应互为往返。
    #[test]
    fn base64_decode_known_vectors() {
        assert_eq!(base64_decode("TWFu"), Some(b"Man".to_vec()));
        assert_eq!(base64_decode("TWE="), Some(b"Ma".to_vec()));
        assert_eq!(base64_decode("TQ=="), Some(b"M".to_vec()));
        assert_eq!(
            base64_decode(""),
            Some(Vec::new()),
            "空串是合法输入（对应 base64_encode(b\"\") 产出的空串）"
        );
    }

    /// BETA-64 T8：encode/decode 往返恒等，覆盖含 UTF-8 多字节字符的场景
    /// （常驻 worker 响应正文就是 OCR 识别出的任意 Unicode 文本）。
    #[test]
    fn base64_round_trips_arbitrary_bytes_including_utf8() {
        let samples: &[&[u8]] = &[
            b"",
            b"a",
            b"ab",
            b"abc",
            b"abcd",
            "会议纪要 2024".as_bytes(),
            "〔OCR数字校正〕15013866763".as_bytes(),
            &[0u8, 1, 2, 3, 255, 254, 253],
        ];
        for sample in samples {
            let encoded = base64_encode(sample);
            let decoded = base64_decode(&encoded);
            assert_eq!(
                decoded.as_deref(),
                Some(*sample),
                "round-trip 失败：sample={sample:?}, encoded={encoded:?}"
            );
        }
    }

    /// BETA-64 T8：非法输入返回 `None` 而不 panic——常驻 worker 响应协议异常时，
    /// 调用方靠这个信号判定"杀 worker 重来"，不能让解码本身崩溃拖垮整个 OCR 引擎。
    #[test]
    fn base64_decode_rejects_malformed_input() {
        assert_eq!(base64_decode("T"), None, "长度非 4 的倍数");
        assert_eq!(base64_decode("TWF"), None, "长度非 4 的倍数");
        assert_eq!(base64_decode("T@Fu"), None, "非法字符");
        assert_eq!(base64_decode("T=Fu"), None, "'=' 出现在非法位置（非末尾）");
    }

    #[cfg(windows)]
    #[test]
    fn encode_powershell_command_round_trips_via_utf16le_base64() {
        // "AB" -> UTF-16LE 字节 [0x41,0x00,0x42,0x00] -> base64。
        assert_eq!(
            encode_powershell_command("AB"),
            base64_encode(&[0x41, 0, 0x42, 0])
        );
    }
}
