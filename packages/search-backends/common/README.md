# packages/search-backends/common

`SearchBackend` trait 与归一化结果类型。

**状态**：PROTO-02 / PROTO-03 / PROTO-04 / PROTO-04A 已完成；MVP-07A 已迁移到 v0.2 async/streaming 接口。

## 计划职责

- Search Intent v1.0 serde 类型。
- `SearchBackend` trait：v0.2 对外只暴露异步 boxed future + `BackendStream`，签名为 `search(intent, cancel) -> BackendSearchFuture`；保留 dyn dispatch，适配 `Box<dyn SearchBackend>`。
- 统一的 `SearchResult` / `SearchResultMetadata` / `SearchError`（含 `UnsupportedIntent`）。
- `CancellationToken`：轻量 `Arc<AtomicBool>` 取消信号，避免新增 `tokio-util` 并保持当前 lockfile 可离线构建。
- `sort_results()`：统一客户端 post-sort，覆盖 `modified/created/accessed/size/name` 排序；`relevance_desc` 保留后端默认序。
- `BackendKind` / `ImplementationStatus` / `BackendRegistry`，生产 fallback 链自动剔除 stub。
- `LocationResolver` trait，由平台 crate 提供实现。
- `tests/fixtures/cases.json` 50 条 fixture 与 schema/serde 交叉测试。

详细设计见 [docs/local-personal-search-agent-project-plan.md §6](../../../docs/local-personal-search-agent-project-plan.md)。
