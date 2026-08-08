# packages/search-backends/windows-search

Windows Search（SystemIndex）后端。

**状态**：MVP-11 骨架完成，待 Windows 实测。

## 实现路线

MVP 用 OLE DB + Search SQL：
- 语法稳定、跨语言可调（Rust / C# / Node 通过 ADO 调用）
- 翻译 SearchIntent → `SELECT ... FROM SystemIndex WHERE ...`
- 必须**参数化 SQL**，避免注入；本 crate 的 `WindowsSearchQuery.sql` 不包含用户输入，用户输入只进入 `params`

Beta 评估 `Windows.Storage.Search`（WinRT）。

## 当前实现

- `WindowsSearchBackend` 已实现 `SearchBackend` v0.2 async/streaming 接口。
- `translate_intent()` 覆盖 schema §7.1-§7.4 共 30 条搜索用例。
- Windows API 调用收敛到异步 `WindowsSearchExecutor` trait，单元测试用 async mock executor。
- `PlatformWindowsSearchExecutor` 用 `#[cfg(target_os = "windows")]` 包住平台实现边界；真实 ADO/OLE DB 调用尚未接入。
- Location hint 依赖 `platform/windows` 的 Known Folders 占位 resolver。
- 结果端统一按 `intent.sort` 做客户端 post-sort；`RelevanceDesc` 保留后端默认序。

## 关键约束

- 默认只索引部分目录（用户配置文件等），其他目录需用户加入索引 → 首次启动一次性提示
- 索引服务可能被企业策略禁用 → 返回 `WINDOWS_SEARCH_DISABLED`，降级提示
- SystemIndex SQL 不支持 `LIMIT` → 结果端截断
- 当前实现在 macOS 上只能跑 mock-based 单元测试；真实查询必须在 Windows 11 + Windows Search 索引完成后验证

详细设计见 [docs/local-personal-search-agent-project-plan.md §6.3](../../../docs/local-personal-search-agent-project-plan.md)。
