//! 增量索引：walkdir 遍历 + mtime 比对 + 磁盘删除回收。
//!
//! [`run_incremental_index`] 是 BETA-01 音乐 / BETA-02 文档共用的通用增量骨架，
//! 仅「扩展名白名单 + 提取器 + 存储」不同，经 [`IncrementalStore`] trait 抽象。

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, Instant, UNIX_EPOCH};

use globset::{Glob, GlobSet, GlobSetBuilder};
use walkdir::WalkDir;

use crate::db::MusicIndex;
use crate::model::{DocumentEntry, ExtractedDoc, IndexStats, MusicEntry};
use crate::progress::{IndexProgress, NoopProgress, StageTimings};
use crate::IndexError;

/// 并行提取分块大小。进度 `on_file` 已下沉到并行段逐文件上报（见下），故分块**只用于
/// 限制内存尖峰**（一次最多驻留一块的正文提取结果），不影响进度粒度；峰值并行度由下方
/// 受限线程池（[`EXTRACT_PARALLELISM`]）决定、与本值无关。取中等值兼顾内存与串行写库批量。
const EXTRACT_CHUNK: usize = 64;

/// 提取阶段并发上限（**重量级**默认值，见 [`extract_parallelism_for`]）。**关键**：扫描版
/// PDF 的提取会 spawn `pdftoppm` 渲染整份 PDF（一份一进程、200 DPI 很吃 CPU/内存）再逐页
/// OCR（又各 spawn 子进程）。若按核数无限并行，多份扫描 PDF 会瞬间起几十个 `pdftoppm`/OCR
/// 子进程互抢 CPU/IO，机器被打爆、整体反而更慢（v0.9.27 真机首索引 50 文件卡「0/50」
/// 十分钟、17 个 pdftoppm 并发实锤）。故用**受限线程池**把同时进行的重量级提取压到少数
/// 几个：既保留多文件并行的提速，又不放任子进程风暴。
const EXTRACT_PARALLELISM: usize = 4;

/// 提取线程池栈大小（64 MiB）。**关键**：pdf-extract / lopdf 解析深层嵌套或畸形 PDF 时
/// 递归很深，rayon worker 的 std 默认栈（Windows ≈2 MiB）会被撑爆 → `STATUS_STACK_OVERFLOW`
/// （0xC00000FD）。栈溢出是 SEH 异常、**不是 panic**，`catch_extract` 的 `catch_unwind`
/// 与 release 的 `panic = "unwind"` 都兜不住，会直接 abort 整个 app（真机撞：偶发、单机、
/// 故障模块为 exe 自身、偏移固定）。并发已被 [`EXTRACT_PARALLELISM`] 限死，栈按需提交，
/// 64 MiB×少数线程的保留成本可忽略。
const EXTRACT_STACK_SIZE: usize = 64 * 1024 * 1024;

/// BETA-64 T3：轻量提取轮（音乐标签，见下）并发上限的 env 覆盖名。
const EXTRACT_PARALLELISM_LIGHT_ENV: &str = "SCOUT_EXTRACT_PARALLELISM_LIGHT";
/// BETA-64 T3：重量级提取轮（文档/图片，见下）并发上限的 env 覆盖名。
const EXTRACT_PARALLELISM_HEAVY_ENV: &str = "SCOUT_EXTRACT_PARALLELISM_HEAVY";

/// 解析 env 覆盖值：未设置 / 非法 / 0 一律忽略（回退默认值），不 panic。
fn env_parallelism_override(key: &str) -> Option<usize> {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
}

/// BETA-64 T3：按本轮扩展名白名单分级并发预算。[`EXTRACT_PARALLELISM`]=4 是为扫描版 PDF /
/// 图片 OCR 这类会 spawn `pdftoppm`/OCR 子进程的重量级提取设的保守值（详见其文档注释的
/// v0.9.27 子进程风暴故障）；但同一常量此前也无差别地套在音乐标签提取（`lofty`，纯内存
/// 解析、不 spawn 任何子进程）上，高核数机器上白白浪费并行度。本函数按 `exts` 是否为
/// [`MUSIC_EXTS`] 走两档：音乐轮按可用核数放开（不受子进程风暴风险约束）；其余轮
/// （`DOC_EXTS`/`IMAGE_EXTS`，可能触发 OCR/pdftoppm）沿用保守上限。两档均可用
/// `SCOUT_EXTRACT_PARALLELISM_LIGHT`/`_HEAVY` env 覆盖，供用户按自己机器调优而不必重新编译。
fn extract_parallelism_for(exts: &[&str]) -> usize {
    let cores = std::thread::available_parallelism()
        .map_or(EXTRACT_PARALLELISM, std::num::NonZeroUsize::get);
    if exts == MUSIC_EXTS {
        env_parallelism_override(EXTRACT_PARALLELISM_LIGHT_ENV)
            .unwrap_or(cores)
            .min(cores)
    } else {
        env_parallelism_override(EXTRACT_PARALLELISM_HEAVY_ENV)
            .unwrap_or(EXTRACT_PARALLELISM)
            .min(cores)
    }
}

/// duration 转毫秒 `u64`（tracing 字段类型；`as_millis()` 返回 `u128`，理论溢出夹到
/// `u64::MAX`，索引单轮耗时不可能触达此上限）。
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// 增量索引的存储后端抽象（音乐 / 文档各 impl 一份）。
pub(crate) trait IncrementalStore {
    /// 一条提取结果（音乐为 `MusicEntry`，文档为 `(DocumentEntry, body)`）。
    type Entry;

    /// 取某 path 已索引的 `modified_time`，不存在返回 `None`。
    fn modified_time_of(&self, path: &str) -> Result<Option<i64>, IndexError>;
    /// 批量取索引中落在 `roots` 任一子树下的全部 `(path, modified_time)`（BETA-64 T7a）。
    /// 增量扫描的 mtime 比对此前逐文件调 [`Self::modified_time_of`]（一次 DB 往返一个文件，
    /// walk 阶段耗时随文件数线性叠加、且大多数文件是"未变化 → 白查一次"）；本方法一次性
    /// 查出整棵子树的已索引 mtime、扫描时改成内存 `HashMap` 查找。返回值同时供**回收阶段**
    /// 复用（key 集合 = 旧版 `paths_under` 的返回值），省掉扫描结束后再跑一次全表扫描。
    /// 默认回退到 `paths_under` + 逐条 `modified_time_of`（行为等价、无性能收益——
    /// 供未覆写的 `IncrementalStore` 实现兜底，当前 `MusicIndex`/`DocumentIndex` 均已覆写）。
    fn modified_times_under(&self, roots: &[String]) -> Result<HashMap<String, i64>, IndexError> {
        let mut map = HashMap::new();
        for p in self.paths_under(roots)? {
            if let Some(mt) = self.modified_time_of(&p)? {
                map.insert(p, mt);
            }
        }
        Ok(map)
    }
    /// 插入或更新。返回 `true` 表示新增、`false` 表示更新。
    fn upsert_entry(&self, entry: &Self::Entry) -> Result<bool, IndexError>;
    /// 批量插入或更新（BETA-64 T2）：默认回退到逐条 `upsert_entry`（各自独立事务，
    /// 行为与改造前逐字节等价）；`MusicIndex`/`DocumentIndex` 覆写为**单个事务**批量提交，
    /// 减少大批量索引时「每文件一次 commit」的固定开销。返回值与入参一一对应，
    /// 语义同 `upsert_entry`（`true`=新增 / `false`=更新）。
    fn upsert_entries(&self, entries: &[Self::Entry]) -> Result<Vec<bool>, IndexError> {
        entries.iter().map(|e| self.upsert_entry(e)).collect()
    }
    /// 索引中 path 落在 `roots` 任一子树下的所有记录路径。
    fn paths_under(&self, roots: &[String]) -> Result<Vec<String>, IndexError>;
    /// 按 path 删除一条记录。返回是否删到了行。
    fn delete_by_path(&self, path: &str) -> Result<bool, IndexError>;

    // ===== 文件级提取失败留痕（BETA-40 收尾，2026-07-04）=====
    // 此前整份文件提取失败只累计 `IndexStats.failed`，哪个文件、什么原因均不落库——
    // 企业取证场景（审计留痕）无法复核。默认 no-op（音乐库暂不留痕），文档库落
    // `index_failures` 表。

    /// 记录（或刷新）一条文件级提取失败留痕，连带记下失败时刻的 `modified_time`。默认 no-op。
    fn record_extract_failure(
        &self,
        _path: &str,
        _reason: &str,
        _modified_time: i64,
    ) -> Result<(), IndexError> {
        Ok(())
    }
    /// 清除某 path 的失败留痕（该文件后来提取成功 / 已从磁盘删除）。默认 no-op。
    fn clear_extract_failure(&self, _path: &str) -> Result<(), IndexError> {
        Ok(())
    }
    /// 失败留痕中落在 `roots` 任一子树下的 path（回收已删文件的留痕用）。默认空。
    fn failure_paths_under(&self, _roots: &[String]) -> Result<Vec<String>, IndexError> {
        Ok(Vec::new())
    }
    /// 2026-07-29：全部失败留痕的 `(path, modified_time)`（同 [`Self::record_extract_failure`]
    /// 记录时的 mtime）——增量循环据此判定"上次失败后文件是否真的变过"，未变则跳过
    /// 重新提取（同一批必然再失败的文件——加密文档/权限过期——此前每轮增量都会重新
    /// 试一次，白白耗时；见 `crate::doc_extract` 加密 PDF 场景）。默认空（音乐库暂不留痕）。
    fn all_failure_modified_times(&self) -> Result<HashMap<String, i64>, IndexError> {
        Ok(HashMap::new())
    }
}

/// 文件扩展名是否在白名单内（大小写不敏感）。
fn has_ext(path: &Path, exts: &[&str]) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| exts.contains(&ext.as_str()))
}

/// Office（Word/Excel/PowerPoint）打开文档期间在同目录生成的隐藏锁文件
/// （固定前缀 `~$`，如 `~$报告.docx`）：内容是几百字节的 owner-info 占位数据，
/// 不是合法 zip/OOXML。当作正常 docx/xlsx/pptx 提取会必然报
/// `invalid Zip archive: Could not find EOCD`，误计一条 failed
/// （用户反馈复现，推断为常开 Office 文档时索引批量命中此报错——待真机复测确认）。
/// `~$` 前缀是 Office 固定约定、不会出现在真实文档文件名首两字符，按 basename 跳过安全。
fn is_office_lock_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("~$"))
}

#[cfg(test)]
mod office_lock_file_tests {
    use super::*;

    #[test]
    fn detects_office_lock_file_prefix() {
        assert!(is_office_lock_file(Path::new("/docs/~$报告.docx")));
        assert!(is_office_lock_file(Path::new("~$slides.pptx")));
    }

    // `\` 只在 Windows 上是路径分隔符——Unix 构建下 `Path::file_name()` 会把整段
    // `C:\docs\~$Report.xlsx` 当成单个不透明分量，不会拆出 `~$Report.xlsx`，
    // 故这条反斜杠场景只在 Windows target 下有意义（真机走 WalkDir 原生分隔符，无此问题）。
    #[cfg(windows)]
    #[test]
    fn detects_office_lock_file_prefix_windows_backslash() {
        assert!(is_office_lock_file(Path::new(r"C:\docs\~$Report.xlsx")));
    }

    #[test]
    fn keeps_normal_documents() {
        assert!(!is_office_lock_file(Path::new("/docs/报告.docx")));
        assert!(!is_office_lock_file(Path::new("/docs/$budget.xlsx")));
        assert!(!is_office_lock_file(Path::new("/docs/report~1.docx")));
    }
}

/// 把目录名 glob 编译成 basename 匹配的 GlobSet。非法 glob 跳过 + 记日志，不中断。
/// 空输入 → 空 GlobSet（is_match 恒 false → 无排除）。
///
/// 本 crate 无日志 facade，非法 glob 经 stderr 提示（与提取器 panic hook 同走 stderr），
/// 故局部放行 `print_stderr`；静默丢弃会让用户的配置笔误无从察觉。
#[must_use]
#[allow(clippy::print_stderr)]
pub fn build_exclude_set(globs: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for g in globs {
        match Glob::new(g) {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(e) => eprintln!("排除 glob 非法，已跳过 `{g}`: {e}"),
        }
    }
    builder.build().unwrap_or_else(|_| GlobSet::empty())
}

// cycle 7-b：旧 `is_excluded_dir` 函数已被 `ExcludeFilter::is_excluded_dir` 取代
// （通过 `ExcludeFilter::from_basename_set` 委托实现 basename-only 兼容）。旧签名保
// 留仅测试参考、无生产 caller、故删除。

/// BETA-33 cycle 7-b：两层排除过滤器（basename 全局 + per-root 相对路径 glob）。
///
/// **兼容层设计**（Codex §10 OBJECT 2）：
/// - 旧 API `index_dirs_excluding(..., &GlobSet)` 保留、内部走 [`ExcludeFilter::from_basename_set`]
///   委托 [`ExcludeFilter::is_excluded_dir`]——BETA-27 basename-only 行为 byte-for-byte 保留。
/// - 新代码（桌面 `perform_reindex` 等）直接构造 `ExcludeFilter::build(exclude_globs, root_excludes)`
///   走 filter API，同时享受全局 basename 排除 + per-root 子路径排除。
///
/// **相对 root 路径匹配**（Codex §10 SUGGEST 1/3）：
/// - Windows：`entry.path().strip_prefix(root)` 后 `\\` → `/` 归一，让 pattern `临时/**` 能匹配 `临时\a`
/// - glob 边界补充：以 `/**` 结尾的 pattern 自动追加去掉 `/**` 的目录本身 pattern
///   （否则 walkdir 遍历到 dir entry `临时` 时未命中、剪枝失效）
///
/// **性能**：per_root vec 短（真机 <10 root），每 entry 常数级比较；仅目录 entry 进 filter
/// （文件 entry 不调 `is_excluded_dir`），alloc 开销可忽略。
#[derive(Debug, Clone, Default)]
pub struct ExcludeFilter {
    pub basename: GlobSet,
    /// key = normalize_root_key 后的 root、value = 编译后的 relative path glob set
    pub per_root: Vec<(String, GlobSet)>,
}

impl ExcludeFilter {
    /// 兼容构造：仅从 basename `GlobSet` 建 filter（per_root 空）。
    /// 旧 `index_dirs_excluding` 等 API 内部走这个。
    #[must_use]
    pub fn from_basename_set(gs: &GlobSet) -> Self {
        Self {
            basename: gs.clone(),
            per_root: Vec::new(),
        }
    }

    /// 生产构造：从 exclude_globs（basename）+ root_excludes（per-root）构建。
    /// `normalize` 参数是 root 归一化 fn（避免本 crate 直接依赖 desktop 侧 `normalize_root_key`）。
    ///
    /// 每个 root_excludes 项的 patterns 里，以 `/**` 结尾的会自动加去掉 `/**` 的
    /// 目录本身 pattern（Codex §10 SUGGEST 3）。非法 glob 跳过 + stderr 提示（同 basename）。
    #[must_use]
    #[allow(clippy::print_stderr)]
    pub fn build<F>(
        exclude_globs: &[String],
        root_excludes: &[(String, Vec<String>)],
        normalize: F,
    ) -> Self
    where
        F: Fn(&str) -> String,
    {
        let basename = build_exclude_set(exclude_globs);
        let mut per_root: Vec<(String, GlobSet)> = Vec::with_capacity(root_excludes.len());
        for (root, patterns) in root_excludes {
            if patterns.is_empty() {
                continue;
            }
            let mut builder = GlobSetBuilder::new();
            for p in patterns {
                match Glob::new(p) {
                    Ok(glob) => {
                        builder.add(glob);
                    }
                    Err(e) => eprintln!("per-root 排除 glob 非法，已跳过 `{p}` (root={root}): {e}"),
                }
                // Codex SUGGEST 3：以 `/**` 结尾的 pattern 补目录本身 pattern
                if let Some(dir_pat) = p.strip_suffix("/**") {
                    if !dir_pat.is_empty() {
                        if let Ok(g) = Glob::new(dir_pat) {
                            builder.add(g);
                        }
                    }
                }
            }
            let gs = builder.build().unwrap_or_else(|_| GlobSet::empty());
            per_root.push((normalize(root), gs));
        }
        Self { basename, per_root }
    }

    /// 判定目录 entry 是否应被剪枝（basename 层 + per-root 相对路径层）。
    /// `normalize` 用于 entry 路径归一化（保 root_key 与 entry_key 分隔符 / 大小写一致）。
    pub fn is_excluded_dir<F>(&self, entry: &walkdir::DirEntry, normalize: F) -> bool
    where
        F: Fn(&str) -> String,
    {
        if !entry.file_type().is_dir() {
            return false;
        }
        // Layer 1：basename 全局
        if self.basename.is_match(entry.file_name()) {
            return true;
        }
        if self.per_root.is_empty() {
            return false;
        }
        // Layer 2：per-root path glob
        let entry_path_str = entry.path().to_string_lossy();
        let entry_key = normalize(&entry_path_str);
        for (root_key, gs) in &self.per_root {
            if let Some(rel) = entry_key.strip_prefix(root_key) {
                let rel = rel.trim_start_matches(['/', '\\']);
                if !rel.is_empty() {
                    // globset 建议路径分隔符统一 `/`（Windows 上 entry_key 已归一化过）
                    let rel_slash = rel.replace('\\', "/");
                    if gs.is_match(&rel_slash) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// 判定平台发现器返回的显式文件路径是否命中与 WalkDir 相同的排除策略。
    ///
    /// WalkDir 在进入目录时调用 [`Self::is_excluded_dir`] 剪枝；Everything/Spotlight
    /// 直接返回叶子文件，若只做 root 过滤会绕过排除目录。这里检查路径自身及祖先目录的
    /// basename，并把相对 root 路径交给同一份 per-root glob，保证两条发现路径语义一致。
    #[must_use]
    pub fn is_excluded_path<F>(&self, path: &Path, normalize: F) -> bool
    where
        F: Fn(&str) -> String,
    {
        if path
            .ancestors()
            .filter_map(Path::file_name)
            .any(|name| self.basename.is_match(name))
        {
            return true;
        }
        if self.per_root.is_empty() {
            return false;
        }
        let path_text = path.to_string_lossy();
        let path_key = normalize(&path_text);
        self.per_root.iter().any(|(root_key, patterns)| {
            path_key.strip_prefix(root_key).is_some_and(|rel| {
                let rel = rel.trim_start_matches(['/', '\\']);
                !rel.is_empty() && patterns.is_match(rel.replace('\\', "/"))
            })
        })
    }
}

/// 通用增量索引：递归遍历 `roots`，对扩展名命中 `exts` 的文件按 mtime 比对，
/// 变更则 `extract` + upsert，回收 `roots` 子树下磁盘已删的记录。
/// `exclude` 命中的目录名整棵子树短路剪枝（空集 = 无排除，行为与旧版逐字节一致）。
///
/// 默认 [`NoopProgress`]；需要边索引边汇报进度的调用方（如 daemon）走
/// [`run_incremental_index_with_progress`]。
pub(crate) fn run_incremental_index<S, F>(
    store: &S,
    roots: &[PathBuf],
    exts: &[&str],
    exclude: &GlobSet,
    extract: F,
) -> Result<IndexStats, IndexError>
where
    S: IncrementalStore,
    S::Entry: Send,
    F: Fn(&Path, i64) -> Result<S::Entry, IndexError> + Sync,
{
    run_incremental_index_with_progress(store, roots, exts, exclude, extract, &NoopProgress)
}

/// 同 [`run_incremental_index`]，但每文件 / 整批完成时回调 `progress` 汇报进度。
/// 文件回调 `mime` 退化为小写扩展名（提取器层不返回真实 MIME；daemon 仅做 tracing log）。
/// `indexed` 在 upsert 成功（added 或 updated）时为 `true`，skip / failed 时为 `false`。
pub(crate) fn run_incremental_index_with_progress<S, F, P>(
    store: &S,
    roots: &[PathBuf],
    exts: &[&str],
    exclude: &GlobSet,
    extract: F,
    progress: &P,
) -> Result<IndexStats, IndexError>
where
    S: IncrementalStore,
    S::Entry: Send,
    F: Fn(&Path, i64) -> Result<S::Entry, IndexError> + Sync,
    P: IndexProgress + ?Sized,
{
    // cycle 7-b（Codex OBJECT 2）：老 GlobSet API 走兼容层——包成 basename-only ExcludeFilter
    // 委托新 filter API；identity normalize（basename-only 不需要归一化）。BETA-27 basename-only
    // 行为逐字节保留。
    let filter = ExcludeFilter::from_basename_set(exclude);
    run_incremental_index_with_filter_and_progress(
        store,
        roots,
        exts,
        &filter,
        // no-op normalize（basename-only filter 不走 per_root 分支）
        <str as ToOwned>::to_owned,
        extract,
        progress,
    )
}

/// BETA-33 cycle 7-b：`run_incremental_index_with_progress` 的 filter 版本，
/// 支持 basename 全局 + per-root 相对路径两层排除。
/// `normalize_root` 是 root key 归一化 fn（desktop 传 [`crate::scan`] 之外的 `normalize_root_key`，
/// daemon 或 CLI 可传恒等 fn），保持 indexer 对 desktop 无依赖。
struct ExtractCandidate {
    path: PathBuf,
    path_str: String,
    ext: String,
    modified_secs: i64,
}

#[allow(clippy::too_many_lines)] // 承载整轮：串行预检 + 受限池并行提取 + 串行写库 + 删除回收
pub(crate) fn run_incremental_index_with_filter_and_progress<S, F, N, P>(
    store: &S,
    roots: &[PathBuf],
    exts: &[&str],
    filter: &ExcludeFilter,
    normalize_root: N,
    extract: F,
    progress: &P,
) -> Result<IndexStats, IndexError>
where
    S: IncrementalStore,
    S::Entry: Send,
    F: Fn(&Path, i64) -> Result<S::Entry, IndexError> + Sync,
    N: Fn(&str) -> String + Copy,
    P: IndexProgress + ?Sized,
{
    use rayon::prelude::*;

    // BETA-64 T1：阶段耗时埋点——遍历/提取/写库/回收四段各计时，轮次结束汇总打点，
    // 先测量再决定后续优化投入顺序（此前只有 scanned/added 等计数，无耗时数据）。
    let run_start = Instant::now();
    let mut stats = IndexStats::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut to_extract: Vec<ExtractCandidate> = Vec::new();

    // BETA-64 T7a：一次性批量预取本轮 roots 下已索引的 (path, modified_time)，替代逐文件
    // `modified_time_of` 查询；同一份 map 结束后回收阶段复用（见下），故本轮总计 DB 查询数
    // 从「N 个文件 + 1 次全表扫描」降到「1 次全表扫描」。
    let root_strs: Vec<String> = roots
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let known_mtimes = store.modified_times_under(&root_strs)?;
    // 2026-07-29：同一批 `all_failure_modified_times()` 结果本轮全程复用（同 `known_mtimes`
    // 的一次性预取模式）——`index_failures` 表通常很小（异常文件，非常态），不值得像
    // `documents` 表那样搞跨函数缓存，但至少这一轮内不用每个文件都查一次。
    let known_failure_mtimes = store.all_failure_modified_times()?;

    for root in roots {
        for entry in WalkDir::new(root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !filter.is_excluded_dir(e, normalize_root))
            .filter_map(Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if is_office_lock_file(path) {
                continue;
            }
            if !has_ext(path, exts) {
                continue;
            }
            stats.scanned += 1;
            let path_str = path.to_string_lossy().into_owned();
            seen.insert(path_str.clone());

            let ext = ext_lower(path);
            let Some(modified_secs) = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .and_then(|d| i64::try_from(d.as_secs()).ok())
            else {
                stats.failed += 1;
                store.record_extract_failure(&path_str, "读取文件 mtime 失败", 0)?;
                progress.on_file(path, &ext, false);
                continue;
            };

            let already_failed_unchanged = known_failure_mtimes
                .get(&path_str)
                .is_some_and(|&old| old == modified_secs);

            match known_mtimes.get(&path_str) {
                Some(&old) if old == modified_secs => {
                    stats.skipped += 1;
                    progress.on_file(path, &ext, false);
                }
                _ if already_failed_unchanged => {
                    // 上次提取失败（加密文档/权限过期等）、文件自那以后 mtime 没变——
                    // 大概率再试一次还是同样失败，不重复烧提取耗时；等文件真的被改动
                    // （用户解除加密/权限恢复后重新保存）才会再进 to_extract 重试。
                    stats.skipped += 1;
                    progress.on_file(path, &ext, false);
                }
                _ => to_extract.push(ExtractCandidate {
                    path: path.to_path_buf(),
                    path_str,
                    ext,
                    modified_secs,
                }),
            }
        }
    }
    let walk_elapsed = run_start.elapsed();
    // 2026-07-28：此时 `stats.scanned` 就是本阶段最终会触发 `on_file` 的文件总数——
    // skip 分支已经在上面的循环里调过 `on_file`，to_extract 分支会在下面的并行提取段
    // 调用。桌面 UI 用这个总数把"已扫描 N"升级成"N / total"真百分比 + ETA。
    progress.on_scope_known(stats.scanned as u64);

    // 提取器（lofty / pdf-extract / OCR 等）可能很重；并行段只做文件读取/解析，不碰 DB。
    // 按块处理，避免十万级文档把正文提取结果一次性堆到内存里；块内仍使用 rayon 并行。
    // 受限提取线程池：把重量级提取（尤其扫描 PDF 的 pdftoppm 渲染 + 逐页 OCR）的并发压到
    // 预算内，避免子进程风暴打爆机器；音乐轮（无子进程）按核数放开——见
    // [`extract_parallelism_for`]（BETA-64 T3）。构建失败（极罕见）→ None、退回 rayon 全局池
    // （旧行为）。
    let n_threads = extract_parallelism_for(exts);
    let extract_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(n_threads)
        .stack_size(EXTRACT_STACK_SIZE)
        .build()
        .ok();

    // 单块提取：每个文件一提取完就**立即** on_file 报进度。`IndexProgress` 是 Send+Sync、
    // 专为跨线程调用设计，故可在 rayon 工作线程里调。这样 UI 计数器随每个文件前进，
    // 不被整块 rayon barrier 冻住——即便块内有个慢文件（大 PDF / 图片 OCR），其余文件
    // 仍各自报进度（修 BETA-60 首版把 on_file 放块尾串行段、一块内进度全冻、用户误判卡死）。
    let extract_chunk = |chunk: &[ExtractCandidate]| -> Vec<Result<S::Entry, IndexError>> {
        chunk
            .par_iter()
            .map(|candidate| {
                let result =
                    catch_extract(|| extract(candidate.path.as_path(), candidate.modified_secs));
                // indexed 语义：提取成功 → 后续必 upsert（true）；提取失败 → 计 failed（false）。
                progress.on_file(&candidate.path, &candidate.ext, result.is_ok());
                result
            })
            .collect()
    };

    let mut extract_elapsed = Duration::ZERO;
    let mut write_elapsed = Duration::ZERO;

    for chunk in to_extract.chunks(EXTRACT_CHUNK) {
        // 在受限池内跑本块提取（池不可用则退回全局池）。
        let chunk_extract_start = Instant::now();
        let extracted = match &extract_pool {
            Some(pool) => pool.install(|| extract_chunk(chunk)),
            None => extract_chunk(chunk),
        };
        extract_elapsed += chunk_extract_start.elapsed();

        // 串行写库（DB 单 writer）：进度已在并行段报过、此处不再 on_file。
        // BETA-64 T2：提取成功的条目攒进本块一批，一次事务批量 upsert（`upsert_entries`），
        // 而非逐条各开各提的事务——此前每文件一次 commit，一万文件即一万次独立事务，
        // 即便 WAL+NORMAL 已避免最坏情况，仍比按块攒批多出可观的固定开销。提取失败的
        // 条目不入批量事务，单独记留痕（其 stats/留痕语义与改造前逐字节等价）。
        let chunk_write_start = Instant::now();
        let mut ok_candidates: Vec<&ExtractCandidate> = Vec::with_capacity(chunk.len());
        let mut ok_entries: Vec<S::Entry> = Vec::with_capacity(chunk.len());
        for (candidate, result) in chunk.iter().zip(extracted) {
            match result {
                Ok(e) => {
                    ok_candidates.push(candidate);
                    ok_entries.push(e);
                }
                Err(err) => {
                    stats.failed += 1;
                    // 留痕只取失败细节；path 已由表主键记录，避免重复。
                    let reason = match &err {
                        IndexError::Tag { detail, .. } => detail.clone(),
                        other => other.to_string(),
                    };
                    store.record_extract_failure(
                        &candidate.path_str,
                        &reason,
                        candidate.modified_secs,
                    )?;
                }
            }
        }
        if !ok_entries.is_empty() {
            let added_flags = store.upsert_entries(&ok_entries)?;
            for (candidate, added) in ok_candidates.iter().zip(added_flags) {
                if added {
                    stats.added += 1;
                } else {
                    stats.updated += 1;
                }
                // 之前失败过、这轮成功 => 清留痕。
                store.clear_extract_failure(&candidate.path_str)?;
            }
        }
        write_elapsed += chunk_write_start.elapsed();
    }

    // 回收：DB 中落在 roots 子树下、本轮未见到、且**扩展名属于本轮白名单**的记录 = 磁盘已删。
    // 扩展名收窄是关键：图片与文档可能同根目录，文档轮不应回收图片（反之亦然）。
    // BETA-64 T7a：直接复用上方预取的 `known_mtimes` key 集合，不再对 `paths_under` second
    // 全表查询——两者数据源等价（都是"roots 子树下索引中的全部 path"），拆两次查询纯属浪费。
    let recycle_start = Instant::now();
    for p in known_mtimes.keys() {
        if !seen.contains(p) && has_ext(Path::new(p), exts) && store.delete_by_path(p)? {
            stats.removed += 1;
        }
    }
    // 失败留痕同款回收：文件已从磁盘消失（本轮未见）→ 留痕清除，不留幽灵记录。
    for p in store.failure_paths_under(&root_strs)? {
        if !seen.contains(&p) && has_ext(Path::new(&p), exts) {
            store.clear_extract_failure(&p)?;
        }
    }
    let recycle_elapsed = recycle_start.elapsed();

    let indexed = stats.added + stats.updated;
    progress.on_batch_done(stats.scanned as u64, indexed as u64);
    progress.on_stage_timings(StageTimings {
        walk_ms: millis(walk_elapsed),
        extract_ms: millis(extract_elapsed),
        write_ms: millis(write_elapsed),
        recycle_ms: millis(recycle_elapsed),
    });

    tracing::info!(
        first_ext = exts.first().copied().unwrap_or(""),
        scanned = stats.scanned,
        added = stats.added,
        updated = stats.updated,
        skipped = stats.skipped,
        failed = stats.failed,
        removed = stats.removed,
        extract_parallelism = n_threads,
        walk_ms = millis(walk_elapsed),
        extract_ms = millis(extract_elapsed),
        write_ms = millis(write_elapsed),
        recycle_ms = millis(recycle_elapsed),
        total_ms = millis(run_start.elapsed()),
        "增量索引轮次完成"
    );

    Ok(stats)
}

/// 文件扩展名小写（不带点）。无扩展名返回空串。供进度回调的 `mime` 字段使用。
fn ext_lower(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default()
}

/// 音乐扩展名白名单（**索引侧**：决定 reindex 扫描哪些音频文件入库，大小写不敏感）。
///
/// 注意：与搜索侧的 `scout_search_backend::extensions_for_file_type(FileType::Audio)`
/// **故意不同**——索引侧含 `opus`/`aif` 等"能提取就入库"的格式；搜索侧是 file_type 查询映射。
/// 两者语义不同、不可合并（硬合并会改变索引范围或搜索匹配，详见 CONVENTIONS 收工记录 2026-06-03）。
const MUSIC_EXTS: &[&str] = &[
    "mp3", "flac", "m4a", "aac", "ogg", "opus", "wav", "wma", "aiff", "aif", "ape",
];

/// 默认音乐根目录（系统 Music 目录，经 [`dirs::audio_dir`]）。无法确定时返回空。
#[must_use]
pub fn default_music_roots() -> Vec<PathBuf> {
    dirs::audio_dir().into_iter().collect()
}

impl MusicIndex {
    /// 增量索引给定根目录（递归）。跳过 mtime 未变的文件；
    /// 回收 `roots` 范围内已不存在于磁盘的记录。
    pub fn index_dirs(&self, roots: &[PathBuf]) -> Result<IndexStats, IndexError> {
        self.index_dirs_excluding(roots, &GlobSet::empty())
    }

    /// 同 [`MusicIndex::index_dirs`]，但 `exclude` 命中的目录名整棵子树剪枝。
    pub fn index_dirs_excluding(
        &self,
        roots: &[PathBuf],
        exclude: &GlobSet,
    ) -> Result<IndexStats, IndexError> {
        run_incremental_index(
            self,
            roots,
            MUSIC_EXTS,
            exclude,
            crate::extract::extract_metadata,
        )
    }

    /// 与 [`MusicIndex::index_dirs`] 等价，但接受 [`IndexProgress`] 回调每文件 / 每批进度。
    /// 桌面 app 维持调旧 `index_dirs`（行为不变）；headless daemon 调本函数 + tracing log impl。
    pub fn index_dirs_with_progress<P: IndexProgress + ?Sized>(
        &self,
        roots: &[PathBuf],
        progress: &P,
    ) -> Result<IndexStats, IndexError> {
        self.index_dirs_excluding_with_progress(roots, &GlobSet::empty(), progress)
    }

    /// 同 [`MusicIndex::index_dirs_excluding`]，外加 [`IndexProgress`] 回调。
    pub fn index_dirs_excluding_with_progress<P: IndexProgress + ?Sized>(
        &self,
        roots: &[PathBuf],
        exclude: &GlobSet,
        progress: &P,
    ) -> Result<IndexStats, IndexError> {
        run_incremental_index_with_progress(
            self,
            roots,
            MUSIC_EXTS,
            exclude,
            crate::extract::extract_metadata,
            progress,
        )
    }

    /// BETA-33 cycle 7-b：filter 版本（basename + per-root path glob 双层排除）。
    /// 桌面 `perform_reindex` 通过 [`ExcludeFilter::build`] 构造 filter 再调本方法。
    /// 兼容层的旧 API `index_dirs_excluding_with_progress(&GlobSet)` 保留不变。
    pub fn index_dirs_with_filter_and_progress<N, P>(
        &self,
        roots: &[PathBuf],
        filter: &ExcludeFilter,
        normalize_root: N,
        progress: &P,
    ) -> Result<IndexStats, IndexError>
    where
        N: Fn(&str) -> String + Copy,
        P: IndexProgress + ?Sized,
    {
        run_incremental_index_with_filter_and_progress(
            self,
            roots,
            MUSIC_EXTS,
            filter,
            normalize_root,
            crate::extract::extract_metadata,
            progress,
        )
    }

    /// 索引一个**显式路径列表**（不递归、不回收）。用于「发现层」由外部提供
    /// 全盘音频路径（如 Everything / Spotlight 枚举）的场景（BETA-01A）。
    ///
    /// 分三阶段（rusqlite `Connection: !Sync`）：① 顺序预检（扩展名 / fs mtime / `modified_time_of`
    /// / 占位符属性，全是 metadata 读不触发下载）→ 分类 skip / 占位符 / 待提取；② **rayon 并行**
    /// lofty 提取（无 DB）；③ 顺序 upsert（DB 写）。**仅在线占位符不读标签、只存文件名**
    /// （避失败 + 避触发 OneDrive 下载，仍按文件名可搜）。
    pub fn index_paths(
        &self,
        paths: &[PathBuf],
        progress: &dyn IndexProgress,
    ) -> Result<IndexStats, IndexError> {
        use rayon::prelude::*;

        let run_start = Instant::now();
        let mut stats = IndexStats::default();
        let mut placeholders: Vec<MusicEntry> = Vec::new();
        let mut to_extract: Vec<(PathBuf, i64)> = Vec::new();

        // ① 顺序预检（DB 读）。BETA-64 T7：一次性批量预取全表 mtime，替代逐路径
        // `modified_time_of` 查询——发现层是全盘规模调用，这里省下的往返次数最直接。
        let known_mtimes = self.all_modified_times()?;
        for path in paths {
            if !has_ext(path, MUSIC_EXTS) {
                continue;
            }
            stats.scanned += 1;
            let path_str = path.to_string_lossy().into_owned();
            let ext = ext_lower(path);
            let Some(mtime) = fs_mtime(path) else {
                stats.failed += 1;
                progress.on_file(path, &ext, false);
                continue;
            };
            match known_mtimes.get(&path_str) {
                Some(&old) if old == mtime => {
                    stats.skipped += 1;
                    progress.on_file(path, &ext, false);
                }
                _ if crate::placeholder::is_online_only(path) => {
                    // 仅在线占位符不触发下载、只存文件名——分类阶段即可判定"会被入库"，
                    // 不像提取候选那样要等 ② 并行提取完才知道成功与否。
                    progress.on_file(path, &ext, true);
                    placeholders.push(filename_only_entry(path, mtime));
                }
                _ => to_extract.push((path.clone(), mtime)),
            }
        }
        let walk_elapsed = run_start.elapsed();
        // 2026-07-28：此时 stats.scanned 已是本阶段最终 on_file 触发总数（同款语义见
        // `run_incremental_index_with_filter_and_progress`/`index_discovered_paths`）。
        progress.on_scope_known(stats.scanned as u64);

        // ② rayon 并行提取（无 DB 访问）。catch_extract 兜住提取器 panic（畸形文件）。
        // 必须走受限栈线程池（同 run_incremental_index_with_filter_and_progress）：lofty 解析
        // 畸形/深嵌套音频标签（MP4 box 嵌套、ID3v2 链式帧等）递归可能很深，rayon **全局**池的
        // worker 栈是系统默认（Windows ≈2 MiB），会被撑爆 → STATUS_STACK_OVERFLOW（0xC00000FD，
        // SEH 异常、catch_unwind 兜不住，直接 abort）——与 pdf-extract 那次真机崩溃同款故障，
        // 只是入口换成了 BETA-01A 全盘发现层（Everything/Windows Search 枚举后调本方法）。
        let n_threads = EXTRACT_PARALLELISM.min(
            std::thread::available_parallelism()
                .map_or(EXTRACT_PARALLELISM, std::num::NonZeroUsize::get),
        );
        let extract_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(n_threads)
            .stack_size(EXTRACT_STACK_SIZE)
            .build()
            .ok();
        let do_extract = || -> Vec<Result<MusicEntry, IndexError>> {
            to_extract
                .par_iter()
                .map(|(path, mtime)| {
                    let ext = ext_lower(path);
                    let result = catch_extract(|| crate::extract::extract_metadata(path, *mtime));
                    progress.on_file(path, &ext, result.is_ok());
                    result
                })
                .collect()
        };
        let extract_start = Instant::now();
        let extracted: Vec<Result<MusicEntry, IndexError>> = match &extract_pool {
            Some(pool) => pool.install(do_extract),
            None => do_extract(),
        };
        let extract_elapsed = extract_start.elapsed();

        // ③ 顺序 upsert（DB 写）。占位符（仅文件名）+ 提取成功项入库；提取失败计 failed。
        let write_start = Instant::now();
        for entry in placeholders {
            if self.upsert_entry(&entry)? {
                stats.added += 1;
            } else {
                stats.updated += 1;
            }
        }
        for result in extracted {
            match result {
                Ok(entry) => {
                    if self.upsert_entry(&entry)? {
                        stats.added += 1;
                    } else {
                        stats.updated += 1;
                    }
                }
                Err(_) => stats.failed += 1,
            }
        }
        let write_elapsed = write_start.elapsed();

        // recycle_ms 恒为 0：本方法不做回收，回收由调用方另外调 `prune_deleted()`。
        progress.on_stage_timings(StageTimings {
            walk_ms: millis(walk_elapsed),
            extract_ms: millis(extract_elapsed),
            write_ms: millis(write_elapsed),
            recycle_ms: 0,
        });

        Ok(stats)
    }
}

thread_local! {
    /// 当前线程是否正处于 [`catch_extract`] 包裹内。提取器（pdf-extract 等）对畸形文件
    /// 的 panic 已被 `catch_unwind` 兜底并计入 failed，无需默认 hook 再打印到 stderr 刷屏。
    static IN_CATCH_EXTRACT: Cell<bool> = const { Cell::new(false) };
}

/// 安装一次性 panic hook：仅当 panic 发生在 [`catch_extract`] 内（即提取器对畸形文件炸了、
/// 已被兜底）时静默，其余 panic 仍走默认 hook 正常打印。进程级一次安装（`Once`），
/// 通过线程局部标志判定是否抑制——抑制范围精确到「正在被 catch 的提取调用」，不影响其他 panic。
fn install_quiet_extract_panic_hook() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !IN_CATCH_EXTRACT.with(Cell::get) {
                default_hook(info);
            }
        }));
    });
}

/// 调提取器并兜住 panic。提取器（lofty / pdf-extract / calamine 等）对畸形文件可能 **panic**
/// 而非返 Err；catch_unwind 兜成 Err（计入 failed），不让单个坏文件中断整轮 reindex。
/// 注：release 须用 `panic = "unwind"`（非 abort），否则 abort 下 catch_unwind 无效、进程仍崩。
///
/// 兜住的同时静默默认 panic hook 的 stderr 打印（畸形 PDF 启动一次刷十几条噪声）：
/// 标志置位 → 提取若 panic，hook 在**同一线程**（顺序路径或 rayon worker 均成立）读到置位而跳过打印。
/// `catch_unwind` 必返回，标志随后无条件复位，故对正常返回 / panic 两路径均不泄漏置位状态。
fn catch_extract<T>(extract: impl FnOnce() -> Result<T, IndexError>) -> Result<T, IndexError> {
    install_quiet_extract_panic_hook();
    IN_CATCH_EXTRACT.with(|flag| flag.set(true));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(extract));
    IN_CATCH_EXTRACT.with(|flag| flag.set(false));
    match outcome {
        Ok(result) => result,
        Err(_) => Err(IndexError::Tag {
            path: String::new(),
            detail: "提取器 panic（畸形文件）".to_owned(),
        }),
    }
}

/// 取文件 mtime（unix 秒）；失败返回 None。
fn fs_mtime(path: &Path) -> Option<i64> {
    path.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_secs()).ok())
}

/// 占位符的仅文件名记录（不读标签，避触发云下载）。
fn filename_only_entry(path: &Path, modified_time: i64) -> MusicEntry {
    MusicEntry {
        path: path.to_string_lossy().into_owned(),
        file_name: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string(),
        modified_time,
        ..Default::default()
    }
}

/// 文档扩展名白名单（**索引侧**：决定 reindex 对哪些文件做内容提取入库，大小写不敏感）。
///
/// 注意：这是"内容提取白名单"，**跨** 搜索侧的 Document / Spreadsheet / Presentation 三个 `FileType`，
/// 与 `scout_search_backend::extensions_for_file_type` 的按类型映射**故意不同**、不可合并。
/// BETA-64 T5：`pub(crate)`（而非私有）以便 `discovery.rs` 的文档发现层复用同一份
/// 扩展名白名单，避免第二份手动同步的重复列表漂移。
pub(crate) const DOC_EXTS: &[&str] = &[
    "docx", "xlsx", "pptx", "pdf", "txt", "md", "markdown", "html", "htm", "xls", "ods",
    // BETA-37：邮件（msg 后置 BETA-37b，pst 明确不做）。
    "eml",
    // BETA-40 评测暴露：企业归档常见纯文本表格（权限清单 / 台账导出），按纯文本提取。
    "csv", "tsv",
];

/// 默认文档根目录（系统 Documents 目录，经 [`dirs::document_dir`]）。无法确定时返回空。
#[must_use]
pub fn default_document_roots() -> Vec<PathBuf> {
    dirs::document_dir().into_iter().collect()
}

impl crate::doc_db::DocumentIndex {
    /// 增量索引给定根目录（递归）。跳过 mtime 未变的文件；
    /// 回收 `roots` 范围内已不存在于磁盘的记录。
    pub fn index_dirs(&self, roots: &[PathBuf]) -> Result<IndexStats, IndexError> {
        self.index_dirs_excluding(roots, &GlobSet::empty())
    }

    /// 同 [`DocumentIndex::index_dirs`]，但 `exclude` 命中的目录名整棵子树剪枝。
    pub fn index_dirs_excluding(
        &self,
        roots: &[PathBuf],
        exclude: &GlobSet,
    ) -> Result<IndexStats, IndexError> {
        run_incremental_index(
            self,
            roots,
            DOC_EXTS,
            exclude,
            crate::doc_extract::extract_document,
        )
    }

    /// 与 [`DocumentIndex::index_dirs`] 等价，但接受 [`IndexProgress`] 回调每文件 / 每批进度。
    /// 桌面 app 维持调旧 `index_dirs`（行为不变）；headless daemon 调本函数 + tracing log impl。
    pub fn index_dirs_with_progress<P: IndexProgress + ?Sized>(
        &self,
        roots: &[PathBuf],
        progress: &P,
    ) -> Result<IndexStats, IndexError> {
        self.index_dirs_excluding_with_progress(roots, &GlobSet::empty(), progress)
    }

    /// 同 [`DocumentIndex::index_dirs_excluding`]，外加 [`IndexProgress`] 回调。
    pub fn index_dirs_excluding_with_progress<P: IndexProgress + ?Sized>(
        &self,
        roots: &[PathBuf],
        exclude: &GlobSet,
        progress: &P,
    ) -> Result<IndexStats, IndexError> {
        run_incremental_index_with_progress(
            self,
            roots,
            DOC_EXTS,
            exclude,
            crate::doc_extract::extract_document,
            progress,
        )
    }

    /// BETA-33 cycle 7-b：filter 版本（basename + per-root path glob 双层排除）。
    pub fn index_dirs_with_filter_and_progress<N, P>(
        &self,
        roots: &[PathBuf],
        filter: &ExcludeFilter,
        normalize_root: N,
        progress: &P,
    ) -> Result<IndexStats, IndexError>
    where
        N: Fn(&str) -> String + Copy,
        P: IndexProgress + ?Sized,
    {
        run_incremental_index_with_filter_and_progress(
            self,
            roots,
            DOC_EXTS,
            filter,
            normalize_root,
            crate::doc_extract::extract_document,
            progress,
        )
    }

    /// 增量 **OCR** 索引图片目录（递归 + mtime skip + 回收）。每图经 `ocr` 识别文字进 FTS。
    /// 与文档共用 `documents` 表（图片 doc_type = 扩展名）。OCR 失败计 failed、不中断整轮。
    pub fn index_image_dirs(
        &self,
        roots: &[PathBuf],
        ocr: &dyn crate::ocr::OcrEngine,
    ) -> Result<IndexStats, IndexError> {
        self.index_image_dirs_excluding(roots, ocr, &GlobSet::empty())
    }

    /// 同 [`DocumentIndex::index_image_dirs`]，但 `exclude` 命中的目录名整棵子树剪枝。
    pub fn index_image_dirs_excluding(
        &self,
        roots: &[PathBuf],
        ocr: &dyn crate::ocr::OcrEngine,
        exclude: &GlobSet,
    ) -> Result<IndexStats, IndexError> {
        // recognize 已返回归一化文字（trait 契约）；body 即 OCR 文字。
        run_incremental_index(self, roots, IMAGE_EXTS, exclude, |path, mtime| {
            Ok(ExtractedDoc {
                entry: image_entry(path, mtime),
                body: ocr.recognize(path)?,
                passages: Vec::new(),
                failed_pages: Vec::new(),
            })
        })
    }

    /// 与 [`DocumentIndex::index_image_dirs`] 等价，外加 [`IndexProgress`] 回调。
    pub fn index_image_dirs_with_progress<P: IndexProgress + ?Sized>(
        &self,
        roots: &[PathBuf],
        ocr: &dyn crate::ocr::OcrEngine,
        progress: &P,
    ) -> Result<IndexStats, IndexError> {
        self.index_image_dirs_excluding_with_progress(roots, ocr, &GlobSet::empty(), progress)
    }

    /// 同 [`DocumentIndex::index_image_dirs_excluding`]，外加 [`IndexProgress`] 回调。
    pub fn index_image_dirs_excluding_with_progress<P: IndexProgress + ?Sized>(
        &self,
        roots: &[PathBuf],
        ocr: &dyn crate::ocr::OcrEngine,
        exclude: &GlobSet,
        progress: &P,
    ) -> Result<IndexStats, IndexError> {
        run_incremental_index_with_progress(
            self,
            roots,
            IMAGE_EXTS,
            exclude,
            |path, mtime| {
                Ok(ExtractedDoc {
                    entry: image_entry(path, mtime),
                    body: ocr.recognize(path)?,
                    passages: Vec::new(),
                    failed_pages: Vec::new(),
                })
            },
            progress,
        )
    }

    /// BETA-33 cycle 7-b：filter 版本（basename + per-root path glob 双层排除）。
    pub fn index_image_dirs_with_filter_and_progress<N, P>(
        &self,
        roots: &[PathBuf],
        ocr: &dyn crate::ocr::OcrEngine,
        filter: &ExcludeFilter,
        normalize_root: N,
        progress: &P,
    ) -> Result<IndexStats, IndexError>
    where
        N: Fn(&str) -> String + Copy,
        P: IndexProgress + ?Sized,
    {
        run_incremental_index_with_filter_and_progress(
            self,
            roots,
            IMAGE_EXTS,
            filter,
            normalize_root,
            |path, mtime| {
                Ok(ExtractedDoc {
                    entry: image_entry(path, mtime),
                    body: ocr.recognize(path)?,
                    passages: Vec::new(),
                    failed_pages: Vec::new(),
                })
            },
            progress,
        )
    }

    /// 索引一个**显式路径列表**（不递归、不回收）。用于「发现层」由外部提供全盘文档路径
    /// （如 Everything / Spotlight 枚举）的场景（BETA-64 T5，比照 BETA-01A
    /// [`crate::MusicIndex::index_paths`]）。无 per-file 进度（发现层本身秒级完成，
    /// 与音乐发现分支同款约定）。
    ///
    /// 单独调用本方法会内部现取一份 `all_modified_times()`；若调用方在同一轮里还会
    /// 紧接着调 [`Self::index_image_paths`]（如 `packages/search-backends/local-index`
    /// 的三条生产 reindex 入口），两次调用各自扫一遍 `documents` 全表——改用
    /// [`Self::index_paths_with_known_mtimes`] 自己只取一次、两处复用可以省掉这次
    /// 重复扫描。
    pub fn index_paths(
        &self,
        paths: &[PathBuf],
        progress: &dyn IndexProgress,
    ) -> Result<IndexStats, IndexError> {
        let known_mtimes = self.all_modified_times()?;
        self.index_paths_with_known_mtimes(paths, &known_mtimes, progress)
    }

    /// 同 [`Self::index_paths`]，但 mtime 表由调用方预取传入（不再内部现取），供需要
    /// 与 [`Self::index_image_paths_with_known_mtimes`] 共享同一份 `all_modified_times()`
    /// 结果的调用方使用（BETA-64 T7 收尾：消掉同一轮 reindex 内的重复全表扫描）。
    pub fn index_paths_with_known_mtimes(
        &self,
        paths: &[PathBuf],
        known_mtimes: &HashMap<String, i64>,
        progress: &dyn IndexProgress,
    ) -> Result<IndexStats, IndexError> {
        index_discovered_paths(
            self,
            paths,
            DOC_EXTS,
            known_mtimes,
            crate::doc_extract::extract_document,
            progress,
        )
    }

    /// 图片 OCR 版本的 [`DocumentIndex::index_paths`]（BETA-64 T5，比照
    /// [`DocumentIndex::index_image_dirs`]）。同样不递归、不回收；内部现取
    /// `all_modified_times()`——若与 [`Self::index_paths`] 同轮调用，改用
    /// [`Self::index_image_paths_with_known_mtimes`] 复用同一份预取结果。
    pub fn index_image_paths(
        &self,
        paths: &[PathBuf],
        ocr: &dyn crate::ocr::OcrEngine,
        progress: &dyn IndexProgress,
    ) -> Result<IndexStats, IndexError> {
        let known_mtimes = self.all_modified_times()?;
        self.index_image_paths_with_known_mtimes(paths, ocr, &known_mtimes, progress)
    }

    /// 同 [`Self::index_image_paths`]，但 mtime 表由调用方预取传入（不再内部现取）。
    pub fn index_image_paths_with_known_mtimes(
        &self,
        paths: &[PathBuf],
        ocr: &dyn crate::ocr::OcrEngine,
        known_mtimes: &HashMap<String, i64>,
        progress: &dyn IndexProgress,
    ) -> Result<IndexStats, IndexError> {
        index_discovered_paths(
            self,
            paths,
            IMAGE_EXTS,
            known_mtimes,
            |path, mtime| {
                Ok(ExtractedDoc {
                    entry: image_entry(path, mtime),
                    body: ocr.recognize(path)?,
                    passages: Vec::new(),
                    failed_pages: Vec::new(),
                })
            },
            progress,
        )
    }
}

/// [`DocumentIndex::index_paths`] / [`DocumentIndex::index_image_paths`]（BETA-64 T5）
/// 共用骨架：给定一批已知路径（不递归 `WalkDir`、不回收磁盘已删记录——发现层只做加速，
/// 真正的删除回收仍由定期的目录扫描轮兜底），三段式处理：① 顺序预检（mtime 比对，
/// metadata 读不触发 OneDrive 占位符下载）；② 受限池并行提取（[`extract_parallelism_for`]，
/// 与 walkdir 路径同一并发预算，音乐/文档/图片三轮语义一致）；③ 按块批量 upsert
/// （[`DocumentIndex::upsert_documents_with_pages`]，T2）。耗时埋点同 T1。
///
/// `known_mtimes`（BETA-64 T7）由调用方传入而非本函数内部现取——`index_paths`/
/// `index_image_paths` 各自单独调用时各取各的（行为不变），`_with_known_mtimes`
/// 变体让同轮调用两次的调用方只取一次、两处复用。
///
/// 2026-07-28：新增 `progress` 参数——此前发现层这条路径完全不报逐文件进度
/// （`IndexStatus.fts_progress` 在 Everything/Spotlight 发现成功时纹丝不动，只有
/// 回退到 `WalkDir` 才会动），T5 落地后发现层已是桌面端主路径，这个缺口比"没有
/// 真百分比"更基础——真机装了 Everything/Spotlight 的常见场景下几乎看不到进度。
/// 照抄 `run_incremental_index_with_filter_and_progress` 的写法补齐。
#[allow(clippy::too_many_lines)] // 承载整轮：串行预检 + 受限池并行提取 + 串行写库，同 line 415 理由
fn index_discovered_paths<F>(
    store: &crate::doc_db::DocumentIndex,
    paths: &[PathBuf],
    exts: &[&str],
    known_mtimes: &HashMap<String, i64>,
    extract: F,
    progress: &dyn IndexProgress,
) -> Result<IndexStats, IndexError>
where
    F: Fn(&Path, i64) -> Result<ExtractedDoc, IndexError> + Sync,
{
    use rayon::prelude::*;

    let run_start = Instant::now();
    let mut stats = IndexStats::default();
    let mut to_extract: Vec<ExtractCandidate> = Vec::new();
    // 2026-07-29：同 `run_incremental_index_with_filter_and_progress`——一次性预取，
    // 避免上次已知失败且文件未变的候选重新走一遍必然再失败的提取。
    let known_failure_mtimes = store.all_failure_modified_times()?;

    for path in paths {
        if !has_ext(path, exts) {
            continue;
        }
        stats.scanned += 1;
        let path_str = path.to_string_lossy().into_owned();
        let ext = ext_lower(path);
        let Some(modified_secs) = fs_mtime(path) else {
            stats.failed += 1;
            store.record_extract_failure(&path_str, "读取文件 mtime 失败", 0)?;
            progress.on_file(path, &ext, false);
            continue;
        };
        let already_failed_unchanged = known_failure_mtimes
            .get(&path_str)
            .is_some_and(|&old| old == modified_secs);
        match known_mtimes.get(&path_str) {
            Some(&old) if old == modified_secs => {
                stats.skipped += 1;
                progress.on_file(path, &ext, false);
            }
            _ if already_failed_unchanged => {
                stats.skipped += 1;
                progress.on_file(path, &ext, false);
            }
            _ => to_extract.push(ExtractCandidate {
                path: path.clone(),
                path_str,
                ext,
                modified_secs,
            }),
        }
    }
    let walk_elapsed = run_start.elapsed();
    // 此时 stats.scanned 已是本阶段最终 on_file 触发总数（skip 分支已报，to_extract
    // 分支会在下面并行段报），同款语义见 `run_incremental_index_with_filter_and_progress`。
    progress.on_scope_known(stats.scanned as u64);

    let n_threads = extract_parallelism_for(exts);
    let extract_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(n_threads)
        .stack_size(EXTRACT_STACK_SIZE)
        .build()
        .ok();
    let extract_chunk = |chunk: &[ExtractCandidate]| -> Vec<Result<ExtractedDoc, IndexError>> {
        chunk
            .par_iter()
            .map(|candidate| {
                let result =
                    catch_extract(|| extract(candidate.path.as_path(), candidate.modified_secs));
                progress.on_file(&candidate.path, &candidate.ext, result.is_ok());
                result
            })
            .collect()
    };

    let mut extract_elapsed = Duration::ZERO;
    let mut write_elapsed = Duration::ZERO;

    for chunk in to_extract.chunks(EXTRACT_CHUNK) {
        let chunk_extract_start = Instant::now();
        let extracted = match &extract_pool {
            Some(pool) => pool.install(|| extract_chunk(chunk)),
            None => extract_chunk(chunk),
        };
        extract_elapsed += chunk_extract_start.elapsed();

        let chunk_write_start = Instant::now();
        let mut ok_candidates: Vec<&ExtractCandidate> = Vec::with_capacity(chunk.len());
        let mut ok_entries: Vec<ExtractedDoc> = Vec::with_capacity(chunk.len());
        for (candidate, result) in chunk.iter().zip(extracted) {
            match result {
                Ok(e) => {
                    ok_candidates.push(candidate);
                    ok_entries.push(e);
                }
                Err(err) => {
                    stats.failed += 1;
                    let reason = match &err {
                        IndexError::Tag { detail, .. } => detail.clone(),
                        other => other.to_string(),
                    };
                    store.record_extract_failure(
                        &candidate.path_str,
                        &reason,
                        candidate.modified_secs,
                    )?;
                }
            }
        }
        if !ok_entries.is_empty() {
            let added_flags = store.upsert_documents_with_pages(&ok_entries)?;
            for (candidate, added) in ok_candidates.iter().zip(added_flags) {
                if added {
                    stats.added += 1;
                } else {
                    stats.updated += 1;
                }
                store.clear_extract_failure(&candidate.path_str)?;
            }
        }
        write_elapsed += chunk_write_start.elapsed();
    }

    // recycle_ms 恒为 0：本函数不做回收，回收由调用方另外调 `prune_deleted()`。
    progress.on_stage_timings(StageTimings {
        walk_ms: millis(walk_elapsed),
        extract_ms: millis(extract_elapsed),
        write_ms: millis(write_elapsed),
        recycle_ms: 0,
    });

    tracing::info!(
        first_ext = exts.first().copied().unwrap_or(""),
        scanned = stats.scanned,
        added = stats.added,
        updated = stats.updated,
        skipped = stats.skipped,
        failed = stats.failed,
        extract_parallelism = n_threads,
        walk_ms = millis(walk_elapsed),
        extract_ms = millis(extract_elapsed),
        write_ms = millis(write_elapsed),
        total_ms = millis(run_start.elapsed()),
        "发现层索引轮次完成"
    );

    Ok(stats)
}

/// 图片扩展名白名单（**索引侧**：决定 reindex 对哪些图片做 OCR 入库，大小写不敏感）。
///
/// 与搜索侧 common `extensions_for_file_type(Image)` **故意不同**（不含 `svg`/`heif`——OCR 不适用矢量图）。
/// `LocalIndexBackend` 复用本集合限定图片 OCR 记录查询范围（单一信源，避免两处手动同步漂移）。
pub const IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "bmp", "tif", "tiff", "gif", "webp", "heic",
];

/// 默认图片根目录（系统 Pictures 目录，含截图子目录；经 [`dirs::picture_dir`]）。无法确定返回空。
#[must_use]
pub fn default_image_roots() -> Vec<PathBuf> {
    dirs::picture_dir().into_iter().collect()
}

/// 构造图片的 [`DocumentEntry`]（doc_type = 小写扩展名，title / author = None，正文另传 OCR 文字）。
fn image_entry(path: &Path, modified_time: i64) -> DocumentEntry {
    DocumentEntry {
        path: path.to_string_lossy().into_owned(),
        file_name: path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string(),
        title: None,
        author: None,
        doc_type: path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default(),
        page_count: None,
        modified_time,
        // BETA-38 doc identity：图片文件原始字节指纹（读取失败降级 None）。
        content_hash: crate::embed::file_identity_hash(path).ok(),
    }
}

#[cfg(test)]
mod exclude_filter_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::fs;

    /// 恒等归一化 helper（跨平台单测用；desktop 走真 normalize_root_key）。
    fn id_norm(s: &str) -> String {
        s.to_owned()
    }

    /// cycle 7-b：兼容层——`from_basename_set` 与旧 basename-only 行为等价。
    #[test]
    fn exclude_filter_from_basename_set_matches_old_behavior() {
        let gs = build_exclude_set(&["node_modules".to_string(), "*cache*".to_string()]);
        let filter = ExcludeFilter::from_basename_set(&gs);
        // 手工构造 walkdir DirEntry 麻烦、直接验 basename GlobSet 语义等价。
        assert!(gs.is_match("node_modules"));
        assert!(filter.basename.is_match("node_modules"));
        assert!(filter.per_root.is_empty(), "兼容构造 per_root 空");
    }

    /// cycle 7-b · Codex SUGGEST 3：以 `/**` 结尾的 pattern 自动追加目录本身 pattern，
    /// 让 walkdir 遇到 dir entry `临时` 时也能命中（否则剪枝失效）。
    #[test]
    fn exclude_filter_build_appends_dir_itself_pattern() {
        let root_ex: &[(String, Vec<String>)] =
            &[("/root".to_string(), vec!["临时/**".to_string()])];
        let filter = ExcludeFilter::build(&[], root_ex, id_norm);
        assert_eq!(filter.per_root.len(), 1);
        let (_, gs) = &filter.per_root[0];
        // 补的 `临时` 目录本身也应命中
        assert!(gs.is_match("临时"), "补充目录本身 pattern");
        assert!(gs.is_match("临时/a.docx"), "原 /** pattern");
        assert!(gs.is_match("临时/子/b.pdf"), "原 /** pattern 深层");
    }

    /// cycle 7-b · Codex SUGGEST 1：Windows 分隔符归一——pattern 用 `/`、
    /// 真实 path 用 `\`，`ExcludeFilter::is_excluded_dir` 内部把 rel 转 `/` 再匹配。
    /// 直接测 GlobSet 匹配（DirEntry 不好造），验证归一化后能命中。
    #[test]
    fn exclude_filter_windows_separator_normalization() {
        let root_ex: &[(String, Vec<String>)] = &[(
            "c:\\users\\roger\\documents".to_string(), // normalize 后 key
            vec!["临时/**".to_string()],
        )];
        let filter = ExcludeFilter::build(&[], root_ex, id_norm);
        let (_, gs) = &filter.per_root[0];
        // 模拟 is_excluded_dir 内部的 rel 归一化：Windows path `临时\a` → `临时/a`
        let rel_norm = "临时\\a".replace('\\', "/");
        assert!(gs.is_match(&rel_norm), "反斜杠转正斜杠后命中");
    }

    /// cycle 7-b：per_root 空 vec 时短路走纯 basename、行为与 from_basename_set 一致。
    #[test]
    fn exclude_filter_empty_root_excludes_short_circuits_to_basename() {
        let gs_bn = build_exclude_set(&["node_modules".to_string()]);
        let filter = ExcludeFilter {
            basename: gs_bn,
            per_root: Vec::new(),
        };
        assert!(filter.basename.is_match("node_modules"));
        assert!(filter.per_root.is_empty());
    }

    /// cycle 7-b：走 walkdir 实测——per-root path glob 真剪枝子树，root 外目录不受影响。
    #[test]
    fn exclude_filter_walkdir_prunes_matching_subtree_per_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("myroot");
        fs::create_dir_all(root.join("keep").join("sub")).unwrap();
        fs::create_dir_all(root.join("临时").join("child")).unwrap();
        fs::write(root.join("keep").join("sub").join("a.txt"), "a").unwrap();
        fs::write(root.join("临时").join("child").join("b.txt"), "b").unwrap();

        let root_key = root.to_string_lossy().into_owned();
        let root_ex: Vec<(String, Vec<String>)> =
            vec![(root_key.clone(), vec!["临时/**".to_string()])];
        let filter = ExcludeFilter::build(&[], &root_ex, id_norm);

        // 遍历 root 用 filter 剪枝；结果不应含 root/临时 及其子。
        let mut found: Vec<String> = Vec::new();
        for entry in WalkDir::new(&root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !filter.is_excluded_dir(e, id_norm))
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() {
                found.push(entry.path().to_string_lossy().into_owned());
            }
        }
        assert!(
            found.iter().any(|p| p.ends_with("a.txt")),
            "keep/sub/a.txt 应保留"
        );
        assert!(
            !found.iter().any(|p| p.contains("临时")),
            "临时 子树整棵剪掉：found={found:?}"
        );
    }

    /// cycle 7-b：per-root exclude 只对指定 root 生效、不影响其他 root。
    #[test]
    fn exclude_filter_per_root_does_not_leak_across_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root_a = dir.path().join("root_a");
        let root_b = dir.path().join("root_b");
        fs::create_dir_all(root_a.join("backup")).unwrap();
        fs::create_dir_all(root_b.join("backup")).unwrap();
        fs::write(root_a.join("backup").join("x.txt"), "x").unwrap();
        fs::write(root_b.join("backup").join("y.txt"), "y").unwrap();

        // 只给 root_a 加 backup/** 排除
        let root_ex: Vec<(String, Vec<String>)> = vec![(
            root_a.to_string_lossy().into_owned(),
            vec!["backup/**".to_string()],
        )];
        let filter = ExcludeFilter::build(&[], &root_ex, id_norm);

        let mut in_a: Vec<String> = Vec::new();
        for entry in WalkDir::new(&root_a)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !filter.is_excluded_dir(e, id_norm))
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() {
                in_a.push(entry.path().to_string_lossy().into_owned());
            }
        }
        assert!(
            !in_a.iter().any(|p| p.contains("backup")),
            "root_a backup 被剪"
        );

        let mut in_b: Vec<String> = Vec::new();
        for entry in WalkDir::new(&root_b)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| !filter.is_excluded_dir(e, id_norm))
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() {
                in_b.push(entry.path().to_string_lossy().into_owned());
            }
        }
        assert!(
            in_b.iter().any(|p| p.ends_with("y.txt")),
            "root_b backup 不受 root_a 排除影响：in_b={in_b:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::model::{MusicEntry, MusicQuery};
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::time::{Duration, SystemTime};

    /// 写一个假音乐文件并把 mtime 设为指定 unix 秒（确定性，不依赖 sleep）。
    fn touch(path: &Path, secs: u64) {
        let mut f = File::create(path).unwrap();
        f.write_all(b"not real audio").unwrap();
        drop(f);
        let f = OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    /// stub 提取器：不解析音频，返回固定字段的 entry（隔离 lofty）。
    /// 必须返回 `Result` 以匹配 `run_incremental_index` 的提取器闭包签名。
    #[allow(clippy::unnecessary_wraps)]
    fn stub(path: &Path, mtime: i64) -> Result<MusicEntry, IndexError> {
        Ok(MusicEntry {
            path: path.to_string_lossy().into_owned(),
            file_name: path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
            artist: Some("StubArtist".to_string()),
            title: Some("StubTitle".to_string()),
            album: None,
            duration_secs: Some(120.0),
            format: Some("MP3".to_string()),
            bitrate: Some(256),
            modified_time: mtime,
        })
    }

    #[test]
    fn first_scan_adds_music_skips_non_music() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.mp3"), 1000);
        touch(&dir.path().join("b.flac"), 1000);
        touch(&dir.path().join("c.MP3"), 1000); // 大写扩展名也算
        touch(&dir.path().join("notes.txt"), 1000); // 非音乐

        let idx = MusicIndex::open_in_memory().unwrap();
        let stats = run_incremental_index(
            &idx,
            &[dir.path().to_path_buf()],
            MUSIC_EXTS,
            &GlobSet::empty(),
            stub,
        )
        .unwrap();
        assert_eq!(stats.scanned, 3, "只数 3 个音乐文件");
        assert_eq!(stats.added, 3);
        assert_eq!(stats.skipped, 0);
        assert_eq!(idx.count().unwrap(), 3);
    }

    #[test]
    fn rescan_unchanged_skips_all() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.mp3"), 1000);
        let idx = MusicIndex::open_in_memory().unwrap();
        let roots = [dir.path().to_path_buf()];
        run_incremental_index(&idx, &roots, MUSIC_EXTS, &GlobSet::empty(), stub).unwrap();
        let stats =
            run_incremental_index(&idx, &roots, MUSIC_EXTS, &GlobSet::empty(), stub).unwrap();
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.added, 0);
        assert_eq!(stats.updated, 0);
    }

    #[test]
    fn changed_mtime_updates() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.mp3");
        touch(&file, 1000);
        touch(&dir.path().join("b.mp3"), 1000);
        let idx = MusicIndex::open_in_memory().unwrap();
        let roots = [dir.path().to_path_buf()];
        run_incremental_index(&idx, &roots, MUSIC_EXTS, &GlobSet::empty(), stub).unwrap();
        // 改一个文件的 mtime。
        let f = OpenOptions::new().write(true).open(&file).unwrap();
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(2000))
            .unwrap();
        drop(f);
        let stats =
            run_incremental_index(&idx, &roots, MUSIC_EXTS, &GlobSet::empty(), stub).unwrap();
        assert_eq!(stats.updated, 1);
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.added, 0);
    }

    #[test]
    fn deleted_file_is_removed_from_index() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.mp3");
        touch(&file, 1000);
        touch(&dir.path().join("b.mp3"), 1000);
        let idx = MusicIndex::open_in_memory().unwrap();
        let roots = [dir.path().to_path_buf()];
        run_incremental_index(&idx, &roots, MUSIC_EXTS, &GlobSet::empty(), stub).unwrap();
        assert_eq!(idx.count().unwrap(), 2);
        std::fs::remove_file(&file).unwrap();
        let stats =
            run_incremental_index(&idx, &roots, MUSIC_EXTS, &GlobSet::empty(), stub).unwrap();
        assert_eq!(stats.removed, 1);
        assert_eq!(idx.count().unwrap(), 1);
    }

    #[test]
    fn removal_does_not_touch_records_outside_roots() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.mp3"), 1000);
        let idx = MusicIndex::open_in_memory().unwrap();
        // 直接插入一条 roots 之外的记录。
        idx.upsert_entry(&MusicEntry {
            path: "/somewhere/else/x.mp3".to_string(),
            file_name: "x.mp3".to_string(),
            modified_time: 1,
            ..Default::default()
        })
        .unwrap();
        let stats = run_incremental_index(
            &idx,
            &[dir.path().to_path_buf()],
            MUSIC_EXTS,
            &GlobSet::empty(),
            stub,
        )
        .unwrap();
        assert_eq!(stats.removed, 0, "roots 外的记录不应被回收");
        assert_eq!(idx.count().unwrap(), 2);
    }

    #[test]
    fn extractor_error_counts_as_failed_not_added() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.mp3"), 1000);
        let idx = MusicIndex::open_in_memory().unwrap();
        let failing = |path: &Path, _mt: i64| -> Result<MusicEntry, IndexError> {
            Err(IndexError::Tag {
                path: path.to_string_lossy().into_owned(),
                detail: "boom".to_string(),
            })
        };
        let stats = run_incremental_index(
            &idx,
            &[dir.path().to_path_buf()],
            MUSIC_EXTS,
            &GlobSet::empty(),
            failing,
        )
        .unwrap();
        assert_eq!(stats.scanned, 1);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.added, 0);
        assert_eq!(idx.count().unwrap(), 0);
    }

    #[test]
    fn extractor_panic_counts_as_failed_not_crash() {
        // 提取器对畸形文件 panic（真机撞 pdf-extract）→ catch_unwind 兜成 failed，不崩整轮。
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.mp3"), 1000);
        touch(&dir.path().join("b.mp3"), 1000);
        let idx = MusicIndex::open_in_memory().unwrap();
        let panicking = |_path: &Path, _mt: i64| -> Result<MusicEntry, IndexError> {
            panic!("提取器炸了（模拟 pdf-extract panic）")
        };
        let stats = run_incremental_index(
            &idx,
            &[dir.path().to_path_buf()],
            MUSIC_EXTS,
            &GlobSet::empty(),
            panicking,
        )
        .expect("整轮不应崩");
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.failed, 2, "两个 panic 都计 failed");
        assert_eq!(stats.added, 0);
    }

    #[test]
    fn extract_failure_recorded_cleared_and_recycled_for_documents() {
        // BETA-40 收尾：文件级提取失败留痕全周期——失败落表 → mtime 未变时不重试 →
        // mtime 变化后重试成功 → 留痕清除 → 磁盘删除回收（2026-07-29 补第二步）。
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        touch_text(&file, "正文内容 hello", 1000);
        let idx = crate::DocumentIndex::open_in_memory().unwrap();
        let roots = [dir.path().to_path_buf()];

        // 第一轮：提取失败 → index_failures 记录 path + reason + 失败时的 mtime。
        let failing = |path: &Path, _mt: i64| -> Result<crate::ExtractedDoc, IndexError> {
            Err(IndexError::Tag {
                path: path.to_string_lossy().into_owned(),
                detail: "模拟提取失败".to_string(),
            })
        };
        let stats =
            run_incremental_index(&idx, &roots, DOC_EXTS, &GlobSet::empty(), failing).unwrap();
        assert_eq!(stats.failed, 1);
        let failures = idx.extraction_failures().unwrap();
        assert_eq!(failures.len(), 1, "失败应留痕");
        assert_eq!(failures[0].path, file.to_string_lossy());
        assert_eq!(
            failures[0].reason, "模拟提取失败",
            "reason 只存细节不重复 path"
        );
        assert_eq!(idx.extraction_failure_count().unwrap(), 1);

        // 第二轮：mtime 未变——即便换成真提取器（本该成功），也不应该被重新调用。
        // 这是本次新增行为的核心断言：已知失败 + 文件没动 → 跳过，不重复烧提取耗时。
        let stats = run_incremental_index(
            &idx,
            &roots,
            DOC_EXTS,
            &GlobSet::empty(),
            crate::doc_extract::extract_document,
        )
        .unwrap();
        assert_eq!(stats.added, 0, "mtime 未变，不该重试");
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.failed, 0, "跳过不是新失败，不重复计 failed");
        assert_eq!(
            idx.extraction_failure_count().unwrap(),
            1,
            "留痕应原样保留，证明确实没有重新调用提取器"
        );

        // 第三轮：mtime 变化（文件被改动，比如用户解除了加密重新保存）→ 应该重试且成功。
        touch_text(&file, "正文内容 hello", 2000);
        let stats = run_incremental_index(
            &idx,
            &roots,
            DOC_EXTS,
            &GlobSet::empty(),
            crate::doc_extract::extract_document,
        )
        .unwrap();
        assert_eq!(stats.added, 1);
        assert!(
            idx.extraction_failures().unwrap().is_empty(),
            "提取成功后留痕应清除"
        );

        // 第四轮：再造一条失败留痕后把文件从磁盘删掉 → 回收阶段清幽灵留痕。
        idx.record_extract_failure(&file.to_string_lossy(), "again", 2000)
            .unwrap();
        std::fs::remove_file(&file).unwrap();
        run_incremental_index(
            &idx,
            &roots,
            DOC_EXTS,
            &GlobSet::empty(),
            crate::doc_extract::extract_document,
        )
        .unwrap();
        assert!(
            idx.extraction_failures().unwrap().is_empty(),
            "文件已删，留痕应随回收清除"
        );
    }

    #[test]
    fn catch_extract_resets_suppress_flag_on_both_paths() {
        // 静默标志只应在「正在被 catch 的提取」期间置位；返回后必须复位，
        // 否则之后本线程真实 panic 会被误吞、stderr 不打印。
        let ok = catch_extract(|| Ok::<u8, IndexError>(7));
        assert_eq!(ok.unwrap(), 7);
        assert!(!IN_CATCH_EXTRACT.with(Cell::get), "成功路径后标志应复位");

        let err = catch_extract(|| -> Result<u8, IndexError> { panic!("boom") });
        assert!(matches!(err, Err(IndexError::Tag { .. })), "panic 兜成 Tag");
        assert!(!IN_CATCH_EXTRACT.with(Cell::get), "panic 路径后标志应复位");
    }

    #[test]
    fn default_music_roots_does_not_panic() {
        let _ = default_music_roots();
    }

    #[test]
    fn empty_query_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.mp3"), 1000);
        let idx = MusicIndex::open_in_memory().unwrap();
        run_incremental_index(
            &idx,
            &[dir.path().to_path_buf()],
            MUSIC_EXTS,
            &GlobSet::empty(),
            stub,
        )
        .unwrap();
        let out = idx.query(&MusicQuery::default()).unwrap();
        assert_eq!(out.len(), 1);
    }

    /// 写文本文件并设 mtime（文档增量测试用）。
    fn touch_text(path: &Path, content: &str, secs: u64) {
        std::fs::write(path, content).unwrap();
        let f = OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn document_index_dirs_end_to_end_txt_md() {
        use crate::doc_db::DocumentIndex;
        use crate::model::DocumentQuery;

        let dir = tempfile::tempdir().unwrap();
        touch_text(&dir.path().join("a.txt"), "季度预算分析报告", 1000);
        touch_text(&dir.path().join("b.md"), "# 标题\n内容关键词在此", 1000);
        touch_text(&dir.path().join("song.mp3"), "binary", 1000); // 非文档不计

        let idx = DocumentIndex::open_in_memory().unwrap();
        let roots = [dir.path().to_path_buf()];

        let stats = idx.index_dirs(&roots).unwrap();
        assert_eq!(stats.scanned, 2, "只数 2 个文档（txt+md），mp3 不计");
        assert_eq!(stats.added, 2);
        assert_eq!(idx.count().unwrap(), 2);

        // 增量：未变跳过。
        let s2 = idx.index_dirs(&roots).unwrap();
        assert_eq!(s2.skipped, 2);
        assert_eq!(s2.added, 0);

        // 删除回收。
        std::fs::remove_file(dir.path().join("a.txt")).unwrap();
        let s3 = idx.index_dirs(&roots).unwrap();
        assert_eq!(s3.removed, 1);
        assert_eq!(idx.count().unwrap(), 1);

        // 查询命中正文。
        let hits = idx
            .query(&DocumentQuery {
                text: Some("内容关键词".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.doc_type, "md");
    }

    #[test]
    fn default_document_roots_does_not_panic() {
        let _ = default_document_roots();
    }

    // ===== BETA-01A：index_paths（并行 + 占位符 + 文件名） =====

    /// 写一个带 artist/title 标签的最小静音 WAV（供 index_paths 真提取测试）。
    fn write_tagged_wav(path: &Path, artist: &str, title: &str) {
        use lofty::config::WriteOptions;
        use lofty::prelude::{AudioFile, TaggedFileExt};
        use lofty::tag::{Accessor, Tag, TagType};

        // 最小合法 PCM WAV（8kHz/单声道/16bit/0.25s 静音）。
        let mut buf: Vec<u8> = Vec::new();
        let data_len: u32 = 2000 * 2;
        buf.extend_from_slice(b"RIFF");
        buf.extend_from_slice(&(36 + data_len).to_le_bytes());
        buf.extend_from_slice(b"WAVEfmt ");
        buf.extend_from_slice(&16u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&8000u32.to_le_bytes());
        buf.extend_from_slice(&16000u32.to_le_bytes());
        buf.extend_from_slice(&2u16.to_le_bytes());
        buf.extend_from_slice(&16u16.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&data_len.to_le_bytes());
        buf.extend(std::iter::repeat_n(0u8, data_len as usize));
        std::fs::write(path, &buf).unwrap();

        let mut tagged = lofty::read_from_path(path).unwrap();
        let mut tag = Tag::new(TagType::RiffInfo);
        tag.set_artist(artist.to_string());
        tag.set_title(title.to_string());
        tagged.insert_tag(tag);
        tagged.save_to_path(path, WriteOptions::default()).unwrap();
    }

    #[test]
    fn index_paths_real_wav_parallel_extracts_and_searchable() {
        use crate::model::MusicQuery;
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("周华健-朋友.wav");
        let b = dir.path().join("song2.wav");
        write_tagged_wav(&a, "周华健", "朋友");
        write_tagged_wav(&b, "Eason", "Hua");
        let txt = dir.path().join("notes.txt");
        std::fs::write(&txt, "x").unwrap();

        let idx = MusicIndex::open_in_memory().unwrap();
        let paths = vec![a.clone(), b.clone(), txt, PathBuf::from("/no/such.wav")];
        let stats = idx.index_paths(&paths, &NoopProgress).unwrap();
        assert_eq!(
            stats.scanned, 3,
            "3 个音乐扩展名（含不存在的 .wav），txt 不计"
        );
        assert_eq!(stats.added, 2, "2 个真 WAV 入库");
        assert_eq!(stats.failed, 1, "不存在的 .wav 计 failed");

        // 按 artist 标签搜。
        let by_artist = idx
            .query(&MusicQuery {
                text: Some("周华健".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_artist.len(), 1);
        // 按文件名搜（song2 无 artist 标签 = Eason，但文件名 song2 可搜）。
        let by_name = idx
            .query(&MusicQuery {
                text: Some("song2".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_name.len(), 1, "应按文件名命中");
    }

    /// 2026-07-28：`MusicIndex::index_paths`（发现层成功路径）此前完全不报逐文件进度
    /// （`on_file`/`on_scope_known` 都不触发）——真机装了 Everything/Spotlight 的常见
    /// 场景下，索引进行中除 phase chip 外几乎看不到任何实时反馈。补齐后验证：total
    /// 与最终 scanned 一致、每个路径都触发一次 on_file、耗时埋点触发一次。
    #[test]
    fn index_paths_reports_scope_known_and_on_file_via_spy() {
        use crate::progress::SpyProgress;
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.wav");
        let b = dir.path().join("b.wav");
        write_tagged_wav(&a, "A", "T1");
        write_tagged_wav(&b, "B", "T2");

        let idx = MusicIndex::open_in_memory().unwrap();
        let spy = SpyProgress::default();
        let stats = idx
            .index_paths(&[a, b, PathBuf::from("/no/such.wav")], &spy)
            .unwrap();

        assert_eq!(stats.scanned, 3);
        assert_eq!(
            spy.scope_known_calls.load(Ordering::Relaxed),
            1,
            "预检结束后触发一次 on_scope_known"
        );
        assert_eq!(
            spy.last_scope_total.load(Ordering::Relaxed),
            3,
            "total 与最终 scanned 一致"
        );
        assert_eq!(
            spy.files.load(Ordering::Relaxed),
            3,
            "每个路径（含不存在的一条）都触发一次 on_file"
        );
        assert_eq!(
            spy.stage_timings_calls.load(Ordering::Relaxed),
            1,
            "整轮结束触发一次 on_stage_timings"
        );
    }

    /// 同上，覆盖 `DocumentIndex::index_paths_with_known_mtimes`（`index_discovered_paths`
    /// 共用骨架，文档/图片两轮共用）。
    #[test]
    fn document_index_paths_with_known_mtimes_reports_scope_known_and_on_file_via_spy() {
        use crate::doc_db::DocumentIndex;
        use crate::progress::SpyProgress;
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        touch_text(&dir.path().join("a.txt"), "first body", 1000);
        touch_text(&dir.path().join("b.txt"), "second body", 1000);
        let paths = vec![dir.path().join("a.txt"), dir.path().join("b.txt")];

        let idx = DocumentIndex::open_in_memory().unwrap();
        let known_mtimes = std::collections::HashMap::new();
        let spy = SpyProgress::default();
        let stats = idx
            .index_paths_with_known_mtimes(&paths, &known_mtimes, &spy)
            .unwrap();

        assert_eq!(stats.scanned, 2);
        assert_eq!(spy.scope_known_calls.load(Ordering::Relaxed), 1);
        assert_eq!(spy.last_scope_total.load(Ordering::Relaxed), 2);
        assert_eq!(
            spy.files.load(Ordering::Relaxed),
            2,
            "发现层路径此前不触发 on_file，这是本轮要修的缺口"
        );
        assert_eq!(spy.stage_timings_calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn index_paths_skips_unchanged_on_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.wav");
        write_tagged_wav(&a, "A", "T");
        let idx = MusicIndex::open_in_memory().unwrap();
        let paths = vec![a.clone()];
        let s1 = idx.index_paths(&paths, &NoopProgress).unwrap();
        assert_eq!(s1.added, 1);
        let s2 = idx.index_paths(&paths, &NoopProgress).unwrap();
        assert_eq!(s2.skipped, 1, "mtime 未变应跳过");
        assert_eq!(s2.added, 0);
    }

    #[test]
    fn filename_only_entry_has_only_name() {
        let e = filename_only_entry(Path::new("/m/周华健-朋友.mp3"), 1234);
        assert_eq!(e.file_name, "周华健-朋友.mp3");
        assert_eq!(e.modified_time, 1234);
        assert!(e.artist.is_none(), "占位符不读标签");
        assert!(e.title.is_none());
        assert!(e.duration_secs.is_none());
    }

    // ===== BETA-03：图片 OCR 索引 + 回收按扩展名收窄 =====

    /// stub OCR 引擎：不读文件，返回固定文字（或失败），隔离真 OCR。
    #[derive(Debug)]
    struct StubOcr {
        text: String,
        fail: bool,
    }
    impl crate::ocr::OcrEngine for StubOcr {
        fn recognize(&self, _image: &Path) -> Result<String, IndexError> {
            if self.fail {
                Err(IndexError::Tag {
                    path: String::new(),
                    detail: "stub fail".to_string(),
                })
            } else {
                Ok(self.text.clone())
            }
        }
        fn name(&self) -> &'static str {
            "Stub"
        }
    }

    #[test]
    fn recycle_is_scoped_to_round_extensions() {
        use crate::doc_db::DocumentIndex;
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("shot.png"), 1000); // 图片
        touch_text(&dir.path().join("notes.txt"), "纯文本笔记内容", 1000); // 文档
        let roots = [dir.path().to_path_buf()];
        let stub = StubOcr {
            text: "图片里的文字内容".to_string(),
            fail: false,
        };
        let idx = DocumentIndex::open_in_memory().unwrap();

        // 两轮各自入库：图片轮加 png、文档轮加 txt。
        assert_eq!(idx.index_image_dirs(&roots, &stub).unwrap().added, 1);
        assert_eq!(idx.index_dirs(&roots).unwrap().added, 1);
        assert_eq!(idx.count().unwrap(), 2);

        // 再跑文档轮：png 不在 DOC_EXTS → 不应被回收。
        let doc_again = idx.index_dirs(&roots).unwrap();
        assert_eq!(doc_again.removed, 0, "文档轮不得回收图片");
        assert_eq!(idx.count().unwrap(), 2);

        // 再跑图片轮：txt 不在 IMAGE_EXTS → 不应被回收。
        let img_again = idx.index_image_dirs(&roots, &stub).unwrap();
        assert_eq!(img_again.removed, 0, "图片轮不得回收文档");
        assert_eq!(idx.count().unwrap(), 2);
    }

    #[test]
    fn index_image_dirs_end_to_end_with_stub_ocr() {
        use crate::doc_db::DocumentIndex;
        use crate::model::DocumentQuery;

        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.png"), 1000);
        touch(&dir.path().join("b.jpg"), 1000);
        touch_text(&dir.path().join("c.txt"), "x", 1000); // 非图片不计
        let roots = [dir.path().to_path_buf()];
        let stub = StubOcr {
            text: "会议纪要第三季度".to_string(),
            fail: false,
        };
        let idx = DocumentIndex::open_in_memory().unwrap();

        let stats = idx.index_image_dirs(&roots, &stub).unwrap();
        assert_eq!(stats.scanned, 2, "只数 2 个图片（png+jpg），txt 不计");
        assert_eq!(stats.added, 2);
        assert_eq!(idx.count().unwrap(), 2);

        // OCR 文字进 FTS，可命中（trigram 需 ≥3 字符）。
        let hits = idx
            .query(&DocumentQuery {
                text: Some("会议纪要".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits
            .iter()
            .all(|h| ["png", "jpg"].contains(&h.entry.doc_type.as_str())));

        // mtime 未变跳过。
        let again = idx.index_image_dirs(&roots, &stub).unwrap();
        assert_eq!(again.skipped, 2);
        assert_eq!(again.added, 0);

        // 删一张图片 → 回收。
        std::fs::remove_file(dir.path().join("a.png")).unwrap();
        let after_del = idx.index_image_dirs(&roots, &stub).unwrap();
        assert_eq!(after_del.removed, 1);
        assert_eq!(idx.count().unwrap(), 1);
    }

    /// BETA-64 T5：`DocumentIndex::index_paths`（发现层给显式路径列表）与
    /// `index_dirs`（WalkDir 全遍历）产出等价的入库结果——验证新增的
    /// `index_discovered_paths` 骨架（含 T2 批量 upsert）行为正确。
    #[test]
    fn document_index_paths_indexes_explicit_list_and_is_searchable() {
        use crate::doc_db::DocumentIndex;
        use crate::model::DocumentQuery;

        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("report.txt");
        let b = dir.path().join("notes.md");
        touch_text(&a, "季度预算分析报告正文", 1000);
        touch_text(&b, "会议纪要", 1000);
        let png = dir.path().join("shot.png"); // 不在 DOC_EXTS，应被跳过不计数

        let idx = DocumentIndex::open_in_memory().unwrap();
        let paths = vec![
            a.clone(),
            b.clone(),
            png,
            PathBuf::from("/no/such/report.txt"),
        ];
        let stats = idx.index_paths(&paths, &NoopProgress).unwrap();
        assert_eq!(stats.scanned, 3, "2 个真文档 + 1 个不存在的 .txt，png 不计");
        assert_eq!(stats.added, 2);
        assert_eq!(stats.failed, 1, "不存在的文件计 failed");

        // trigram FTS 需 ≥3 字符（<3 字符走短查询 metadata-only 兜底、不扫 body）。
        let hits = idx
            .query(&DocumentQuery {
                text: Some("预算分析".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1);

        // mtime 未变 → 跳过（同 index_dirs 语义）。
        let again = idx.index_paths(&paths, &NoopProgress).unwrap();
        assert_eq!(again.skipped, 2);
        assert_eq!(again.added, 0);
    }

    /// BETA-64 T5：`DocumentIndex::index_image_paths`（发现层）与
    /// `index_image_dirs`（WalkDir）产出等价结果，OCR 文字可搜。
    #[test]
    fn document_index_image_paths_with_stub_ocr_is_searchable() {
        use crate::doc_db::DocumentIndex;
        use crate::model::DocumentQuery;

        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.png");
        let b = dir.path().join("b.jpg");
        touch(&a, 1000);
        touch(&b, 1000);
        let txt = dir.path().join("c.txt"); // 不在 IMAGE_EXTS，应被跳过不计数
        touch_text(&txt, "x", 1000);

        let stub = StubOcr {
            text: "会议纪要第三季度".to_string(),
            fail: false,
        };
        let idx = DocumentIndex::open_in_memory().unwrap();
        let paths = vec![a, b, txt];
        let stats = idx.index_image_paths(&paths, &stub, &NoopProgress).unwrap();
        assert_eq!(stats.scanned, 2, "只数 2 个图片，txt 不计");
        assert_eq!(stats.added, 2);

        let hits = idx
            .query(&DocumentQuery {
                text: Some("会议纪要".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn index_image_dirs_ocr_failure_counts_failed_not_crash() {
        use crate::doc_db::DocumentIndex;
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.png"), 1000);
        touch(&dir.path().join("b.png"), 1000);
        let roots = [dir.path().to_path_buf()];
        let stub = StubOcr {
            text: String::new(),
            fail: true,
        };
        let idx = DocumentIndex::open_in_memory().unwrap();
        let stats = idx.index_image_dirs(&roots, &stub).unwrap();
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.failed, 2, "OCR 失败计 failed");
        assert_eq!(stats.added, 0);
        assert_eq!(idx.count().unwrap(), 0);
    }

    #[test]
    fn default_image_roots_does_not_panic() {
        let _ = default_image_roots();
    }

    // ===== BETA-32 C1a：IndexProgress callback + _with_progress 变体 =====

    #[test]
    fn document_index_dirs_with_progress_calls_callback() {
        use crate::doc_db::DocumentIndex;
        use crate::progress::SpyProgress;
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        touch_text(&dir.path().join("a.txt"), "first body", 1000);
        touch_text(&dir.path().join("b.txt"), "second body", 1000);
        let idx = DocumentIndex::open_in_memory().unwrap();
        let spy = SpyProgress::default();

        let stats = idx
            .index_dirs_with_progress(&[dir.path().to_path_buf()], &spy)
            .unwrap();

        assert_eq!(stats.scanned, 2, "扫到 2 个 txt");
        assert_eq!(stats.added, 2, "两个都新增");
        assert_eq!(
            spy.files.load(Ordering::Relaxed),
            2,
            "每文件触发一次 on_file"
        );
        assert_eq!(
            spy.batches.load(Ordering::Relaxed),
            1,
            "整批结束触发一次 on_batch_done"
        );
        // on_batch_done 实参契约：scanned 真带 stats.scanned、indexed 真带 added+updated。
        assert_eq!(
            spy.last_scanned.load(Ordering::Relaxed),
            stats.scanned as u64,
            "on_batch_done 收到正确 scanned 实参"
        );
        assert_eq!(
            spy.last_indexed.load(Ordering::Relaxed),
            (stats.added + stats.updated) as u64,
            "on_batch_done 收到正确 indexed 实参"
        );
    }

    /// 2026-07-28：`on_scope_known` 在 walk 结束、逐文件 `on_file` 完成之前就已经拿到
    /// 正确总数（3 个 txt，含一个会被跳过的场景不在本测试覆盖——这里只验证首轮全新增
    /// 场景下 total == scanned == files 触发次数）；`on_stage_timings` 每轮触发一次。
    #[test]
    fn document_index_dirs_with_progress_reports_scope_and_stage_timings() {
        use crate::doc_db::DocumentIndex;
        use crate::progress::SpyProgress;
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        touch_text(&dir.path().join("a.txt"), "first body", 1000);
        touch_text(&dir.path().join("b.txt"), "second body", 1000);
        touch_text(&dir.path().join("c.txt"), "third body", 1000);
        let idx = DocumentIndex::open_in_memory().unwrap();
        let spy = SpyProgress::default();

        let stats = idx
            .index_dirs_with_progress(&[dir.path().to_path_buf()], &spy)
            .unwrap();

        assert_eq!(stats.scanned, 3);
        assert_eq!(
            spy.scope_known_calls.load(Ordering::Relaxed),
            1,
            "walk 结束后触发一次 on_scope_known"
        );
        assert_eq!(
            spy.last_scope_total.load(Ordering::Relaxed),
            3,
            "total 与最终 scanned 一致"
        );
        assert_eq!(
            spy.stage_timings_calls.load(Ordering::Relaxed),
            1,
            "整轮结束触发一次 on_stage_timings"
        );
        assert!(
            spy.last_stage_timings
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_some(),
            "on_stage_timings 收到具体数值"
        );
    }

    #[test]
    fn music_index_dirs_with_progress_calls_callback() {
        use crate::progress::SpyProgress;
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.wav");
        let b = dir.path().join("b.wav");
        write_tagged_wav(&a, "A1", "T1");
        write_tagged_wav(&b, "A2", "T2");
        let idx = MusicIndex::open_in_memory().unwrap();
        let spy = SpyProgress::default();

        let stats = idx
            .index_dirs_with_progress(&[dir.path().to_path_buf()], &spy)
            .unwrap();

        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.added, 2);
        assert_eq!(spy.files.load(Ordering::Relaxed), 2);
        assert_eq!(spy.batches.load(Ordering::Relaxed), 1);
        assert_eq!(
            spy.last_scanned.load(Ordering::Relaxed),
            stats.scanned as u64,
            "on_batch_done 收到正确 scanned 实参"
        );
        assert_eq!(
            spy.last_indexed.load(Ordering::Relaxed),
            (stats.added + stats.updated) as u64,
            "on_batch_done 收到正确 indexed 实参"
        );
    }

    #[test]
    fn with_progress_fires_on_failed_path_when_extractor_panics() {
        // 覆盖 scan.rs catch_extract 边界：提取器 panic → 计 failed → on_file 仍触发、
        // 整批结束 on_batch_done 触发一次（indexed=0、failed 不算 indexed）。
        use crate::progress::SpyProgress;
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a.mp3"), 1000);
        touch(&dir.path().join("b.mp3"), 1000);
        let idx = MusicIndex::open_in_memory().unwrap();
        let panicking = |_p: &Path, _mt: i64| -> Result<MusicEntry, IndexError> {
            panic!("提取器炸了（模拟 pdf-extract panic）")
        };
        let spy = SpyProgress::default();

        let stats = run_incremental_index_with_progress(
            &idx,
            &[dir.path().to_path_buf()],
            MUSIC_EXTS,
            &GlobSet::empty(),
            panicking,
            &spy,
        )
        .expect("整轮不应崩");

        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.failed, 2, "两个 panic 都计 failed");
        assert_eq!(stats.added, 0);
        // 关键：catch_extract 边界之后 callback 仍存活、每文件都触发一次 on_file。
        assert_eq!(
            spy.files.load(Ordering::Relaxed),
            2,
            "failed 路径仍触发 on_file（panic 不漏报 daemon）"
        );
        assert_eq!(
            spy.batches.load(Ordering::Relaxed),
            1,
            "整批结束触发一次 on_batch_done"
        );
        assert_eq!(
            spy.last_indexed.load(Ordering::Relaxed),
            0,
            "全 failed 时 indexed = 0"
        );
        assert_eq!(spy.last_scanned.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn with_progress_fires_on_skipped_path_when_mtime_unchanged() {
        // 覆盖 scan.rs mtime-skip 分支：rerun 全 skip → on_file 仍每文件触发、
        // on_batch_done 触发一次（indexed=0、skipped 不算 indexed）。
        use crate::doc_db::DocumentIndex;
        use crate::progress::SpyProgress;
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().unwrap();
        touch_text(&dir.path().join("a.txt"), "x", 1000);
        touch_text(&dir.path().join("b.txt"), "y", 1000);
        let idx = DocumentIndex::open_in_memory().unwrap();
        let roots = [dir.path().to_path_buf()];

        // 首轮入库，spy 不关心。
        idx.index_dirs(&roots).unwrap();

        // 二轮：mtime 未变 → 全 skipped。
        let spy = SpyProgress::default();
        let stats = idx.index_dirs_with_progress(&roots, &spy).unwrap();
        assert_eq!(stats.skipped, 2);
        assert_eq!(stats.added, 0);
        assert_eq!(stats.updated, 0);
        assert_eq!(
            spy.files.load(Ordering::Relaxed),
            2,
            "skipped 路径仍触发 on_file"
        );
        assert_eq!(spy.batches.load(Ordering::Relaxed), 1);
        assert_eq!(
            spy.last_indexed.load(Ordering::Relaxed),
            0,
            "全 skipped 时 indexed = 0"
        );
        assert_eq!(spy.last_scanned.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn index_dirs_with_progress_default_noop_matches_old_behavior() {
        // 桌面 app 行为等价红线：旧 index_dirs 与 _with_progress(NoopProgress) 结果完全一致。
        use crate::doc_db::DocumentIndex;
        use crate::progress::NoopProgress;

        let dir = tempfile::tempdir().unwrap();
        touch_text(&dir.path().join("a.txt"), "hello", 1000);
        touch_text(&dir.path().join("b.md"), "# t\n内容", 1000);

        let idx_old = DocumentIndex::open_in_memory().unwrap();
        let idx_new = DocumentIndex::open_in_memory().unwrap();
        let roots = [dir.path().to_path_buf()];

        let s_old = idx_old.index_dirs(&roots).unwrap();
        let s_new = idx_new
            .index_dirs_with_progress(&roots, &NoopProgress)
            .unwrap();

        assert_eq!(s_old.scanned, s_new.scanned);
        assert_eq!(s_old.added, s_new.added);
        assert_eq!(s_old.updated, s_new.updated);
        assert_eq!(s_old.skipped, s_new.skipped);
        assert_eq!(s_old.removed, s_new.removed);
        assert_eq!(s_old.failed, s_new.failed);
    }

    // ===== BETA-15B-1 B2：embed_pending 内联补嵌 =====

    /// stub embedder：把文本映射成确定性 3 维向量（隔离真模型）。
    struct StubEmbedder;
    impl crate::embed::TextEmbedder for StubEmbedder {
        #[allow(clippy::cast_precision_loss)]
        fn embed(&self, text: &str) -> Result<Vec<f32>, IndexError> {
            Ok(vec![
                text.chars().count() as f32,
                if text.contains('报') { 1.0 } else { 0.0 },
                1.0,
            ])
        }
        fn model_id(&self) -> &'static str {
            "stub-emb"
        }
    }

    /// 总是失败的 embedder（验证单篇失败计数但不中断、文档仍 FTS 可搜）。
    struct FailingEmbedder;
    impl crate::embed::TextEmbedder for FailingEmbedder {
        fn embed(&self, _text: &str) -> Result<Vec<f32>, IndexError> {
            Err(IndexError::Tag {
                path: String::new(),
                detail: "stub embed fail".to_string(),
            })
        }
        fn model_id(&self) -> &'static str {
            "failing-emb"
        }
    }

    #[test]
    fn embed_pending_tolerates_embed_failure() {
        use crate::doc_db::DocumentIndex;
        use crate::model::DocumentQuery;
        let dir = tempfile::tempdir().unwrap();
        // BETA-31-v3 cycle 5：body ≥ 20 字符以通过 cycle 3 加的 is_embed_worthy 守门
        touch_text(
            &dir.path().join("a.txt"),
            "季度预算分析报告，包含本季度营收同比数据与下季度预测说明",
            1000,
        );
        let idx = DocumentIndex::open_in_memory().unwrap();
        let roots = [dir.path().to_path_buf()];

        // 先建 FTS（embed_pending 只补向量，不建 FTS）。
        let stats = idx.index_dirs(&roots).unwrap();
        assert_eq!(stats.added, 1, "文档仍入库");

        // 直测 embed_pending：单篇失败计数、不中断。
        let (embedded, reused, failed) = idx
            .embed_pending(&roots, &FailingEmbedder, false, &mut |_, _| {})
            .unwrap();
        assert_eq!(embedded, 0);
        assert_eq!(reused, 0, "无副本可复用");
        assert_eq!(failed, 1, "嵌入失败计数");
        assert!(idx.candidate_vectors().unwrap().is_empty(), "失败不写向量");
        // 文档仍 FTS 可搜（嵌入失败不影响 FTS）。
        let hits = idx
            .query(&DocumentQuery {
                text: Some("季度预算".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(hits.len(), 1, "嵌入失败的文档仍 FTS 可搜");
    }

    #[test]
    fn embed_pending_reports_progress_and_skips_current() {
        use crate::doc_db::DocumentIndex;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("idx.db");
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        // BETA-31-v3 cycle 5：body ≥ 20 字符通过 cycle 3 加的 is_embed_worthy 守门
        std::fs::write(
            docs.join("a.txt"),
            "alpha body content padded to twenty plus chars for embed test.",
        )
        .unwrap();
        std::fs::write(
            docs.join("b.txt"),
            "beta body content padded to twenty plus chars for embed test.",
        )
        .unwrap();
        let roots = vec![docs.clone()];

        let idx = DocumentIndex::open(&db).unwrap();
        idx.index_dirs(&roots).unwrap(); // 先建 FTS（embed_pending 只补向量，不建 FTS）

        // 首轮：两篇待嵌，进度回调单调到 (2,2)。
        let mut seen: Vec<(usize, usize)> = Vec::new();
        let (embedded, reused, failed) = idx
            .embed_pending(&roots, &StubEmbedder, false, &mut |done, total| {
                seen.push((done, total));
            })
            .unwrap();
        assert_eq!(
            (embedded, reused, failed),
            (2, 0, 0),
            "两篇都新嵌、零复用零失败"
        );
        assert_eq!(seen.last().copied(), Some((2, 2)), "进度终值 (total,total)");
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0), "done 单调不减");

        // 二轮：全 vector_is_current 命中 → 待嵌 0、回调不触发。
        let mut seen2: Vec<(usize, usize)> = Vec::new();
        let (e2, r2, f2) = idx
            .embed_pending(&roots, &StubEmbedder, false, &mut |d, t| seen2.push((d, t)))
            .unwrap();
        assert_eq!((e2, r2, f2), (0, 0, 0), "二轮无新嵌");
        assert!(seen2.is_empty(), "无待嵌时不回调");
    }

    /// BETA-38 cycle 2：副本去重——两份内容完全相同的文件只 embed 一次、另一份复用向量；
    /// 不同内容的文件各自 embed。返回 `(embedded, reused, failed)` 计数区分。
    #[test]
    fn embed_pending_dedups_identical_content_copies() {
        use crate::doc_db::DocumentIndex;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("idx.db");
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        // 两份完全相同内容（同 content_hash）+ 一份不同内容，均 ≥20 字符过 is_embed_worthy。
        let same = "季度预算报告正文内容用于副本去重测试足够长度";
        std::fs::write(docs.join("orig.txt"), same).unwrap();
        std::fs::write(docs.join("copy.txt"), same).unwrap();
        std::fs::write(
            docs.join("other.txt"),
            "另一份完全不同的材料正文内容也足够长度用于对照的异内容文档测试",
        )
        .unwrap();
        let roots = vec![docs.clone()];

        let idx = DocumentIndex::open(&db).unwrap();
        idx.index_dirs(&roots).unwrap();
        let (embedded, reused, failed) = idx
            .embed_pending(&roots, &StubEmbedder, false, &mut |_, _| {})
            .unwrap();
        // 2 个不同 content_hash（same×2 折一 + other）→ embed 2；same 的第二份复用 → reused 1。
        assert_eq!(
            (embedded, reused, failed),
            (2, 1, 0),
            "同内容只嵌一次、副本复用、异内容各嵌"
        );
        // 三个文档都拿到向量（复用也 upsert）。
        assert_eq!(idx.vector_count().unwrap(), 3, "三份文档都落向量");
        // 两副本向量逐位相同（复用是精确复制）。
        let cands = idx.candidate_vectors().unwrap();
        let orig = dir
            .path()
            .join("docs/orig.txt")
            .to_string_lossy()
            .into_owned();
        let copy = dir
            .path()
            .join("docs/copy.txt")
            .to_string_lossy()
            .into_owned();
        let v_orig = cands.iter().find(|c| c.path == orig).map(|c| &c.vector);
        let v_copy = cands.iter().find(|c| c.path == copy).map(|c| &c.vector);
        assert_eq!(v_orig, v_copy, "副本向量应与原件逐位相同");
    }

    #[test]
    fn vector_count_reflects_embedded_documents() {
        use crate::doc_db::DocumentIndex;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("idx.db");
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        // BETA-31-v3 cycle 5：body ≥ 20 字符通过 cycle 3 加的 is_embed_worthy 守门
        std::fs::write(
            docs.join("a.txt"),
            "alpha body content padded to twenty plus chars for embed test.",
        )
        .unwrap();
        std::fs::write(
            docs.join("b.txt"),
            "beta body content padded to twenty plus chars for embed test.",
        )
        .unwrap();
        let roots = vec![docs];

        let idx = DocumentIndex::open(&db).unwrap();
        // 空库（建表后无向量）→ 0。
        assert_eq!(idx.vector_count().unwrap(), 0, "空库无向量");

        // 建 FTS 后补嵌 → vector_count 等于嵌入篇数。
        idx.index_dirs(&roots).unwrap();
        let (embedded, reused, failed) = idx
            .embed_pending(&roots, &StubEmbedder, false, &mut |_, _| {})
            .unwrap();
        assert_eq!((embedded, reused, failed), (2, 0, 0), "两篇都新嵌");
        assert_eq!(
            idx.vector_count().unwrap(),
            u64::try_from(embedded).unwrap(),
            "vector_count 等于嵌入篇数"
        );
    }

    // ===== BETA-27：目录名通配符排除（globset filter_entry 短路） =====

    #[test]
    fn build_exclude_set_matches_basenames() {
        let set = build_exclude_set(&["node_modules".to_string(), "*cache*".to_string()]);
        assert!(set.is_match("node_modules"));
        assert!(set.is_match("mycache"));
        assert!(!set.is_match("src"));
    }

    #[test]
    fn build_exclude_set_empty_never_matches() {
        let set = build_exclude_set(&[]);
        assert!(!set.is_match("node_modules"));
        assert!(!set.is_match("anything"));
    }

    #[test]
    fn build_exclude_set_skips_invalid_glob() {
        let set = build_exclude_set(&["[".to_string(), "node_modules".to_string()]);
        assert!(set.is_match("node_modules"));
    }

    #[test]
    fn index_dirs_excluding_prunes_subtree() {
        use crate::doc_db::DocumentIndex;
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        let nm = docs.join("node_modules");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(docs.join("keep.txt"), "hello").unwrap();
        std::fs::write(nm.join("junk.txt"), "junk").unwrap();
        let db = dir.path().join("idx.db");
        let idx = DocumentIndex::open(&db).unwrap();
        let exclude = build_exclude_set(&["node_modules".to_string()]);
        idx.index_dirs_excluding(std::slice::from_ref(&docs), &exclude)
            .unwrap();
        assert_eq!(
            idx.count().unwrap(),
            1,
            "仅 keep.txt 入库，node_modules 被剪枝"
        );
    }

    #[test]
    fn index_dirs_empty_exclude_equals_old_behavior() {
        use crate::doc_db::DocumentIndex;
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        let nm = docs.join("node_modules");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(docs.join("keep.txt"), "hello").unwrap();
        std::fs::write(nm.join("junk.txt"), "junk").unwrap();
        let db = dir.path().join("idx.db");
        let idx = DocumentIndex::open(&db).unwrap();
        idx.index_dirs(std::slice::from_ref(&docs)).unwrap();
        assert_eq!(idx.count().unwrap(), 2, "无排除时全索引（含 node_modules）");
    }
}
