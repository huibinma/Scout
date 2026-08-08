//! 用**真实** [`LocalFileActionExecutor`] 对真实临时目录跑 copy/move，
//! 端到端验证方案 A 多目标行为（dir-join + 预检冲突 + 同名碰撞）真落盘。
//!
//! 单元测试用 MockExecutor（不写盘）；本集成测试补上"真文件操作"这块，
//! 替代真机手测中"真 copy/move"环节（UI 确认框 / 真 Spotlight 仍需人工，但
//! 文件操作正确性由此自动化保证）。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use scout_harness::context::ContextMemory;
use scout_harness::file_action_tool::{
    FileActionError, FileActionOutcome, FileActionTool, LocalFileActionExecutor,
};
use scout_harness::PolicyEngine;
use scout_search_backend::{
    BackendKind, FileAction, FileActionKind, FileSearch, Language, MatchType, SchemaVersion,
    SearchIntent, SearchResult, SearchResultMetadata, TargetRef, TargetSelector,
};
use std::path::{Path, PathBuf};

/// 进程内唯一的临时目录名（避免并行测试 / 多次运行碰撞）。
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("scout-real-{}-{tag}-{n}", std::process::id()))
}

fn dummy_file_search() -> FileSearch {
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

fn result_at(path: &Path) -> SearchResult {
    SearchResult {
        id: path.to_string_lossy().into_owned(),
        path: path.to_path_buf(),
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        source: BackendKind::Spotlight,
        match_type: MatchType::Filename,
        score: None,
        metadata: SearchResultMetadata::default(),
    }
}

/// 建一个 ContextMemory，last results 指向给定的真实路径列表。
fn context_with_paths(paths: &[PathBuf]) -> ContextMemory {
    let mut mem = ContextMemory::new();
    let results = paths.iter().map(|p| result_at(p)).collect();
    mem.record(SearchIntent::FileSearch(dummy_file_search()), results);
    mem
}

fn mk_action(kind: FileActionKind, dest_dir: &Path) -> FileAction {
    FileAction {
        schema_version: SchemaVersion::V1,
        language: Some(Language::Zh),
        action: kind,
        target_ref: TargetRef::LastResults {
            selector: TargetSelector::All,
        },
        destination: Some(dest_dir.to_string_lossy().into_owned()),
        new_name: None,
        // requires_confirmation=true → Policy RequireConfirmation 下直接执行（绕过确认返回）
        requires_confirmation: true,
    }
}

fn real_tool() -> FileActionTool {
    FileActionTool::new(std::sync::Arc::new(LocalFileActionExecutor), PolicyEngine)
}

/// 真多目标 copy：2 个不同名源 → 目标目录，各 join basename 真落盘；源保留。
#[test]
fn real_copy_multi_target_lands_all_files() {
    let src = unique_dir("copy-src");
    let dest = unique_dir("copy-dest");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    let a = src.join("alpha.txt");
    let b = src.join("beta.txt");
    std::fs::write(&a, b"AAA").unwrap();
    std::fs::write(&b, b"BBB").unwrap();

    let ctx = context_with_paths(&[a.clone(), b.clone()]);
    let tool = real_tool();
    let outcome = tool
        .invoke(&mk_action(FileActionKind::Copy, &dest), &ctx)
        .unwrap();

    let FileActionOutcome::Executed { affected } = outcome else {
        panic!("expected Executed, got {outcome:?}")
    };
    assert_eq!(affected.len(), 2);

    // 两个文件都真落到 dest/basename
    assert_eq!(std::fs::read(dest.join("alpha.txt")).unwrap(), b"AAA");
    assert_eq!(std::fs::read(dest.join("beta.txt")).unwrap(), b"BBB");
    // copy 不动源
    assert!(a.exists() && b.exists(), "copy 不应删除源");

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dest);
}

/// 真多目标 move：2 个源 → 目标目录落盘，源被移走。
#[test]
fn real_move_multi_target_relocates_all_files() {
    let src = unique_dir("move-src");
    let dest = unique_dir("move-dest");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    let a = src.join("one.dat");
    let b = src.join("two.dat");
    std::fs::write(&a, b"1").unwrap();
    std::fs::write(&b, b"2").unwrap();

    let ctx = context_with_paths(&[a.clone(), b.clone()]);
    let tool = real_tool();
    let outcome = tool
        .invoke(&mk_action(FileActionKind::Move, &dest), &ctx)
        .unwrap();

    let FileActionOutcome::Executed { affected } = outcome else {
        panic!("expected Executed, got {outcome:?}")
    };
    assert_eq!(affected.len(), 2);

    assert_eq!(std::fs::read(dest.join("one.dat")).unwrap(), b"1");
    assert_eq!(std::fs::read(dest.join("two.dat")).unwrap(), b"2");
    // move 移走源
    assert!(!a.exists() && !b.exists(), "move 应删除源");

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dest);
}

/// 真预检冲突：目标目录已有同名文件 → PathConflict，且其余文件未被创建（原子）。
#[test]
fn real_copy_preflight_conflict_is_atomic() {
    let src = unique_dir("conf-src");
    let dest = unique_dir("conf-dest");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    let a = src.join("keep.txt");
    let b = src.join("clash.txt");
    std::fs::write(&a, b"A").unwrap();
    std::fs::write(&b, b"B").unwrap();
    // dest 已存在与第 2 个源同名的文件
    std::fs::write(dest.join("clash.txt"), b"EXISTING").unwrap();

    let ctx = context_with_paths(&[a.clone(), b.clone()]);
    let tool = real_tool();
    let err = tool
        .invoke(&mk_action(FileActionKind::Copy, &dest), &ctx)
        .unwrap_err();
    assert!(
        matches!(err, FileActionError::PathConflict { .. }),
        "实得 {err}"
    );

    // 原子：第 1 个文件（keep.txt）不该被复制进 dest
    assert!(
        !dest.join("keep.txt").exists(),
        "预检冲突应零副作用，不该创建 keep.txt"
    );
    // 已存在文件内容未被改动
    assert_eq!(std::fs::read(dest.join("clash.txt")).unwrap(), b"EXISTING");

    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&dest);
}

/// 真同名碰撞：两个不同目录的同名文件 → DuplicateTargetName，零落盘（防静默覆盖）。
#[test]
fn real_copy_duplicate_basename_aborts_no_write() {
    let src_a = unique_dir("dup-a");
    let src_b = unique_dir("dup-b");
    let dest = unique_dir("dup-dest");
    std::fs::create_dir_all(&src_a).unwrap();
    std::fs::create_dir_all(&src_b).unwrap();
    std::fs::create_dir_all(&dest).unwrap();
    let a = src_a.join("report.pdf");
    let b = src_b.join("report.pdf");
    std::fs::write(&a, b"A").unwrap();
    std::fs::write(&b, b"B").unwrap();

    let ctx = context_with_paths(&[a.clone(), b.clone()]);
    let tool = real_tool();
    let err = tool
        .invoke(&mk_action(FileActionKind::Copy, &dest), &ctx)
        .unwrap_err();
    assert!(
        matches!(err, FileActionError::DuplicateTargetName { .. }),
        "实得 {err}"
    );
    // 零落盘：dest 里不该出现 report.pdf
    assert!(
        !dest.join("report.pdf").exists(),
        "同名碰撞应整体中止，不该写任何文件"
    );

    let _ = std::fs::remove_dir_all(&src_a);
    let _ = std::fs::remove_dir_all(&src_b);
    let _ = std::fs::remove_dir_all(&dest);
}
