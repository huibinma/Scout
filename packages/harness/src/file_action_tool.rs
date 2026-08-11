//! FileActionTool —— Tool 抽象下的文件操作执行器（MVP-10A）。
//!
//! 集成 [`crate::policy::PolicyEngine`] 与 [`crate::context::ContextMemory`]：
//! 把 `SearchIntent::FileAction` 解析成具体的 (path, action) 列表，过 Policy
//! 校验后调用 [`FileActionExecutor`] 真正执行。
//!
//! # 安全边界
//!
//! - `delete` 在 schema 层与 Policy 层双重禁用（[`PolicyEngine`] 返回 Deny，
//!   此处先以 [`FileActionError::DeleteNotSupported`] 兜底，保证即使 Policy
//!   被误配置也不会删用户文件）。
//! - 批量阈值（默认 10）：超过则返回 [`FileActionError::BatchThresholdExceeded`]
//!   ，提示上层降级到 Clarify。
//! - 跨卷 move：当 `std::fs::rename` 报 `CrossesDevices` 时由默认 executor
//!   fallback 到 copy + 删除源（仍受 Policy 阀门）。
//!
//! # 与 schema §7.6 的契约
//!
//! 用例 #36/#37（open by index）/ #38（locate）/ #39（refine + copy 两阶段）/
//! #40（delete clarify）由集成测试覆盖。

#![allow(clippy::doc_markdown)]

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use scout_search_backend::{FileAction, FileActionKind};

use crate::context::{ContextMemory, TargetRefError};
use crate::policy::{PolicyAction, PolicyDecision, PolicyEngine};
use crate::{ImplementationStatus, Tool, ToolCapability, ToolKind};

/// 默认批量阈值：超过该数量的目标必须 clarify。
pub const DEFAULT_BATCH_THRESHOLD: usize = 10;

// ============================================================
// §1. FileActionExecutor trait
// ============================================================

/// 平台无关的文件操作 trait。
///
/// 真实平台实现走 macOS `open` / Windows `explorer.exe`；测试用 `MockFileActionExecutor`。
pub trait FileActionExecutor: fmt::Debug + Send + Sync {
    /// 用系统默认应用打开。
    fn open(&self, path: &Path) -> io::Result<()>;
    /// 在文件管理器中显示 / 高亮（macOS Finder reveal / Windows Explorer select）。
    fn locate(&self, path: &Path) -> io::Result<()>;
    /// 复制到目标路径。
    fn copy(&self, src: &Path, dest: &Path) -> io::Result<()>;
    /// 移动到目标路径；跨卷时由具体实现 fallback 到 copy + 删除源。
    fn move_to(&self, src: &Path, dest: &Path) -> io::Result<()>;
    /// 同目录下重命名为 `new_name`。
    fn rename(&self, src: &Path, new_name: &str) -> io::Result<()>;
}

/// 默认本地实现：直接调用 OS 工具。
///
/// 仅在 macOS / Windows 下实现 `open` / `locate`；其他系统返回 `Unsupported`。
/// `copy` / `move_to` / `rename` 用 `std::fs` 跨平台实现。
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalFileActionExecutor;

impl FileActionExecutor for LocalFileActionExecutor {
    fn open(&self, path: &Path) -> io::Result<()> {
        platform_open(path)
    }

    fn locate(&self, path: &Path) -> io::Result<()> {
        platform_locate(path)
    }

    fn copy(&self, src: &Path, dest: &Path) -> io::Result<()> {
        std::fs::copy(src, dest).map(|_| ())
    }

    fn move_to(&self, src: &Path, dest: &Path) -> io::Result<()> {
        match std::fs::rename(src, dest) {
            Ok(()) => Ok(()),
            // CrossesDevices 在 stable Rust 上通过 raw_os_error 检测；这里走 fallback
            Err(err) if is_cross_device(&err) => {
                std::fs::copy(src, dest)?;
                std::fs::remove_file(src)?;
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    fn rename(&self, src: &Path, new_name: &str) -> io::Result<()> {
        let parent = src.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "source path has no parent")
        })?;
        let dest = parent.join(new_name);
        std::fs::rename(src, dest)
    }
}

#[cfg(target_os = "macos")]
fn platform_open(path: &Path) -> io::Result<()> {
    std::process::Command::new("open").arg(path).status()?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn platform_locate(path: &Path) -> io::Result<()> {
    std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .status()?;
    Ok(())
}

/// Windows `CREATE_NO_WINDOW`：GUI app spawn 控制台子进程时不闪黑框。
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(target_os = "windows")]
fn platform_open(path: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;
    let target = scout_search_backend::user_facing_path(path);
    std::process::Command::new("cmd")
        .args(["/C", "start", "", &target])
        .creation_flags(CREATE_NO_WINDOW)
        .status()?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn platform_locate(path: &Path) -> io::Result<()> {
    use std::os::windows::process::CommandExt;

    let [select_switch, target] = windows_explorer_select_args(path);
    std::process::Command::new("explorer.exe")
        // Explorer 使用自己的命令行解析器：不接受 `\\?\` 前缀；若再把
        // `/select,` 与带空格路径拼成一个普通参数，Rust 会给整个参数加引号，
        // Explorer 也可能误判。这里先恢复用户路径，再把 switch 与路径分开传递。
        .raw_arg(select_switch)
        .arg(target)
        .creation_flags(CREATE_NO_WINDOW)
        .status()?;
    Ok(())
}

/// Windows Explorer 的 `/select, <path>` 必须把 switch 与目标路径分开传递。
///
/// 目标路径先走统一的用户边界规范化，去掉 Explorer 不接受的 `\\?\` 前缀。
#[cfg(any(target_os = "windows", test))]
fn windows_explorer_select_args(path: &Path) -> [std::ffi::OsString; 2] {
    let target = scout_search_backend::user_facing_path(path);
    [
        std::ffi::OsString::from("/select,"),
        std::ffi::OsString::from(target),
    ]
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_open(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "open is not supported on this platform",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn platform_locate(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "locate is not supported on this platform",
    ))
}

fn is_cross_device(err: &io::Error) -> bool {
    // Linux EXDEV = 18, macOS EXDEV = 18, Windows: 不直接区分
    err.raw_os_error() == Some(18)
}

#[cfg(test)]
mod platform_tests {
    use super::windows_explorer_select_args;
    use std::ffi::OsString;
    use std::path::Path;

    #[test]
    fn windows_explorer_select_keeps_switch_and_spaced_path_separate() {
        let path = Path::new(r"C:\Users\测试用户\My Documents\报告 终稿.pdf");
        let args = windows_explorer_select_args(path);

        assert_eq!(args[0], OsString::from("/select,"));
        assert_eq!(args[1], path.as_os_str());
    }

    #[test]
    fn windows_explorer_select_removes_verbatim_prefix() {
        let args = windows_explorer_select_args(Path::new(
            r"\\?\C:\Users\测试用户\My Documents\报告 终稿.pdf",
        ));

        assert_eq!(args[0], OsString::from("/select,"));
        assert_eq!(
            args[1],
            OsString::from(r"C:\Users\测试用户\My Documents\报告 终稿.pdf")
        );
    }
}

// ============================================================
// §2. FileActionTool
// ============================================================

/// 文件操作工具。注册到 [`crate::ToolRegistry`] 后由 Intent Router 或 Tool Loop 调用。
pub struct FileActionTool {
    id: String,
    name: String,
    capability: ToolCapability,
    policy: PolicyEngine,
    executor: Arc<dyn FileActionExecutor>,
    batch_threshold: usize,
}

impl fmt::Debug for FileActionTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileActionTool")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("batch_threshold", &self.batch_threshold)
            .finish_non_exhaustive()
    }
}

impl FileActionTool {
    /// 构造一个 FileActionTool。MVP-10A 默认配置：
    /// - id: `"file_action.local"`
    /// - name: `"本地文件操作"`
    /// - 支持的动作：open / locate / copy / move / rename（**不含 delete**）
    /// - batch_threshold: [`DEFAULT_BATCH_THRESHOLD`]
    #[must_use]
    pub fn new(executor: Arc<dyn FileActionExecutor>, policy: PolicyEngine) -> Self {
        let capability = ToolCapability::for_file_action(
            "本地文件操作（open / locate / copy / move / rename）",
            vec![
                FileActionKind::Open,
                FileActionKind::Locate,
                FileActionKind::Copy,
                FileActionKind::Move,
                FileActionKind::Rename,
            ],
        );
        Self {
            id: "file_action.local".to_owned(),
            name: "本地文件操作".to_owned(),
            capability,
            policy,
            executor,
            batch_threshold: DEFAULT_BATCH_THRESHOLD,
        }
    }

    /// 测试 / 高级用法：自定义 id、name、批量阈值。
    #[must_use]
    pub fn with_overrides(
        mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        batch_threshold: usize,
    ) -> Self {
        self.id = id.into();
        self.name = name.into();
        self.batch_threshold = batch_threshold;
        self
    }

    /// 执行一次文件操作。
    ///
    /// 流程：
    /// 1. delete 直接返回 [`FileActionError::DeleteNotSupported`]（MVP 硬禁用）
    /// 2. 解析 `target_ref` → 路径列表
    /// 3. 检查批量阈值
    /// 4. 走 Policy：
    ///    - Allow → 直接执行
    ///    - RequireConfirmation 且 `action.requires_confirmation = false` → 返回 [`FileActionOutcome::RequiresConfirmation`]
    ///    - RequireConfirmation 且 `requires_confirmation = true` → 执行
    ///    - Deny → 返回 [`FileActionError::PolicyDenied`]
    /// 5. 对每条路径调用 [`FileActionExecutor`] 对应方法
    pub fn invoke(
        &self,
        action: &FileAction,
        context: &ContextMemory,
    ) -> Result<FileActionOutcome, FileActionError> {
        // 1. delete 硬禁用（即便 Policy 误配也兜底）
        if matches!(action.action, FileActionKind::Delete) {
            return Err(FileActionError::DeleteNotSupported);
        }

        // 2. 解析 target_ref
        let targets = context
            .resolve_target_ref(&action.target_ref)
            .map_err(FileActionError::TargetRef)?;
        if targets.is_empty() {
            return Err(FileActionError::EmptyTargets);
        }

        // 3. 批量阈值
        if targets.len() > self.batch_threshold {
            return Err(FileActionError::BatchThresholdExceeded {
                count: targets.len(),
                threshold: self.batch_threshold,
            });
        }

        // 4. Policy 校验
        let decision = self
            .policy
            .evaluate(&PolicyAction::FileAction(action.action));
        match decision {
            PolicyDecision::Deny { reason } => {
                return Err(FileActionError::PolicyDenied { reason });
            }
            PolicyDecision::RequireConfirmation if !action.requires_confirmation => {
                return Ok(FileActionOutcome::RequiresConfirmation { paths: targets });
            }
            PolicyDecision::Allow | PolicyDecision::RequireConfirmation => {}
        }

        // 5. 校验动作所需参数
        match action.action {
            FileActionKind::Copy | FileActionKind::Move => {
                if action.destination.as_deref().map_or(true, str::is_empty) {
                    return Err(FileActionError::MissingDestination);
                }
            }
            FileActionKind::Rename => {
                if action.new_name.as_deref().map_or(true, str::is_empty) {
                    return Err(FileActionError::MissingNewName);
                }
            }
            FileActionKind::Open | FileActionKind::Locate => {}
            FileActionKind::Delete => unreachable!(), // §5.1 已拦截
        }

        // 5.5 预检（copy/move）：算出全部落点 → 零副作用整体校验。
        //   - 任一落点已存在 → PathConflict。
        //   - 批内两个源 join 到同一落点(同名)→ DuplicateTargetName(否则后者会覆盖前者)。
        // 注：本预检仅保证"冲突/重名"路径零副作用；预检通过后 executor 中途失败(权限/磁盘)
        //   仍可能留下部分已执行的目标(MVP 不回滚)。
        if matches!(action.action, FileActionKind::Copy | FileActionKind::Move) {
            let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
            for target in &targets {
                let dest = Self::dest_path_for(action, target)?;
                if dest.exists() {
                    return Err(FileActionError::PathConflict { dest });
                }
                if !seen.insert(dest.clone()) {
                    return Err(FileActionError::DuplicateTargetName { dest });
                }
            }
        }

        // 6. 执行
        for target in &targets {
            self.execute_one(action, target)?;
        }

        Ok(FileActionOutcome::Executed { affected: targets })
    }

    fn execute_one(&self, action: &FileAction, target: &Path) -> Result<(), FileActionError> {
        match action.action {
            FileActionKind::Open => self
                .executor
                .open(target)
                .map_err(FileActionError::Executor),
            FileActionKind::Locate => self
                .executor
                .locate(target)
                .map_err(FileActionError::Executor),
            FileActionKind::Copy => {
                let dest_path = Self::dest_path_for(action, target)?;
                self.executor
                    .copy(target, &dest_path)
                    .map_err(FileActionError::Executor)
            }
            FileActionKind::Move => {
                let dest_path = Self::dest_path_for(action, target)?;
                self.executor
                    .move_to(target, &dest_path)
                    .map_err(FileActionError::Executor)
            }
            FileActionKind::Rename => {
                let new_name = action.new_name.as_deref().unwrap_or_default();
                self.executor
                    .rename(target, new_name)
                    .map_err(FileActionError::Executor)
            }
            FileActionKind::Delete => unreachable!(),
        }
    }

    /// copy/move 的落点：把 destination 当**目录**，join 源文件名。
    /// 返回 `dir.join(target.file_name())`。
    fn dest_path_for(action: &FileAction, target: &Path) -> Result<PathBuf, FileActionError> {
        let dir = action.destination.as_deref().unwrap_or_default();
        // SAFETY: targets 来自搜索结果，恒有 file_name；此分支为防御性兜底。
        let file_name = target
            .file_name()
            .ok_or(FileActionError::MissingDestination)?;
        Ok(Path::new(dir).join(file_name))
    }
}

impl Tool for FileActionTool {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    fn kind(&self) -> ToolKind {
        ToolKind::FileAction
    }
    fn capability(&self) -> &ToolCapability {
        &self.capability
    }
    fn implementation_status(&self) -> ImplementationStatus {
        ImplementationStatus::Real
    }
    fn is_available(&self) -> bool {
        true
    }
}

// ============================================================
// §3. Outcome / Error
// ============================================================

/// `FileActionTool::invoke` 的成功输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileActionOutcome {
    /// 操作已执行。`affected` 为实际涉及的路径列表。
    Executed {
        /// 涉及的路径。
        affected: Vec<PathBuf>,
    },
    /// 需要用户确认（Policy 要求确认且 intent 中 `requires_confirmation = false`）。
    RequiresConfirmation {
        /// 拟操作的路径列表（用于 UI 展示）。
        paths: Vec<PathBuf>,
    },
}

/// `FileActionTool::invoke` 的错误类型。
#[derive(Debug)]
pub enum FileActionError {
    /// `delete` 在 MVP 硬禁用。
    DeleteNotSupported,
    /// Policy 拒绝。
    PolicyDenied {
        /// Policy 给出的拒绝理由。
        reason: String,
    },
    /// target_ref 解析失败（无上下文 / 越界 / 空 indices）。
    TargetRef(TargetRefError),
    /// target_ref 解析结果为空数组。
    EmptyTargets,
    /// 批量阈值超限。
    BatchThresholdExceeded {
        /// 目标数量。
        count: usize,
        /// 当前阈值。
        threshold: usize,
    },
    /// Copy / Move 缺 `destination`。
    MissingDestination,
    /// Rename 缺 `new_name`。
    MissingNewName,
    /// Copy / Move 时目标路径已存在。
    PathConflict {
        /// 已存在的目标路径。
        dest: PathBuf,
    },
    /// 批内多个源文件重名,会 join 到同一目标落点(相互覆盖)。
    DuplicateTargetName {
        /// 冲突的目标落点。
        dest: PathBuf,
    },
    /// 平台执行器返回的 IO 错误。
    Executor(io::Error),
}

impl fmt::Display for FileActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeleteNotSupported => f.write_str("delete action is not supported in MVP"),
            Self::PolicyDenied { reason } => write!(f, "policy denied: {reason}"),
            Self::TargetRef(inner) => write!(f, "target_ref error: {inner}"),
            Self::EmptyTargets => f.write_str("target_ref resolved to empty path list"),
            Self::BatchThresholdExceeded { count, threshold } => write!(
                f,
                "batch action targets {count} exceeds threshold {threshold}, requires clarify"
            ),
            Self::MissingDestination => f.write_str("file_action.destination is required"),
            Self::MissingNewName => f.write_str("file_action.new_name is required"),
            Self::PathConflict { dest } => {
                write!(f, "destination already exists: {}", dest.display())
            }
            Self::DuplicateTargetName { dest } => {
                write!(f, "duplicate target name maps to {}", dest.display())
            }
            Self::Executor(err) => write!(f, "executor io error: {err}"),
        }
    }
}

impl std::error::Error for FileActionError {}

// ============================================================
// §4. 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::context::ContextMemory;
    use scout_search_backend::{
        BackendKind, FileSearch, Language, MatchType, SchemaVersion, SearchIntent, SearchResult,
        SearchResultMetadata, TargetRef, TargetSelector,
    };
    use std::sync::Mutex;

    // ---- Mock executor: 仅记录调用，不动文件系统 ----
    #[derive(Debug, Default)]
    struct MockExecutor {
        calls: Mutex<Vec<MockCall>>,
        fail_on: Option<MockOp>,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum MockCall {
        Open(PathBuf),
        Locate(PathBuf),
        Copy(PathBuf, PathBuf),
        Move(PathBuf, PathBuf),
        Rename(PathBuf, String),
    }

    #[allow(dead_code)] // 仅在 fail_on 路径用，部分变体在当前测试集未被引用
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum MockOp {
        Open,
        Locate,
    }

    impl FileActionExecutor for MockExecutor {
        fn open(&self, path: &Path) -> io::Result<()> {
            if matches!(self.fail_on, Some(MockOp::Open)) {
                return Err(io::Error::other("mock open fail"));
            }
            self.calls.lock().unwrap().push(MockCall::Open(path.into()));
            Ok(())
        }
        fn locate(&self, path: &Path) -> io::Result<()> {
            if matches!(self.fail_on, Some(MockOp::Locate)) {
                return Err(io::Error::other("mock locate fail"));
            }
            self.calls
                .lock()
                .unwrap()
                .push(MockCall::Locate(path.into()));
            Ok(())
        }
        fn copy(&self, src: &Path, dest: &Path) -> io::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(MockCall::Copy(src.into(), dest.into()));
            Ok(())
        }
        fn move_to(&self, src: &Path, dest: &Path) -> io::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(MockCall::Move(src.into(), dest.into()));
            Ok(())
        }
        fn rename(&self, src: &Path, new_name: &str) -> io::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(MockCall::Rename(src.into(), new_name.into()));
            Ok(())
        }
    }

    fn mk_result(idx: usize) -> SearchResult {
        SearchResult {
            id: format!("id-{idx}"),
            path: PathBuf::from(format!("/tmp/test-{idx}.txt")),
            name: format!("test-{idx}.txt"),
            source: BackendKind::Spotlight,
            match_type: MatchType::Filename,
            score: None,
            metadata: SearchResultMetadata::default(),
        }
    }

    fn mk_dummy_file_search() -> FileSearch {
        FileSearch {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            keywords: None,
            extensions: None,
            file_type: None,
            location: None,
            modified_time: None,
            created_time: None,
            accessed_time: None,
            size: None,
            exclude_extensions: None,
            exclude_file_type: None,
            sort: None,
            limit: None,
        }
    }

    fn mk_context_with(n: usize) -> ContextMemory {
        let mut mem = ContextMemory::new();
        let results = (1..=n).map(mk_result).collect();
        mem.record(SearchIntent::FileSearch(mk_dummy_file_search()), results);
        mem
    }

    fn mk_tool(mock: Arc<MockExecutor>) -> FileActionTool {
        FileActionTool::new(mock, PolicyEngine)
    }

    // ---- Tool trait 实现 ----

    #[test]
    fn tool_metadata_describes_file_action() {
        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock);
        assert_eq!(tool.id(), "file_action.local");
        assert_eq!(tool.kind(), ToolKind::FileAction);
        assert!(tool.is_available());
        assert_eq!(tool.capability().supported_actions.len(), 5);
        assert!(
            !tool
                .capability()
                .supported_actions
                .contains(&FileActionKind::Delete),
            "delete must not be in capability"
        );
    }

    // ---- §7.6 contract tests ----

    /// §7.6 #36 / #37：打开第三个 / open the third one
    #[test]
    fn contract_36_open_third_result() {
        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock.clone());
        let context = mk_context_with(5);

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            action: FileActionKind::Open,
            target_ref: TargetRef::LastResults {
                selector: TargetSelector::Index { value: 3 },
            },
            destination: None,
            new_name: None,
            requires_confirmation: false,
        };

        let outcome = tool.invoke(&action, &context).unwrap();
        let FileActionOutcome::Executed { affected } = outcome else {
            panic!("expected Executed")
        };
        assert_eq!(affected, vec![PathBuf::from("/tmp/test-3.txt")]);
        assert_eq!(
            *mock.calls.lock().unwrap(),
            vec![MockCall::Open(PathBuf::from("/tmp/test-3.txt"))]
        );
    }

    /// §7.6 #38：在访达里显示第一个
    #[test]
    fn contract_38_locate_first_result() {
        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock.clone());
        let context = mk_context_with(2);

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            action: FileActionKind::Locate,
            target_ref: TargetRef::LastResults {
                selector: TargetSelector::Index { value: 1 },
            },
            destination: None,
            new_name: None,
            requires_confirmation: false,
        };
        tool.invoke(&action, &context).unwrap();

        assert_eq!(
            *mock.calls.lock().unwrap(),
            vec![MockCall::Locate(PathBuf::from("/tmp/test-1.txt"))]
        );
    }

    /// §7.6 #39 第二阶段：把 refine 后的全部 pdf 复制到桌面。
    /// 方案 A：destination 是目录，多目标各 join basename → 3 次 copy，落点互不冲突。
    #[test]
    fn contract_39_copy_all_with_confirmation() {
        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock.clone());
        let context = mk_context_with(3);

        let dir = std::env::temp_dir().join(format!("scout-contract39-{}", std::process::id()));

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            action: FileActionKind::Copy,
            target_ref: TargetRef::LastResults {
                selector: TargetSelector::Indices {
                    values: vec![1, 2, 3],
                },
            },
            destination: Some(dir.to_string_lossy().into_owned()),
            new_name: None,
            requires_confirmation: true,
        };

        let outcome = tool.invoke(&action, &context).unwrap();
        let FileActionOutcome::Executed { affected } = outcome else {
            panic!("expected Executed")
        };
        assert_eq!(affected.len(), 3);
        assert_eq!(mock.calls.lock().unwrap().len(), 3);
    }

    /// §7.6 #40：删除请求（schema 允许 delete，本工具拒绝）
    #[test]
    fn contract_40_delete_is_rejected() {
        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock);
        let context = mk_context_with(1);

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            action: FileActionKind::Delete,
            target_ref: TargetRef::LastResults {
                selector: TargetSelector::Index { value: 1 },
            },
            destination: None,
            new_name: None,
            requires_confirmation: true,
        };

        let err = tool.invoke(&action, &context).unwrap_err();
        assert!(matches!(err, FileActionError::DeleteNotSupported));
    }

    // ---- 边界 / 错误分支 ----

    #[test]
    fn target_ref_out_of_range_propagates() {
        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock);
        let context = mk_context_with(1);

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: None,
            action: FileActionKind::Open,
            target_ref: TargetRef::LastResults {
                selector: TargetSelector::Index { value: 5 },
            },
            destination: None,
            new_name: None,
            requires_confirmation: false,
        };
        let err = tool.invoke(&action, &context).unwrap_err();
        assert!(matches!(
            err,
            FileActionError::TargetRef(TargetRefError::IndexOutOfRange { requested: 5, .. })
        ));
    }

    #[test]
    fn batch_threshold_exceeded() {
        let mock = Arc::new(MockExecutor::default());
        let tool = FileActionTool::new(mock, PolicyEngine).with_overrides(
            "file_action.local",
            "本地文件操作",
            3, // 阈值降到 3
        );
        let context = mk_context_with(5);

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: None,
            action: FileActionKind::Open,
            target_ref: TargetRef::LastResults {
                selector: TargetSelector::All,
            },
            destination: None,
            new_name: None,
            requires_confirmation: false,
        };
        let err = tool.invoke(&action, &context).unwrap_err();
        assert!(matches!(
            err,
            FileActionError::BatchThresholdExceeded {
                count: 5,
                threshold: 3
            }
        ));
    }

    #[test]
    fn copy_requires_destination() {
        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock);
        let context = mk_context_with(1);

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: None,
            action: FileActionKind::Copy,
            target_ref: TargetRef::LastResults {
                selector: TargetSelector::Index { value: 1 },
            },
            destination: None,
            new_name: None,
            requires_confirmation: true,
        };
        let err = tool.invoke(&action, &context).unwrap_err();
        assert!(matches!(err, FileActionError::MissingDestination));
    }

    #[test]
    fn rename_requires_new_name() {
        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock);
        let context = mk_context_with(1);

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: None,
            action: FileActionKind::Rename,
            target_ref: TargetRef::LastResults {
                selector: TargetSelector::Index { value: 1 },
            },
            destination: None,
            new_name: None,
            requires_confirmation: true,
        };
        let err = tool.invoke(&action, &context).unwrap_err();
        assert!(matches!(err, FileActionError::MissingNewName));
    }

    #[test]
    fn write_action_without_confirmation_returns_requires_confirmation() {
        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock);
        let context = mk_context_with(1);

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: None,
            action: FileActionKind::Copy,
            target_ref: TargetRef::LastResults {
                selector: TargetSelector::Index { value: 1 },
            },
            destination: Some("/tmp/dest.txt".to_owned()),
            new_name: None,
            requires_confirmation: false, // 用户未确认
        };
        let outcome = tool.invoke(&action, &context).unwrap();
        let FileActionOutcome::RequiresConfirmation { paths } = outcome else {
            panic!("expected RequiresConfirmation")
        };
        assert_eq!(paths, vec![PathBuf::from("/tmp/test-1.txt")]);
    }

    #[test]
    fn rename_with_new_name_calls_executor() {
        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock.clone());
        let context = mk_context_with(1);

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: None,
            action: FileActionKind::Rename,
            target_ref: TargetRef::LastResults {
                selector: TargetSelector::Index { value: 1 },
            },
            destination: None,
            new_name: Some("renamed.txt".to_owned()),
            requires_confirmation: true,
        };
        tool.invoke(&action, &context).unwrap();

        assert_eq!(
            *mock.calls.lock().unwrap(),
            vec![MockCall::Rename(
                PathBuf::from("/tmp/test-1.txt"),
                "renamed.txt".to_owned()
            )]
        );
    }

    #[test]
    fn no_context_results_in_target_ref_error() {
        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock);
        let context = ContextMemory::new();

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: None,
            action: FileActionKind::Open,
            target_ref: TargetRef::LastResults {
                selector: TargetSelector::Index { value: 1 },
            },
            destination: None,
            new_name: None,
            requires_confirmation: false,
        };
        let err = tool.invoke(&action, &context).unwrap_err();
        assert!(matches!(
            err,
            FileActionError::TargetRef(TargetRefError::NoLastResults)
        ));
    }

    /// 方案 A：多目标 copy，destination 当目录，逐目标 join basename。
    #[test]
    fn copy_multi_target_joins_basename_per_target() {
        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock.clone());
        let context = mk_context_with(3);

        // 唯一且不存在的目录 → 预检通过（不在磁盘创建）
        let dir = std::env::temp_dir().join(format!("scout-multi-join-{}", std::process::id()));

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            action: FileActionKind::Copy,
            target_ref: TargetRef::LastResults {
                selector: TargetSelector::Indices {
                    values: vec![1, 2, 3],
                },
            },
            destination: Some(dir.to_string_lossy().into_owned()),
            new_name: None,
            requires_confirmation: true,
        };

        let outcome = tool.invoke(&action, &context).unwrap();
        let FileActionOutcome::Executed { affected } = outcome else {
            panic!("expected Executed")
        };
        assert_eq!(affected.len(), 3);
        assert_eq!(
            *mock.calls.lock().unwrap(),
            vec![
                MockCall::Copy(PathBuf::from("/tmp/test-1.txt"), dir.join("test-1.txt")),
                MockCall::Copy(PathBuf::from("/tmp/test-2.txt"), dir.join("test-2.txt")),
                MockCall::Copy(PathBuf::from("/tmp/test-3.txt"), dir.join("test-3.txt")),
            ]
        );
    }

    /// 预检：任一落点已存在 → 整体 PathConflict，零 executor 调用（原子中止）。
    #[test]
    fn copy_multi_target_preflight_conflict_atomic() {
        let dir = std::env::temp_dir().join(format!(
            "scout-preflight-{}-{}",
            std::process::id(),
            "atomic"
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("test-2.txt"), b"x").unwrap();

        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock.clone());
        let context = mk_context_with(3);

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            action: FileActionKind::Copy,
            target_ref: TargetRef::LastResults {
                selector: TargetSelector::Indices {
                    values: vec![1, 2, 3],
                },
            },
            destination: Some(dir.to_string_lossy().into_owned()),
            new_name: None,
            requires_confirmation: true,
        };

        let err = tool.invoke(&action, &context).unwrap_err();
        assert!(
            matches!(err, FileActionError::PathConflict { .. }),
            "实得 {err}"
        );
        assert!(
            mock.calls.lock().unwrap().is_empty(),
            "预检冲突应零副作用, 实得 {:?}",
            mock.calls.lock().unwrap()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 批内同名碰撞：两个不同目录的同名文件 join 到同一落点 → DuplicateTargetName，零执行。
    #[test]
    fn copy_multi_target_duplicate_basename_aborts() {
        let mut mem = ContextMemory::new();
        // 两个不同目录、同名文件
        let mk = |dir: &str| SearchResult {
            id: format!("{dir}/report.pdf"),
            path: PathBuf::from(format!("{dir}/report.pdf")),
            name: "report.pdf".to_owned(),
            source: BackendKind::Spotlight,
            match_type: MatchType::Filename,
            score: None,
            metadata: SearchResultMetadata::default(),
        };
        mem.record(
            SearchIntent::FileSearch(mk_dummy_file_search()),
            vec![
                mk("/__scout_nonexistent_a__"),
                mk("/__scout_nonexistent_b__"),
            ],
        );

        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock.clone());

        let dir = std::env::temp_dir().join(format!("scout-dup-{}", std::process::id()));

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: Some(Language::Zh),
            action: FileActionKind::Copy,
            target_ref: TargetRef::LastResults {
                selector: TargetSelector::All,
            },
            destination: Some(dir.to_string_lossy().into_owned()),
            new_name: None,
            requires_confirmation: true,
        };

        let err = tool.invoke(&action, &mem).unwrap_err();
        assert!(
            matches!(err, FileActionError::DuplicateTargetName { .. }),
            "实得 {err}"
        );
        assert!(
            mock.calls.lock().unwrap().is_empty(),
            "重名碰撞应零执行, 实得 {:?}",
            mock.calls.lock().unwrap()
        );
    }

    #[test]
    fn open_via_target_ref_path() {
        let mock = Arc::new(MockExecutor::default());
        let tool = mk_tool(mock.clone());
        let context = ContextMemory::new();

        let action = FileAction {
            schema_version: SchemaVersion::V1,
            language: None,
            action: FileActionKind::Open,
            target_ref: TargetRef::Path {
                value: "/Users/r/budget.pdf".to_owned(),
            },
            destination: None,
            new_name: None,
            requires_confirmation: false,
        };
        tool.invoke(&action, &context).unwrap();
        assert_eq!(
            *mock.calls.lock().unwrap(),
            vec![MockCall::Open(PathBuf::from("/Users/r/budget.pdf"))]
        );
    }
}
