# packages/search-backends/spotlight

macOS Spotlight 搜索后端。

**状态**：PROTO-05 首版已完成；MVP-07A 已迁移到 `SearchBackend` v0.2 async/streaming。

## 实现路线

原型期用 `mdfind` 命令行：
- 进程调用简单、无需特殊权限、Apple 长期支持
- 输入 SearchIntent → 翻译成 `kMDItem*` 谓词表达式
- 用 `-onlyin` 限制路径，结果按行解析
- 通过 `Command` 结构化参数传递，禁止 shell 拼接
- 超时后 kill 子进程；结果端过滤 `location.exclude`
- 使用 `std::fs::metadata` 补基础 metadata
- MVP-07A 后对外返回 `BackendStream`；内部暂保留同步 `mdfind` 壳子并包进 boxed future，原因是当前 lockfile 未包含 Tokio process/macros 相关依赖，且 `mdfind` 需要先收齐结果才能做客户端 post-sort。
- `mdfind` 无原生 sort，后端会先收集结果、按 `intent.sort` 做客户端 post-sort，再流式产出；`relevance_desc` 保留 `mdfind` 默认序。

Beta 评估切换到 `NSMetadataQuery`（流式结果、原生集成）。

## 测试覆盖

- fixture #1-#30 查询翻译全覆盖。
- shell 注入防护：关键词转义双引号与反斜杠。
- 非搜索 intent 返回 `UnsupportedIntent`。
- fake `mdfind` 覆盖 post-sort、取消前置停止、超时 kill 子进程。

## 关键约束

- 不索引"系统设置 → Spotlight → 隐私"中排除的目录 → 必须返回 `SPOTLIGHT_DIRECTORY_EXCLUDED`
- macOS 14+ 访问 `~/Library` 等受保护目录需要 Full Disk Access → 返回 `FULL_DISK_ACCESS_REQUIRED`
- 默认不主动请求 FDA，仅在用户搜索失败时引导

详细设计见 [docs/local-personal-search-agent-project-plan.md §6.2](../../../docs/local-personal-search-agent-project-plan.md)。
