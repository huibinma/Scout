# packages/search-backends/everything

Windows 可选加速后端：Everything by voidtools。

**状态**：MVP-12 命令翻译 + ES CLI 执行层已在 Windows 11 真机端到端实测；`search_expanded` 同义词扩展（BETA-15C）已覆盖。

## 实现路线

- MVP：ES CLI（subprocess + 结构化输出）
- Beta：Everything SDK（IPC，更低延迟）

## 当前实现

- `EverythingBackend` 已实现 `SearchBackend` v0.2 async/streaming 接口。
- CLI 调用被收敛到异步 `EverythingExecutor` trait；真实 `es.exe` 执行器仍是 Windows 实测占位，mock executor 覆盖结构化参数与结果归一化。
- `translate_intent()` 输出 `EverythingCommand { program, args, ... }`，测试覆盖命令构造、恶意关键词单参数传递、mock executor。
- `ImplementationStatus::Real` 仅在 Windows 且 `es.exe` 可检测到时返回；当前 macOS 环境返回 `Stub`，不会进入生产 fallback 链。
- 结果经 `es.exe -export-txt <tmp> -utf8-bom` 导出为 UTF-8(+BOM) 文件再逐行读回（剥 BOM），
  **规避 stdout 控制台代码页对 CJK 文件名的破坏**（中文 Windows 默认 GBK，真机实测 stdout 会乱码）；
  用 `std::fs::metadata` 尽量补全基础 metadata 以支持结果端 post-sort。
- 结果端统一按 `intent.sort` 做客户端 post-sort；`RelevanceDesc` 保留后端默认序。
- `search_expanded`（BETA-15C）：同义词组在文件名层面 `|` OR 展开（`<head|syn1|syn2>`），
  组间 AND；singleton 组退化为裸词，与 `search` 路径产出 byte-equal。Everything 是纯文件名
  引擎，扩展只作用于文件名（内容查询由 Windows Search 后端承担，二者经能力感知路由分工）。

## 关键约束

- **不是产品主体**，仅在用户已安装 Everything 时作为 Windows 默认后端的加速替换
- 检测策略：`es.exe` 在显式路径或 `PATH` 中存在 → backend 可用；后续 Windows 实测需补服务运行状态与版本检查
- 不强制安装、不打扰式弹窗、不在主界面广告 Everything
- 错误码：`EVERYTHING_NOT_INSTALLED` / `EVERYTHING_NOT_RUNNING` / `EVERYTHING_VERSION_TOO_OLD`
- 分发合规：不分发 Everything 二进制；如需分发 ES portable，必须列出 voidtools + PCRE 许可
- macOS 上只能跑 mock-based 单元测试；真实 `es.exe` 行为已在 Windows 11 + Everything 已安装环境验证
  （`tests/real_everything.rs`，默认 `#[ignore]`，含扩展名搜索 + CJK 文件名 + 同义词 OR 端到端）

详细设计与合规要求见 [docs/local-personal-search-agent-project-plan.md §6.4](../../../docs/local-personal-search-agent-project-plan.md) 和 [docs/Scout知识产权保护计划书.md §7.2](../../../docs/Scout知识产权保护计划书.md)。
