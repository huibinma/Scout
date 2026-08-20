# packages/harness

Agent Harness 工程底座。crate 名 `scout-harness`。

**状态**：M1 子阶段 12/12 代码已落地。MVP-01 / 02 / 03 / 04 / 05 / 06 / 07 / 07A / 08 / 09 / 10 / 10A 已完成实现。

## 已落地能力

### MVP-01 Tool Registry（核心）

- **`Tool` trait** —— 所有可调用工具的最小公约数（id / name / kind / capability / implementation_status / is_available）。
- **`ToolKind`** —— `Search` / `FileAction`（`non_exhaustive`，后续可扩 OCR / 索引器 / 模型）。
- **`ToolCapability`** —— 工具能力声明：描述 + 支持的 intent 列表 + 支持的文件操作 + 可选 backend 身份。
- **`SupportedIntent`** —— `SearchIntent` 五变体的轻量枚举，用于 Capability Discovery / Intent Router。
- **`SearchTool<B: SearchBackend>`** —— 把任意 `SearchBackend` 适配为 `Tool`，注册到 `ToolRegistry`。
- **`ToolRegistry`** —— 按 id 索引；`register` / `find_by_id` / `all_tools` / `tools_by_kind`；生产链 API（`production_tools` / `production_tools_supporting` / `available_tools_supporting`）自动剔除 `ImplementationStatus::Stub`。

### M1 第 2 批

- **`SchemaValidator`（MVP-02）** —— 基于 JSON Schema 的运行时 Intent 校验，确保输入严格合规。
- **`PolicyEngine`（MVP-03）** —— 权限分级 L0–L5；搜索默认允许，L4 写操作确认，L5 删除拒绝。
- **`ToolLoopController`（MVP-04）** —— 多步工具循环控制；支持最大步数、整体超时、单步超时和取消信号。
- **`IntentRouter`（MVP-05）** —— 基于 `SearchIntent` 与 `ToolRegistry` 的确定性路由；按 id 升序选择首个可用真实工具。
- **`ContextMemory`（MVP-06）** —— 最近一轮 intent + 结果快照；`resolve_target_ref` 支持 Path / LastResults×{Index, Indices, All}；`apply_refine` 按 schema §3.4 合并语义（Some 覆盖、clear 清空、同字段冲突取 clear）。
- **`Tracer` & `TracingHook`（MVP-08）** —— 工具调用追踪系统，支持事件钩子（如 `JsonLinesHook`），内置隐私脱敏（路径不含 home / 用户名；query 不记录）。
- **`CapabilityDiscovery`（MVP-09）** —— 生产链能力查询接口，暴露支持的 intent/action 并集与后端状态摘要。

### M1 第 3 批

- **`ResultStream` / `StreamSink`（MVP-07）** —— 同步 `Iterator<Item = ResultEvent>` 搜索结果流；可把 v0.1 backend 的完整 `Vec<SearchResult>` 包装为 `Started -> Result* -> Finished`，并支持事件间取消。
- **`FallbackChain`（MVP-10）** —— 基于 `CapabilityDiscovery` + `ToolRegistry` 生成确定性候选链；系统索引优先，内置原生索引（`NativeFileIndex`，替代原 Everything 集成）次之，`NativeIndex`/`SemanticIndex` 最后，失败时保留完整错误链。

### M1 第 4 批

- **`SearchBackend` v0.2 async/streaming 迁移（MVP-07A）** —— `SearchTool::invoke` 改为 async，返回 `BackendStream`；`ResultStream` 改为 `Stream<Item = ResultEvent>`，内部用 channel 包装完整结果；取消信号端到端接入 backend stream。
- **接口选择** —— 未使用 `async-trait`/AFIT，而是 boxed future：`BackendSearchFuture<'a>`。原因是必须保留 `Box<dyn SearchBackend>` dyn dispatch，同时当前离线 lockfile 不适合新增 proc-macro 依赖。

## 与 `BackendRegistry` 的关系

- `BackendRegistry`（在 `scout-search-backend`）只管 `SearchBackend`，是 PROTO-04 的产出。
- `ToolRegistry` 高一层，管所有工具种类。MVP-10A 的 `FileActionTool` 将直接实现 `Tool`，注册到本注册表。
- 在 CLI / 直接调单一 backend 的代码路径可以继续用 `BackendRegistry`；Harness 调度层一律用 `ToolRegistry`。

## 验收（MVP-01 出场指标）

- `Tool` trait + `ToolRegistry` 数据结构完整（见 `src/lib.rs`）
- 单元测试覆盖：注册 / 查找 / 重复 id 拒绝 / 按 kind 过滤 / **生产链剔除 stub** / 可用性过滤 / `SupportedIntent::from_intent` 全变体
- ROADMAP §6.1 / §6.2 出场指标"Stub backend 不进入生产 fallback 链"在 Harness 层的落实入口为 `ToolRegistry::production_tools`。

### M1 第 4 批（主会话）

- **`FileActionTool` / `FileActionExecutor`（MVP-10A）** —— Tool 抽象下的文件操作工具；集成 PolicyEngine + ContextMemory；open/locate/copy/move/rename 五动作，delete 双重禁用；批量阈值默认 10；跨卷 move fallback；schema §7.6 #36/#38/#39/#40 契约测试通过。

## 待解锁

M1 已无剩余 task；后续 UI 流式结果列表可直接消费 `BackendStream` / `ResultStream`。

详细职责见 [`docs/local-personal-search-agent-project-plan.md` §8](../../docs/local-personal-search-agent-project-plan.md)。
