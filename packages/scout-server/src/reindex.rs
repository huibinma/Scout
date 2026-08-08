//! `/admin/reindex` 后台逻辑：per-collection `IN_FLIGHT` guard + 真增量 reindex。
//!
//! BETA-32 T7 落了全局 guard + stub；BETA-36 把粒度拆到 collection（`?collection=<id>`
//! 指名重建单个集合、省略时顺序跑全部**非只读**集合；`read_only=true` 的集合被显式
//! 指名 → [`ReindexError::ReadOnly`]（409，冻结语义冲突））；**BETA-36 follow-up
//! （2026-07-03）接真 indexer**：
//!
//! - **增量而非 atomic swap 全量重建**（对 BETA-32 spec §5.3 的实现修订）：
//!   复用 `index_dirs_with_progress`（mtime skip + 磁盘已删记录回收），与桌面
//!   `perform_reindex` 同款语义。放弃 rename swap 的原因：① Windows 上 rename 被
//!   `CollectionRuntime` 持有的 rusqlite 打开句柄挡住（需先换出连接、时序复杂）；
//!   ② 增量已覆盖"新增 / 修改 / 删除"全部日常场景。schema 变更级的全量重建仍走
//!   daemon 重启 + `--allow-rebuild-schema`（preflight 残留检查因此保留）。
//! - 完成后写回 per-collection `state.doc_count` / `indexed_at`。
//!
//! **PRIVACY CONTRACT**：`Internal(String)` 可能含本机绝对路径；按 spec §6.2 隐私
//! 硬规则、**不允许放 HTTP response body**，仅可进 tracing log。handler 只返
//! status code 不带 error body 是合规的。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use thiserror::Error;

use scout_indexer::embed::TextEmbedder;
use scout_indexer::{DocumentIndex, GlobSet, IndexStats, NoopProgress};

use crate::admin::ReindexResp;
use crate::config::{CollectionRuntime, ServerCtx};

/// reindex 触发期间可能产生的错误。
#[derive(Debug, Error)]
pub enum ReindexError {
    /// 指名的 collection 不存在（handler 映射 404）。
    #[error("collection 不存在：{0}")]
    UnknownCollection(String),
    /// 指名的 collection 是只读态冷冻归档（handler 映射 409）。
    #[error("collection 为只读态（冷冻归档），拒绝 reindex：{0}")]
    ReadOnly(String),
    /// 该 collection 已有 reindex 在进行中（`IN_FLIGHT` guard 命中），handler 映射 409。
    #[error("已有 reindex 在进行中：{0}")]
    InFlight(String),
    /// 内部错误（indexer 失败 / atomic swap 失败等），handler 映射 500。
    #[error("索引失败：{0}")]
    Internal(String),
}

/// 触发一次 reindex。
///
/// - `collection = Some(id)`：只重建该集合；不存在 → [`ReindexError::UnknownCollection`]、
///   只读 → [`ReindexError::ReadOnly`]、已在跑 → [`ReindexError::InFlight`]。
/// - `collection = None`：顺序重建全部**非只读**集合（只读集合静默跳过——冷冻归档
///   不参与全量重建是预期语义）；任一集合在跑 → InFlight（保持整体互斥简单性）。
///
/// # Errors
///
/// 见 [`ReindexError`] 各 variant。
pub async fn trigger_reindex(
    ctx: Arc<ServerCtx>,
    collection: Option<&str>,
) -> Result<ReindexResp, ReindexError> {
    let targets: Vec<&CollectionRuntime> = match collection {
        Some(id) => {
            let rt = ctx
                .collection(id)
                .ok_or_else(|| ReindexError::UnknownCollection(id.to_string()))?;
            if rt.meta.read_only {
                return Err(ReindexError::ReadOnly(id.to_string()));
            }
            vec![rt]
        }
        None => ctx
            .collections
            .values()
            .filter(|rt| !rt.meta.read_only)
            .collect(),
    };

    // 逐个原子检查并抢占；若中途发现冲突，已创建 guard 会自动回滚此前抢到的 flags。
    // guards 必须在进入执行循环前一次性建齐：否则第一个 collection 失败提前返回时，
    // 尚未轮到的 collection 已被置 true、却没有对应 guard 负责复位，会永久卡在 InFlight。
    let mut guards = acquire_reindex_guards(&targets)?;

    let started = Instant::now();
    let mut total_doc_count: u64 = 0;
    let mut reindexed: Vec<String> = Vec::with_capacity(targets.len());
    for (rt, guard) in targets.iter().zip(guards.iter_mut()) {
        // guard 接管 flag 复位职责：FTS 阶段失败时随本次调用的提前 return / panic 正常
        // drop（立即清 flag，语义不变）；FTS 成功则随 `run_collection_reindex` 转交给
        // 后台 embedding 任务持有，直到语义向量补齐也跑完才真正 drop——flag 覆盖整个
        // reindex 生命周期（含后台 embedding），而不只是同步 FTS 阶段（T6b 修订：此前
        // guard 在本函数返回时就 drop，`run_collection_reindex` 却已经把 embedding 挪去
        // 后台异步跑，导致 InFlight 保护提前失效——语义向量还没补完，第二次
        // `/admin/reindex` 就已经能成功触发，两轮 embedding 并发跑，见 T6b 文档注释）。
        let Some(guard) = guard.take() else {
            return Err(ReindexError::Internal(
                "reindex guard 内部状态不一致".to_owned(),
            ));
        };
        // 真增量 reindex；错误链先落 tracing（可能含路径）、HTTP 侧只透 status code。
        let n = run_collection_reindex(rt, ctx.embedder.clone(), ctx.config.embed_images, guard)
            .await
            .map_err(|e| {
                tracing::error!(collection = %rt.meta.id, error = ?e, "collection reindex 失败");
                ReindexError::Internal(e.to_string())
            })?;
        // 写回 per-collection 状态（list_collections 的 doc_count / indexed_at 数据源）。
        {
            let mut st = rt.state.write();
            st.doc_count = n;
            st.indexed_at = Some(chrono::Utc::now());
        }
        total_doc_count = total_doc_count.saturating_add(n);
        reindexed.push(rt.meta.id.clone());
    }

    Ok(ReindexResp {
        status: "completed",
        collections: reindexed,
        doc_count: total_doc_count,
        duration_ms: started.elapsed().as_millis(),
    })
}

/// RAII guard：drop 时把对应 collection 的 `reindex_in_flight` 复位为 false。
///
/// 生命周期覆盖整个 reindex（含 T6b 挪去后台的 embedding 阶段）——`trigger_reindex`
/// 构造后传给 `run_collection_reindex`：FTS 阶段失败时随 `?` 提前 return 正常 drop；
/// FTS 成功则被 `spawn_background_embed` 的 `tokio::spawn` 闭包整体接管（`Send`，
/// 字段只有 `Arc<RwLock<...>>`），直到后台 embedding 任务完整跑完（`Ok`/`Err`/panic
/// 任一路径）才真正 drop、清 flag。
struct ReindexGuard {
    state: Arc<parking_lot::RwLock<crate::config::CollectionState>>,
}

fn acquire_reindex_guards(
    targets: &[&CollectionRuntime],
) -> Result<Vec<Option<ReindexGuard>>, ReindexError> {
    let mut guards = Vec::with_capacity(targets.len());
    for rt in targets {
        let mut state = rt.state.write();
        if state.reindex_in_flight {
            return Err(ReindexError::InFlight(rt.meta.id.clone()));
        }
        state.reindex_in_flight = true;
        drop(state);
        guards.push(Some(ReindexGuard {
            state: rt.state.clone(),
        }));
    }
    Ok(guards)
}

impl Drop for ReindexGuard {
    fn drop(&mut self) {
        self.state.write().reindex_in_flight = false;
    }
}

/// 单 collection 真增量 reindex：`music` + `document` + **图片 OCR** 三轮增量
/// （mtime skip + 回收），返回该集合当前索引总数（music + documents，含图片）。
///
/// **BETA-64 T6b（2026-07-25）：语义向量 pass 挪至后台**——此前三轮 FTS 之后
/// 同步跑 `embed_pending`、把 `/admin/reindex` 的响应时延同 `apps/daemon`
/// `run_initial_collection_index` 摘除前一样，完整绑上了 embedding 耗时（B5：
/// batch=1 + 每次新建 llama.cpp context）。桌面 daemon 首次索引早已在 T6
/// 把 embedding 挪出关键路径（[`apps/daemon/src/main.rs`] `spawn_background_embedding`），
/// 但管理员触发的增量 reindex 走的是本文件独立实现、未同步这处修订——三轮 FTS
/// 一写完即可搜索，语义向量在 detached [`tokio::spawn`] 里补齐；`embed_pending`
/// 幂等可续（`vector_is_current`/`content_hash` 去重），与并发触发的下一次
/// reindex 或并发 search 共享同一把 `document_index` `Mutex`、按 `SQLite` 单连接
/// 语义天然排队，不会数据竞争。
///
/// - **图片轮**：per-call 现场探测 OCR 引擎——admin 装好依赖后无需重启 daemon、
///   下次 reindex 即生效（镜像桌面 onboarding「自动重检」精神）；不可用 → warn 跳过。
/// - **embed pass**：embedder ping 通过才跑（stub 构建自动跳过）；`embed_images`
///   由 `ServerConfig` 注入（daemon 默认 true——企业场景图片证据检索 + 2 字 CJK
///   词语义臂唯一兜底；BETA-39 双层质量门槛沿用，`--disable-image-semantics` 关闭）。
///
/// indexer 是 sync（rusqlite + rayon），放 [`tokio::task::spawn_blocking`] 跑；
/// `Mutex` 长持有到 FTS 三轮写完——期间 MCP search 走 `LocalIndexBackend`（独立连接、
/// 只读）不受阻，`list_collections` 读 state 也不经过这两把锁。
async fn run_collection_reindex(
    rt: &CollectionRuntime,
    embedder: Arc<dyn TextEmbedder>,
    embed_images: bool,
    guard: ReindexGuard,
) -> anyhow::Result<u64> {
    let music = rt.music_index.clone();
    let document = rt.document_index.clone();
    let roots = rt.meta.roots.clone();
    let id = rt.meta.id.clone();
    let (music_count, document_count) = tokio::task::spawn_blocking({
        let roots = roots.clone();
        let id = id.clone();
        let document = document.clone();
        move || -> anyhow::Result<(u64, u64)> {
            // 2026-07-28：执行顺序改为「文档 → 图片 → 音频」——面向工作场景，文档
            // 量最大且最先被搜索，图片次之，音频文件量少、优先级最低排最后
            // （详见 docs/index-performance-design.md §9）。
            let document = document.lock();
            let d_stats = document.index_dirs_with_progress(&roots, &NoopProgress)?;
            let i_stats = if let Some(ocr) = scout_indexer::default_ocr_engine() {
                document.index_image_dirs_excluding_with_progress(
                    &roots,
                    ocr.as_ref(),
                    &GlobSet::empty(),
                    &NoopProgress,
                )?
            } else {
                tracing::warn!(
                    collection = %id,
                    "无可用 OCR 引擎（Windows.Media.Ocr / Tesseract），本轮跳过图片索引"
                );
                IndexStats::default()
            };
            let document_count = document.count()?;
            let extraction_failures = document.extraction_failure_count().unwrap_or(0);
            drop(document);
            let music = music.lock();
            let m_stats = music.index_dirs_with_progress(&roots, &NoopProgress)?;
            let music_count = music.count()?;
            drop(music);
            tracing::info!(
                collection = %id,
                music_scanned = m_stats.scanned,
                document_scanned = d_stats.scanned,
                document_added = d_stats.added,
                document_updated = d_stats.updated,
                document_removed = d_stats.removed,
                document_failed = d_stats.failed,
                image_scanned = i_stats.scanned,
                image_added = i_stats.added,
                image_failed = i_stats.failed,
                extraction_failures,
                "collection FTS 三轮 reindex 完成（语义向量后台补齐中）"
            );
            Ok((music_count, document_count))
        }
    })
    .await
    .context("reindex 任务 panic 或被取消")??;

    spawn_background_embed(id, roots, document, embedder, embed_images, guard);

    Ok(music_count.saturating_add(document_count))
}

/// BETA-64 T6b：`run_collection_reindex` 的语义向量补齐后台任务，镜像
/// [`apps/daemon/src/main.rs`] `spawn_background_embedding` 的 detach 模式
/// （两处未合并为共享 helper——`apps/daemon` 在 `ServerCtx` 构造期跑、本处在
/// 请求处理期跑，触发时机和生命周期不同，勉强合并只会增加间接层）。
/// detached 运行：`/admin/reindex` HTTP 响应不等待、进程收到关闭信号时随
/// runtime drop 一并终止；`embed_pending` 幂等可续，下次 reindex 接着补。
fn spawn_background_embed(
    collection_id: String,
    roots: Vec<PathBuf>,
    document: Arc<parking_lot::Mutex<DocumentIndex>>,
    embedder: Arc<dyn TextEmbedder>,
    embed_images: bool,
    guard: ReindexGuard,
) {
    tokio::spawn(async move {
        // 把 guard 的生命周期绑定到整个后台任务（含下面的 embedding 与 tracing），
        // 而不是 `spawn_background_embed` 函数体本身（那一步立刻返回，`tokio::spawn`
        // 不等任务跑完）——`reindex_in_flight` 真正覆盖到语义向量补齐完成为止，见
        // `ReindexGuard`/`trigger_reindex` 文档注释。
        let _guard = guard;
        let embed_start = Instant::now();
        let result =
            tokio::task::spawn_blocking(move || -> anyhow::Result<(usize, usize, usize)> {
                if embedder.embed("ping").is_err() {
                    return Ok((0, 0, 0));
                }
                let document = document.lock();
                // 与 apps/daemon/src/main.rs 的 spawn_background_embedding 对齐：先清掉
                // body 变短/变空后失效的旧向量（如图片 OCR 不可用时残留的空向量），
                // 否则这些"中性"向量会在 ranker 里污染 top-N（BETA-31-v3 cycle 3 实锤）。
                // 此前本函数遗漏了这一步，管理员触发的 reindex 路径不会清理，daemon
                // 首次索引路径才清，两条路径行为不一致。
                document.purge_short_body_vectors(embed_images)?;
                Ok(document.embed_pending(
                    &roots,
                    embedder.as_ref(),
                    embed_images,
                    &mut |_, _| {},
                )?)
            })
            .await;
        let embed_ms = embed_start.elapsed().as_millis();
        match result {
            Ok(Ok((embed_new, embed_reused, embed_failed))) => tracing::info!(
                collection = %collection_id,
                embedded = embed_new,
                embed_reused,
                embed_failed,
                embed_ms,
                "collection 语义向量后台补齐完成（reindex 触发）"
            ),
            Ok(Err(error)) => tracing::warn!(
                collection = %collection_id,
                %error,
                "collection 语义向量后台补齐失败（reindex 触发；FTS 检索不受影响）"
            ),
            Err(error) => tracing::warn!(
                collection = %collection_id,
                %error,
                "collection 语义向量后台补齐任务 panic 或被取消（reindex 触发；FTS 检索不受影响）"
            ),
        }
    });
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::test_support::{build_test_ctx_inmem, build_test_ctx_multi_inmem};

    /// 第二次并发触发应直接拿到 `InFlight` 错误。
    #[tokio::test]
    async fn concurrent_reindex_second_returns_in_flight() {
        let ctx = build_test_ctx_inmem();
        // 模拟"已有 reindex 在跑"（default collection）。
        ctx.collections["default"].state.write().reindex_in_flight = true;
        let err = trigger_reindex(ctx, None).await.unwrap_err();
        assert!(
            matches!(err, ReindexError::InFlight(_)),
            "并发触发应返 InFlight，实得：{err:?}"
        );
    }

    /// guard drop 后应自动复位 flag。
    #[tokio::test]
    async fn guard_clears_flag_on_drop() {
        let ctx = build_test_ctx_inmem();
        let state = ctx.collections["default"].state.clone();
        {
            let _g = ReindexGuard {
                state: state.clone(),
            };
            state.write().reindex_in_flight = true;
        }
        assert!(
            !state.read().reindex_in_flight,
            "guard drop 后 reindex_in_flight 应被复位为 false"
        );
    }

    #[test]
    fn acquiring_multiple_guards_rolls_back_all_flags_on_drop_or_conflict() {
        let ctx = build_test_ctx_multi_inmem();
        let a = &ctx.collections["case-a"];
        let b = &ctx.collections["case-b"];

        {
            let guards = acquire_reindex_guards(&[a, b]).unwrap();
            assert!(a.state.read().reindex_in_flight);
            assert!(b.state.read().reindex_in_flight);
            drop(guards);
        }
        assert!(!a.state.read().reindex_in_flight);
        assert!(!b.state.read().reindex_in_flight);

        b.state.write().reindex_in_flight = true;
        let Err(err) = acquire_reindex_guards(&[a, b]) else {
            panic!("第二个 target 已在执行时必须返回 InFlight");
        };
        assert!(matches!(err, ReindexError::InFlight(_)));
        assert!(
            !a.state.read().reindex_in_flight,
            "后续 target 冲突时，前面已经抢到的 flag 必须由 guard 自动回滚"
        );
        b.state.write().reindex_in_flight = false;
    }

    /// 轮询等待 `reindex_in_flight` 复位——T6b 修订后 guard 转交给后台 embedding
    /// 任务持有，`trigger_reindex` 返回时该任务多半还没被运行时调度到，flag 不一定
    /// 已经清；测试侧用短轮询而非固定 `sleep` 换稳定性（stub embedder + 空 roots
    /// 的后台任务本身几乎是瞬时完成，轮询上限给够余量、不做成计时依赖的脆弱断言）。
    async fn wait_until_not_in_flight(
        state: &Arc<parking_lot::RwLock<crate::config::CollectionState>>,
    ) {
        for _ in 0..200 {
            if !state.read().reindex_in_flight {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("等待 reindex_in_flight 复位超时（后台 embedding 任务疑似未完成/未被调度）");
    }

    /// happy path：成功 reindex 后 guard 自动 drop（flag 清）+ state 写回。
    ///
    /// inmem ctx 的 root 路径不存在：WalkDir 空扫（0 文件）、in-memory db 计数 0——
    /// 覆盖"真跑但空结果"路径；带 corpus 的真盘路径由 e2e 覆盖。
    ///
    /// T6b 修订：`reindex_in_flight` 现在覆盖到后台 embedding 任务完成为止（见
    /// `ReindexGuard` 文档），`trigger_reindex` 返回时不保证已经清——本测试先轮询
    /// 等它清，再断言其余 state 字段，语义仍是"reindex 完整跑完后状态应如此"。
    #[tokio::test]
    async fn trigger_reindex_clears_flag_and_writes_back_state() {
        let ctx = build_test_ctx_inmem();
        let resp = trigger_reindex(ctx.clone(), None).await.unwrap();
        assert_eq!(resp.status, "completed");
        assert_eq!(resp.collections, vec!["default"]);
        assert_eq!(resp.doc_count, 0, "root 不存在 → 空扫 0 doc");
        wait_until_not_in_flight(&ctx.collections["default"].state).await;
        assert!(
            ctx.collections["default"].state.read().indexed_at.is_some(),
            "真实 reindex 完成后应写回 indexed_at"
        );
        assert_eq!(ctx.collections["default"].state.read().doc_count, 0);
    }

    /// T6b 修订的回归测试：`trigger_reindex` 返回（FTS 阶段完成）后，
    /// `reindex_in_flight` 应仍是 `true`（后台 embedding 任务还没跑完），此时立刻
    /// 再触发一次应被 `InFlight` 拒绝——而不是像 T6b 引入 bug 之前那样静默接受、
    /// 让两轮 embedding 并发跑。`#[tokio::test]` 默认 current-thread 单线程运行时：
    /// `tokio::spawn` 出去的后台任务在测试代码下一次 `.await` 之前不会被调度执行，
    /// 所以"返回后立刻再触发"这个时序在单线程运行时下是确定性的，不是靠运气踩中窗口。
    #[tokio::test]
    async fn second_trigger_immediately_after_first_returns_in_flight() {
        let ctx = build_test_ctx_inmem();
        let resp = trigger_reindex(ctx.clone(), None).await.unwrap();
        assert_eq!(resp.status, "completed", "FTS 阶段应已完成");
        assert!(
            ctx.collections["default"].state.read().reindex_in_flight,
            "FTS 阶段完成后、后台 embedding 任务跑完前，reindex_in_flight 应仍为 true"
        );
        let err = trigger_reindex(ctx.clone(), None).await.unwrap_err();
        assert!(
            matches!(err, ReindexError::InFlight(_)),
            "后台 embedding 未完成时立刻再触发应返 InFlight，实得：{err:?}"
        );
        // 收尾：等后台任务跑完，flag 复位，避免影响同进程内其他测试（虽然每个测试
        // 各有独立 ctx，这里只是保持断言完整闭环）。
        wait_until_not_in_flight(&ctx.collections["default"].state).await;
    }

    /// 指名不存在的 collection → `UnknownCollection`。
    #[tokio::test]
    async fn unknown_collection_rejected() {
        let ctx = build_test_ctx_inmem();
        let err = trigger_reindex(ctx, Some("nonexistent")).await.unwrap_err();
        assert!(matches!(err, ReindexError::UnknownCollection(_)));
    }

    /// 指名只读 collection → ReadOnly；省略时只读集合被静默跳过。
    #[tokio::test]
    async fn read_only_collection_rejected_when_named_skipped_when_all() {
        let ctx = build_test_ctx_multi_inmem();
        // test_support multi builder：case-a 只读、case-b 读写。
        let err = trigger_reindex(ctx.clone(), Some("case-a"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ReindexError::ReadOnly(_)),
            "指名只读集合应返 ReadOnly，实得：{err:?}"
        );

        let resp = trigger_reindex(ctx, None).await.unwrap();
        assert_eq!(
            resp.collections,
            vec!["case-b"],
            "省略 collection 时只读集合应被跳过"
        );
    }

    /// 指名读写 collection → 只重建它。
    #[tokio::test]
    async fn named_collection_reindexes_only_that_one() {
        let ctx = build_test_ctx_multi_inmem();
        let resp = trigger_reindex(ctx, Some("case-b")).await.unwrap();
        assert_eq!(resp.collections, vec!["case-b"]);
    }
}
