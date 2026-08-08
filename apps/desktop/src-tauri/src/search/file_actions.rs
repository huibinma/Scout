//! 文件操作处理（open/locate/confirm/cancel）+ 审计记录（从 search.rs 拆出，逻辑零改动）。

use std::sync::{Arc, Mutex};
use std::time::Instant;

use scout_harness::context::{ContextMemory, TargetRefError};
use scout_harness::file_action_tool::FileActionError;
use scout_harness::Tool;
use tauri::ipc::Channel;

use super::{ActionDoneData, SearchDeps, SearchEvent};

/// 处理 `FileAction`:
/// - `Open`/`Locate` 立即经 [`FileActionTool`] 执行,结果走 [`SearchEvent::ActionDone`]。
/// - `Copy`/`Move`/`Rename` 走确认往返:委托 [`handle_confirmable_action`] 存 pending + 发 ConfirmAction。
/// - `Delete` 拒绝。
///
/// 不 `record` / `clear` context —— action 无搜索结果,只读保证连续 action
/// 引用同一搜索基准。
pub(crate) async fn handle_file_action(
    action: scout_search_backend::FileAction,
    on_event: Channel<SearchEvent>,
    deps: &SearchDeps,
) -> Result<(), String> {
    use scout_search_backend::FileActionKind;

    // scope gate:open/locate 立即执行;copy/move/rename 走确认往返;delete 拒绝。
    match action.action {
        FileActionKind::Open | FileActionKind::Locate => {}
        FileActionKind::Copy | FileActionKind::Move | FileActionKind::Rename => {
            return handle_confirmable_action(action, on_event, &deps.pending, &deps.context).await;
        }
        FileActionKind::Delete => {
            eprintln!("search: delete 不支持");
            let _ = on_event.send(SearchEvent::Error {
                message: "删除操作不支持".to_owned(),
            });
            return Ok(());
        }
    }

    let tool_id = deps.file_action_tool.id().to_owned();
    let tool_start = Instant::now();

    // 2) tool_call trace
    deps.tracer.on_tool_call(&scout_harness::ToolCallEvent {
        tool_id: tool_id.clone(),
        tool_kind: scout_harness::ToolKind::FileAction,
        intent_variant: scout_harness::SupportedIntent::FileAction,
    });

    // 3) invoke(只读 context guard)
    let outcome = {
        let guard = deps.context.lock().unwrap_or_else(|e| e.into_inner());
        deps.file_action_tool.invoke(&action, &guard)
    };

    // BETA-06：执行点记审计（Executed/Failed 记，RequiresConfirmation 不记）。
    record_audit(deps.audit().as_ref(), &action, &outcome);

    match outcome {
        Ok(scout_harness::file_action_tool::FileActionOutcome::Executed { affected }) => {
            deps.tracer.on_tool_result(&scout_harness::ToolResultEvent {
                tool_id,
                duration: tool_start.elapsed(),
                result_count: affected.len(),
            });
            let action_kind = format!("{:?}", action.action).to_lowercase();
            let paths: Vec<String> = affected
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            let _ = on_event.send(SearchEvent::ActionDone { action_kind, paths });
            Ok(())
        }
        Ok(scout_harness::file_action_tool::FileActionOutcome::RequiresConfirmation { .. }) => {
            deps.tracer.on_error(&scout_harness::ToolErrorEvent {
                tool_id,
                duration: tool_start.elapsed(),
                error_type: "UnexpectedRequiresConfirmation".to_owned(),
            });
            eprintln!("search: open/locate 不应触发 RequiresConfirmation");
            let _ = on_event.send(SearchEvent::Error {
                message: "该操作暂不支持(确认流待后续阶段)".to_owned(),
            });
            Ok(())
        }
        Err(err) => {
            deps.tracer.on_error(&scout_harness::ToolErrorEvent {
                tool_id,
                duration: tool_start.elapsed(),
                error_type: file_action_error_kind(&err).to_owned(),
            });
            let _ = on_event.send(SearchEvent::Error {
                message: friendly_file_action_message(&err),
            });
            Ok(())
        }
    }
}

/// 处理 `FileAction(Copy/Move/Rename)` 首次下发:只读 [`ContextMemory`] 解析 target_ref、
/// copy/move 允许多目标(受 batch 上限保护),rename 维持单目标;
/// copy/move 解析 destination 为目录(展开 ~,不 join basename),构造自包含 pending
/// (`target_ref=Paths`)存入 `pending` 槽,发 [`SearchEvent::ConfirmAction`]。
///
/// 首次下发**不调 invoke、不进 trace**(pre-tool)。实际执行在 confirm_action。
pub(crate) async fn handle_confirmable_action(
    action: scout_search_backend::FileAction,
    on_event: Channel<SearchEvent>,
    pending: &Arc<Mutex<Option<scout_search_backend::FileAction>>>,
    context: &Arc<Mutex<ContextMemory>>,
) -> Result<(), String> {
    use scout_search_backend::{FileAction, FileActionKind, TargetRef};

    // 1) 解析 target_ref(只读 context)
    let targets = {
        let guard = context.lock().unwrap_or_else(|e| e.into_inner());
        guard.resolve_target_ref(&action.target_ref)
    };
    let targets = match targets {
        Ok(t) => t,
        Err(e) => {
            let _ = on_event.send(SearchEvent::Error {
                message: friendly_file_action_message(&FileActionError::TargetRef(e)),
            });
            return Ok(());
        }
    };

    // 2) batch 上限(copy/move 多目标)：超过 harness 默认阈值直接友好错误
    use scout_harness::file_action_tool::DEFAULT_BATCH_THRESHOLD;
    if targets.is_empty() {
        let _ = on_event.send(SearchEvent::Error {
            message: "没有可操作的目标".to_owned(),
        });
        return Ok(());
    }

    // rename：维持单目标(N 文件改 1 名无意义)
    if matches!(action.action, FileActionKind::Rename) && targets.len() != 1 {
        let _ = on_event.send(SearchEvent::Error {
            message: "一次只能重命名单个文件(多文件待后续)".to_owned(),
        });
        return Ok(());
    }
    // copy/move：放开多目标，但受 batch 上限保护
    if matches!(action.action, FileActionKind::Copy | FileActionKind::Move)
        && targets.len() > DEFAULT_BATCH_THRESHOLD
    {
        let _ = on_event.send(SearchEvent::Error {
            message: format!("目标过多(最多 {DEFAULT_BATCH_THRESHOLD} 个),请缩小范围"),
        });
        return Ok(());
    }

    // 3) copy/move 解析 destination 目录(展开 ~,不 join basename)；rename 取 new_name
    let (destination, new_name) = match action.action {
        FileActionKind::Copy | FileActionKind::Move => {
            let hint = match action.destination.as_deref() {
                Some(h) if !h.is_empty() => h,
                _ => {
                    let _ = on_event.send(SearchEvent::Error {
                        message: "无法确定目标位置".to_owned(),
                    });
                    return Ok(());
                }
            };
            match resolve_destination_dir(hint) {
                Ok(p) => (Some(p.to_string_lossy().into_owned()), None),
                Err(msg) => {
                    let _ = on_event.send(SearchEvent::Error { message: msg });
                    return Ok(());
                }
            }
        }
        FileActionKind::Rename => match action.new_name.as_deref() {
            Some(n) if !n.is_empty() => (None, Some(n.to_owned())),
            _ => {
                let _ = on_event.send(SearchEvent::Error {
                    message: "未指定新文件名".to_owned(),
                });
                return Ok(());
            }
        },
        other => {
            unreachable!("handle_confirmable_action 只应处理 copy/move/rename, 实得 {other:?}")
        }
    };

    // 4) 构造自包含 pending(target_ref=Paths,确认时 invoke 不依赖 context)
    let path_strs: Vec<String> = targets
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let pending_action = FileAction {
        schema_version: action.schema_version,
        language: action.language,
        action: action.action,
        target_ref: TargetRef::Paths {
            values: path_strs.clone(),
        },
        destination: destination.clone(),
        new_name: new_name.clone(),
        requires_confirmation: true,
    };
    *pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(pending_action);

    // 5) 发 ConfirmAction(paths=全部源,destination=目录)
    let action_kind = format!("{:?}", action.action).to_lowercase();
    let _ = on_event.send(SearchEvent::ConfirmAction {
        action_kind,
        paths: path_strs,
        destination,
        new_name,
    });
    Ok(())
}

/// `confirm_action` command 主体:take pending → invoke 执行 → 返回 [`ActionDoneData`]。
/// 不依赖 [`tauri::State`],可单测。pending 为 None 返 Err。
pub(crate) fn confirm_action_impl(deps: &SearchDeps) -> Result<ActionDoneData, String> {
    let action = {
        let mut guard = deps.pending.lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    };
    let action = action.ok_or_else(|| "没有待确认的操作".to_owned())?;

    let tool_id = deps.file_action_tool.id().to_owned();
    let tool_start = Instant::now();
    deps.tracer.on_tool_call(&scout_harness::ToolCallEvent {
        tool_id: tool_id.clone(),
        tool_kind: scout_harness::ToolKind::FileAction,
        intent_variant: scout_harness::SupportedIntent::FileAction,
    });

    let outcome = {
        let guard = deps.context.lock().unwrap_or_else(|e| e.into_inner());
        deps.file_action_tool.invoke(&action, &guard)
    };

    // BETA-06：执行点记审计（Executed/Failed 记，RequiresConfirmation 不记）。
    record_audit(deps.audit().as_ref(), &action, &outcome);

    match outcome {
        Ok(scout_harness::file_action_tool::FileActionOutcome::Executed { affected }) => {
            deps.tracer.on_tool_result(&scout_harness::ToolResultEvent {
                tool_id,
                duration: tool_start.elapsed(),
                result_count: affected.len(),
            });
            Ok(ActionDoneData {
                action_kind: format!("{:?}", action.action).to_lowercase(),
                paths: affected
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
            })
        }
        Ok(scout_harness::file_action_tool::FileActionOutcome::RequiresConfirmation { .. }) => {
            deps.tracer.on_error(&scout_harness::ToolErrorEvent {
                tool_id,
                duration: tool_start.elapsed(),
                error_type: "UnexpectedRequiresConfirmation".to_owned(),
            });
            Err("操作未能确认执行".to_owned())
        }
        Err(err) => {
            deps.tracer.on_error(&scout_harness::ToolErrorEvent {
                tool_id,
                duration: tool_start.elapsed(),
                error_type: file_action_error_kind(&err).to_owned(),
            });
            Err(friendly_file_action_message(&err))
        }
    }
}

/// `cancel_action` command 主体:清空 pending 槽。不依赖 `tauri::State`,可单测。
pub(crate) fn cancel_action_impl(pending: &Arc<Mutex<Option<scout_search_backend::FileAction>>>) {
    *pending.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 按绝对路径执行 open/locate 的共享实现(供 UI 双击打开 / 右键定位)。
///
/// 复用 [`FileActionTool`](scout_harness::file_action_tool::FileActionTool)
/// (内置 [`PolicyEngine`])而非裸开路径:open/locate 在默认 policy 下为 Allow,
/// 无需确认。`target_ref` 用 [`TargetRef::Path`](scout_search_backend::TargetRef::Path)
/// 直接携带绝对路径,**不依赖上一轮搜索结果**,context 仅传入做只读占位。
pub(crate) fn run_path_action(
    kind: scout_search_backend::FileActionKind,
    path: String,
    deps: &SearchDeps,
) -> Result<ActionDoneData, String> {
    use scout_search_backend::{FileAction, FileActionKind, SchemaVersion, TargetRef};

    // 仅允许 open/locate;其余动作必须走确认流,绝不从 UI 旁路执行。
    if !matches!(kind, FileActionKind::Open | FileActionKind::Locate) {
        return Err("仅支持打开 / 在文件夹中显示".to_owned());
    }

    let action = FileAction {
        schema_version: SchemaVersion::V1,
        language: None,
        action: kind,
        target_ref: TargetRef::Path { value: path },
        destination: None,
        new_name: None,
        requires_confirmation: false,
    };

    let tool_id = deps.file_action_tool.id().to_owned();
    let tool_start = Instant::now();
    deps.tracer.on_tool_call(&scout_harness::ToolCallEvent {
        tool_id: tool_id.clone(),
        tool_kind: scout_harness::ToolKind::FileAction,
        intent_variant: scout_harness::SupportedIntent::FileAction,
    });

    let outcome = {
        let guard = deps.context.lock().unwrap_or_else(|e| e.into_inner());
        deps.file_action_tool.invoke(&action, &guard)
    };

    // BETA-06：执行点记审计（Executed/Failed 记，RequiresConfirmation 不记）。
    record_audit(deps.audit().as_ref(), &action, &outcome);

    match outcome {
        Ok(scout_harness::file_action_tool::FileActionOutcome::Executed { affected }) => {
            deps.tracer.on_tool_result(&scout_harness::ToolResultEvent {
                tool_id,
                duration: tool_start.elapsed(),
                result_count: affected.len(),
            });
            Ok(ActionDoneData {
                action_kind: format!("{:?}", action.action).to_lowercase(),
                paths: affected
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
            })
        }
        Ok(scout_harness::file_action_tool::FileActionOutcome::RequiresConfirmation { .. }) => {
            deps.tracer.on_error(&scout_harness::ToolErrorEvent {
                tool_id,
                duration: tool_start.elapsed(),
                error_type: "UnexpectedRequiresConfirmation".to_owned(),
            });
            Err("该操作需要确认".to_owned())
        }
        Err(err) => {
            deps.tracer.on_error(&scout_harness::ToolErrorEvent {
                tool_id,
                duration: tool_start.elapsed(),
                error_type: file_action_error_kind(&err).to_owned(),
            });
            Err(friendly_file_action_message(&err))
        }
    }
}

/// 返回 FileActionError variant 名,不含 detail(避免泄路径)。供 trace 用。
pub(crate) fn file_action_error_kind(err: &FileActionError) -> &'static str {
    match err {
        FileActionError::DeleteNotSupported => "DeleteNotSupported",
        FileActionError::PolicyDenied { .. } => "PolicyDenied",
        FileActionError::TargetRef(TargetRefError::NoLastResults) => "NoLastResults",
        FileActionError::TargetRef(TargetRefError::IndexOutOfRange { .. }) => "IndexOutOfRange",
        FileActionError::TargetRef(TargetRefError::EmptyIndices) => "EmptyIndices",
        FileActionError::EmptyTargets => "EmptyTargets",
        FileActionError::BatchThresholdExceeded { .. } => "BatchThresholdExceeded",
        FileActionError::MissingDestination => "MissingDestination",
        FileActionError::MissingNewName => "MissingNewName",
        FileActionError::PathConflict { .. } => "PathConflict",
        FileActionError::DuplicateTargetName { .. } => "DuplicateTargetName",
        FileActionError::Executor(_) => "Executor",
    }
}

/// BETA-06：文件操作执行后记一条审计。仅 Executed/Failed 记；RequiresConfirmation 未执行不记。
pub(crate) fn record_audit(
    audit: &dyn scout_harness::AuditLog,
    action: &scout_search_backend::FileAction,
    outcome: &Result<scout_harness::file_action_tool::FileActionOutcome, FileActionError>,
) {
    use scout_harness::file_action_tool::FileActionOutcome;
    use scout_harness::{AuditEntry, AuditOperation, AuditResult};
    use scout_search_backend::FileActionKind;

    let operation = match action.action {
        FileActionKind::Open => AuditOperation::Open,
        FileActionKind::Locate => AuditOperation::Locate,
        FileActionKind::Copy => AuditOperation::Copy,
        FileActionKind::Move => AuditOperation::Move,
        FileActionKind::Rename => AuditOperation::Rename,
        FileActionKind::Delete => return, // delete 永不执行，不审计
    };
    let (result, source_paths, error) = match outcome {
        Ok(FileActionOutcome::Executed { affected }) => (
            AuditResult::Executed,
            affected.iter().map(|p| p.display().to_string()).collect(),
            None,
        ),
        // 未执行（待确认）不记。
        Ok(FileActionOutcome::RequiresConfirmation { .. }) => return,
        Err(e) => (
            AuditResult::Failed,
            action_self_contained_paths(action),
            Some(file_action_error_kind(e).to_owned()),
        ),
    };
    audit.record(&AuditEntry {
        timestamp: chrono::Utc::now(),
        operation,
        source_paths,
        destination: action.destination.clone(),
        new_name: action.new_name.clone(),
        result,
        error,
    });
}

/// 从 action 的自包含 target_ref（Path/Paths）取源路径；LastResults（需 context 解析）取不到返空。
pub(crate) fn action_self_contained_paths(
    action: &scout_search_backend::FileAction,
) -> Vec<String> {
    use scout_search_backend::TargetRef;
    match &action.target_ref {
        TargetRef::Path { value } => vec![value.clone()],
        TargetRef::Paths { values } => values.clone(),
        TargetRef::LastResults { .. } => Vec::new(),
    }
}

/// 把 parser 的 destination(如 "~/Desktop")展开为绝对**目录**。
/// basename join 下放 harness `execute_one`(方案 A,逐目标 join)。
pub(crate) fn resolve_destination_dir(dest_hint: &str) -> Result<std::path::PathBuf, String> {
    expand_tilde(dest_hint)
}

/// 展开 `~` / `~/...` 到 home 目录;非 `~` 开头原样。
pub(crate) fn expand_tilde(p: &str) -> Result<std::path::PathBuf, String> {
    if let Some(rest) = p.strip_prefix("~/") {
        Ok(home_dir()?.join(rest))
    } else if p == "~" {
        home_dir()
    } else {
        Ok(std::path::PathBuf::from(p))
    }
}

/// home 目录:HOME(unix)→ USERPROFILE(windows)兜底。
pub(crate) fn home_dir() -> Result<std::path::PathBuf, String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .ok_or_else(|| "无法获取主目录".to_owned())
}

/// 把 FileActionError 映射成面向用户的中文友好文案。
pub(crate) fn friendly_file_action_message(err: &FileActionError) -> String {
    match err {
        FileActionError::TargetRef(TargetRefError::NoLastResults) => {
            "没有可操作的上一轮搜索结果,请先发起一次搜索".to_owned()
        }
        FileActionError::TargetRef(TargetRefError::IndexOutOfRange {
            requested,
            available,
        }) => format!("第 {requested} 个结果不存在(上一轮共 {available} 条)"),
        FileActionError::TargetRef(TargetRefError::EmptyIndices) => {
            "未指定要操作的结果序号".to_owned()
        }
        FileActionError::EmptyTargets => "没有可操作的目标".to_owned(),
        FileActionError::PathConflict { dest } => {
            format!("目标已存在:{}", dest.display())
        }
        FileActionError::DuplicateTargetName { dest } => {
            format!("有多个同名文件,无法同时操作到:{}", dest.display())
        }
        FileActionError::Executor(io) => format!("操作失败:{io}"),
        // open/locate 路径不可达的错误,兜底用 Display
        other => other.to_string(),
    }
}
