//! 单卷索引服务：一次性 MFT 枚举建初始索引 + 后台线程持续读取 USN Journal
//! 增量更新。是 crate 对外的主要入口（[`crate::NativeIndexService`] 重导出本模块类型）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
#[cfg(windows)]
use std::time::Duration;

use crate::error::NativeIndexError;
#[cfg(windows)]
use crate::index::FileRecord;
use crate::index::{MemIndex, NameQuery};
#[cfg(windows)]
use crate::record::parse_usn_records;
#[cfg(windows)]
use crate::sys;
#[cfg(windows)]
use crate::sys::VolumeHandle;

#[cfg(windows)]
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
#[cfg(windows)]
const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;

/// 单个 NTFS 卷的原生索引服务：初次构建 + 后台增量维护。
///
/// `Drop` 时停止后台线程（`join`，不做 detach——与项目内既有惯例
/// [`docs/index-performance-design.md` §5.2] 的教训一致：不 join 的后台线程句柄
/// 会在进程生命周期边界产生资源竞态）。
#[derive(Debug)]
pub struct NativeIndexService {
    drive_letter: char,
    index: Arc<RwLock<MemIndex>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl NativeIndexService {
    /// 为指定盘符启动索引服务：同步做一次全量 MFT 枚举（`FSCTL_ENUM_USN_DATA`，
    /// 阻塞直至枚举完卷内全部记录），随后启动后台线程持续 tail USN Journal。
    ///
    /// 失败场景：非 Windows 平台、盘符不存在、卷非 NTFS、卷未启用 USN Journal、
    /// 进程无管理员权限（打开卷句柄的 Win32 硬性要求）。调用方应把失败视为
    /// "原生索引不可用"，回退到 `WalkDir` 全量扫描（与原 Everything 发现层
    /// 同款优雅降级契约，见 `packages/indexer/src/discovery.rs`）。
    pub fn start(drive_letter: char) -> Result<Self, NativeIndexError> {
        #[cfg(not(windows))]
        {
            let _ = drive_letter;
            Err(NativeIndexError::UnsupportedPlatform)
        }

        #[cfg(windows)]
        {
            let volume = sys::open_volume(drive_letter)?;
            let journal = sys::query_usn_journal(&volume)?;

            let mut index = MemIndex::new();
            populate_full_index(&volume, &mut index)?;
            let index = Arc::new(RwLock::new(index));

            let stop = Arc::new(AtomicBool::new(false));
            let worker = spawn_tail_worker(
                volume,
                journal.UsnJournalID,
                journal.NextUsn,
                Arc::clone(&index),
                Arc::clone(&stop),
            );

            Ok(Self {
                drive_letter,
                index,
                stop,
                worker: Some(worker),
            })
        }
    }

    /// 本服务索引的盘符。
    #[must_use]
    pub const fn drive_letter(&self) -> char {
        self.drive_letter
    }

    /// 当前索引记录数（近似值——读锁获取瞬间的快照，后台线程可能同时在写）。
    #[must_use]
    pub fn record_count(&self) -> usize {
        self.index.read().map_or(0, |idx| idx.len())
    }

    /// 大小写不敏感文件名子串搜索，见 [`MemIndex::search_substring`]。
    #[must_use]
    pub fn search(&self, query: &str, limit: usize) -> Vec<PathBuf> {
        self.index.read().map_or_else(
            |_| Vec::new(),
            |idx| idx.search_substring(query, self.drive_letter, limit),
        )
    }

    /// 按扩展名集合全盘发现，见 [`MemIndex::search_by_extensions`]。
    #[must_use]
    pub fn search_by_extensions(&self, extensions: &[&str], limit: usize) -> Vec<PathBuf> {
        self.index.read().map_or_else(
            |_| Vec::new(),
            |idx| idx.search_by_extensions(extensions, self.drive_letter, limit),
        )
    }

    /// 组合条件查询，见 [`MemIndex::search_query`]。
    #[must_use]
    pub fn search_query(&self, query: &NameQuery, limit: usize) -> Vec<PathBuf> {
        self.index.read().map_or_else(
            |_| Vec::new(),
            |idx| idx.search_query(query, self.drive_letter, limit),
        )
    }
}

impl Drop for NativeIndexService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(windows)]
fn populate_full_index(
    volume: &VolumeHandle,
    index: &mut MemIndex,
) -> Result<(), NativeIndexError> {
    let mut buf = vec![0u8; sys::IO_BUFFER_SIZE].into_boxed_slice();
    let mut start_frn = 0u64;
    while let Some((next_frn, bytes)) = sys::enum_usn_data(volume, start_frn, &mut buf)? {
        for raw in parse_usn_records(bytes) {
            index.upsert(FileRecord {
                frn: raw.frn,
                parent_frn: raw.parent_frn,
                name: raw.name,
                is_directory: raw.attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
            });
        }
        if next_frn == start_frn {
            // 防御：理论上 EOF 应由 `enum_usn_data` 返回 `None` 触发，这里兜底避免
            // 游标不前进导致死循环。
            break;
        }
        start_frn = next_frn;
    }
    Ok(())
}

/// 后台 tail 线程：轮询 `FSCTL_READ_USN_JOURNAL`，把增量变更应用到共享索引。
/// 轮询而非阻塞等待（`Timeout`/`BytesToWaitFor` 置零）——实现简单、无需额外
/// 事件对象，轮询间隔 [`POLL_INTERVAL`] 在"变更感知延迟"与"空轮询 CPU 占用"
/// 间取折中，符合"极低资源占用"目标（远低于 Everything 服务的常驻内存/CPU）。
#[cfg(windows)]
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 单次读失败后连续重试的上限（仅计瞬时错误，`JournalInvalidated` 不计入、
/// 直接终止）——超过则放弃 tail 并记录终态日志，避免对一个已经彻底坏掉的卷
/// 无限重试空转；正常瞬时 I/O 故障（网络卷抖动、句柄短暂繁忙）预期在几次
/// 退避内自愈，一旦某次成功即清零计数（见调用处）。
#[cfg(windows)]
const MAX_CONSECUTIVE_TRANSIENT_FAILURES: u32 = 5;

#[cfg(windows)]
fn spawn_tail_worker(
    volume: VolumeHandle,
    journal_id: u64,
    start_usn: i64,
    index: Arc<RwLock<MemIndex>>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut cursor = start_usn;
        let mut buf = vec![0u8; sys::IO_BUFFER_SIZE].into_boxed_slice();
        let mut consecutive_failures = 0u32;
        while !stop.load(Ordering::SeqCst) {
            match sys::read_usn_journal(&volume, journal_id, cursor, &mut buf) {
                Ok((next_usn, bytes)) => {
                    consecutive_failures = 0;
                    if bytes.is_empty() {
                        std::thread::sleep(POLL_INTERVAL);
                        continue;
                    }
                    let records = parse_usn_records(bytes);
                    if let Ok(mut idx) = index.write() {
                        for raw in records {
                            if raw.reason & USN_REASON_FILE_DELETE != 0 {
                                idx.remove(raw.frn);
                            } else {
                                idx.upsert(FileRecord {
                                    frn: raw.frn,
                                    parent_frn: raw.parent_frn,
                                    name: raw.name,
                                    is_directory: raw.attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
                                });
                            }
                        }
                    }
                    cursor = next_usn;
                }
                Err(NativeIndexError::JournalInvalidated { detail }) => {
                    // Journal 被删除重建（id 失效）：不可恢复，重试无意义，直接停止。
                    // 索引停留在最后一次成功状态（不完全但仍可用）——调用方若需要严格
                    // 一致性应重新 `NativeIndexService::start` 触发一次全量重建。
                    tracing::error!(
                        detail,
                        "USN journal 已失效，tail 线程停止；索引停留在最后一次成功状态，\
                         需重新全量重建才能恢复实时增量"
                    );
                    break;
                }
                Err(e) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= MAX_CONSECUTIVE_TRANSIENT_FAILURES {
                        tracing::error!(
                            error = %e,
                            consecutive_failures,
                            "USN journal 读取连续失败超过上限，tail 线程放弃重试并停止；\
                             索引停留在最后一次成功状态"
                        );
                        break;
                    }
                    tracing::warn!(
                        error = %e,
                        consecutive_failures,
                        "USN journal 读取失败（判定为瞬时错误），退避后重试"
                    );
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
        }
    })
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn start_reports_unsupported_platform_off_windows() {
        assert!(matches!(
            NativeIndexService::start('C'),
            Err(NativeIndexError::UnsupportedPlatform)
        ));
    }
}
