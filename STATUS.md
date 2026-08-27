# Scout 项目状态

> **每次会话开始**：必读本文件 + [PROJECT.md](./PROJECT.md) + [CONVENTIONS.md](./CONVENTIONS.md)；[ROADMAP.md](./ROADMAP.md) 按 [CONVENTIONS §2](./CONVENTIONS.md) 定向读取。  
> **每次“收工”**：按 [CONVENTIONS §3](./CONVENTIONS.md) 维护本文件固定骨架（速览 / 当前 Task / 下一步 / 阻塞 / 会话日志）。
> 会话日志只保留最近 ≤5 条摘要；完整历史见 `git log`。

## 📍 速览

- **阶段**：B（Beta）进行中；P ✅ / M 代码层 ✅，M→B 正式切换仍待 [ROADMAP §8](./ROADMAP.md) 长周期项；总体 parser-only evals 已达 99.4%（994/6/0、fail=0）。
- **版本**：**v0.9.60 已发布**（CI/Release macOS/Release Windows 三个 workflow 全绿，`gh release edit` 已补真实 changelog）——BETA-82 三路评审修复，待用户真机验证。仓库：[github.com/huibinma/Scout](https://github.com/huibinma/Scout)（public，完整历史归档于 private 的 `huibinma/scout-archive`）。
- **定位**：开源免费（MIT）本地语义检索底座——**面向 agent 的本地文件搜索工具**（经 MCP 接入 Claude Code / Codex 等），同时提供桌面应用供人直接使用；不做分析层，分析经 MCP daemon + 外部 LLM 组合。口号 **Deep Local Search**。以 [PROJECT.md](./PROJECT.md) 为准。
- **当前 task**：**BETA-82 已完成并随 v0.9.60 发布，待用户真机验证**——详见下方「当前 Task」节。
- **下一步 top-3**：① 用户真机验证 v0.9.60（`Scoutd` 崩溃自愈、物理卷更换后索引重建、设置页检测文案不再误报"需管理员权限"）；② connection.json 明文 token 的 ACL 加固（仍未修，需真机多用户/提权环境）；③ 继续 BETA-64~75 真机验证积压。
- **阻塞**：无；Class A 仅剩双平台 evals 真机 + v0.9.60 改动的真机验证（服务崩溃自愈、卷更换后索引重建）。

## 当前 Task

**2026-08-27（最新，Claude Code (Sonnet 5)）— BETA-82：三路深度评审修复，v0.9.60 已发布**

用户经 `/goal` 驱动三路深度评审（对照 Everything 完整性 / scoutd 启动阻塞 / Desktop UI 梳理），并行派发三个子会话独立评审+验证，本会话逐项复核 diff 并独立重跑 build/clippy/fmt/test 确认无回归后收口，均属真实缺陷非文案问题：**native-index** 修复物理卷更换（如 U 盘拔出后另一设备复用同一盘符）后陈旧缓存不失效（核对 `GetVolumeInformationW` 卷序列号）；**scoutd** 新增 SCM 失败恢复策略（崩溃后 10s/30s 自动重启，此前完全未配置）+ 修复恒报成功 exit code 给 SCM 的问题 + `load_embedder` 补齐 `spawn_blocking`；**Desktop UI** 修复「内置原生索引可用性检测」仍探测桌面进程自身管理员权限（BETA-78 前的过期判据）导致长期误报"不可用"的核心 bug，改用真正反映 scoutd 连接态的信号源，并按用户反馈精简顶栏「本机服务」面板（去掉与其余三项冗余的 NativeFileIndex 一行）。详见 [ROADMAP BETA-82](./ROADMAP.md)。**发布**：bump 到 v0.9.60 → push main + tag → CI/Release macOS/Release Windows 三个 workflow 全绿 → `gh release edit` 补真实 changelog。**未尽事宜**：connection.json ACL、journal 失效自动重建、桌面-scoutd 连接状态专门 UI、NSIS 安装失败提示——均已记录未修；本会话无管理员权限，无法真机验证服务崩溃自愈/卷更换重建的端到端效果。

## 下一步

1. **v0.9.60 真机验证**：`Scoutd` 服务崩溃后能否按新恢复策略自动重启；物理卷更换（拔插 U 盘复用盘符）后内置原生索引是否正确重建而非返回旧卷陈旧结果；设置页「内置原生索引」检测是否正确反映 scoutd 连接态、不再误报"需管理员权限"。
2. **connection.json ACL 加固**（BETA-79 评审发现，仍未修）：明文 admin token 当前继承 `%ProgramData%` 默认 ACL，本机任意标准用户可读；正确修法需按装机时的交互用户 SID 精确授权（简单粗暴的"仅 SYSTEM+Administrators"方案会因 UAC token 过滤反而连桌面客户端自己都读不到，已验证过不可行），需要真实多用户/提权环境验证。
3. **BETA-82 记录的后续项**：journal 失效自动重建（native-index）、桌面-scoutd 连接状态专门 UI（区分"启动中"与"确实挂了"）、NSIS 安装失败用户可见提示。
4. **BETA-78 后续任务**：本地 reindex 循环 / `mcp_service.rs` / 设置页 roots 编辑迁移到调用 scoutd；desktop 原生窗口人工点击复测。
5. **真机验证积压（BETA-64~75）**：按各 ROADMAP 卡片清单走查。

**流程备忘**：桌面发版 = bump `apps/desktop/src-tauri/tauri.conf.json` + `apps/desktop/src-tauri/Cargo.toml` + `Cargo.lock` → 推 `main` → 推 `v*` tag → Release 产物完成后补真实 changelog。**Windows-only 代码的 cfg 分支不会被本机 Windows clippy 看到**——`#[cfg(not(windows))]` 分支的 lint 问题只有 Linux CI 编译到该分支时才会现形。Windows 编带 llama 的 scoutd 用 `scripts\build-scoutd-llama.bat`（本机开发态）；CI release-windows.yml 现在也会编一份带 llama-cpp 的 scoutd.exe 打进桌面安装包（BETA-78）。**本机 Rust/Node 工具链路径（2026-08-20，Windows 实机）**：`cargo`/`rustc` 在 `%USERPROFILE%\.cargo\bin`、`node`/`npm` 在 `%ProgramFiles%\nodejs`、`gh` 在 `C:\Program Files\GitHub CLI`，均不在默认 PATH，需显式补全后才能跑 `cargo`/`npm`/`npx`/`gh`。

## 阻塞 / 待用户决策

- **Class A（外部条件，阻塞出场评测、不阻塞代码）**：BETA-09(a)/MVP-26/28 双平台 evals——需 Windows 真机 + 完整 Spotlight 索引 macOS；**BETA-78 管理员权限/真实安装包验证**——需管理员权限会话或用户自行手测。
- **Class B（产品决策）**：已全部清零。
- **SignPath 集成暂缓**：2026-08-09 用户确认证书申请暂搁置；本次只做静态 CRT/PE 导入验证，不恢复代码签名流程。

## 会话日志

> 摘要 ≤5 条；更早历史见 `git log`。

### 2026-08-25 — Claude Code (Sonnet 5) — BETA-82：三路深度评审（对照 Everything / scoutd 启动 / Desktop UI）

**承接**：用户经 `/goal` 下达"做3个深度Review和优化"，覆盖对照 Everything 的完整性审计、scoutd 服务启动阻塞排查、Desktop UI 梳理。**方法**：三方向并行派发子会话独立评审+验证，本会话逐项复核 diff 并独立重跑 `fmt`/`clippy -D warnings`/`check`/`test` 确认无回归。**产出**（均属真实缺陷非文案问题，详见 [ROADMAP BETA-82](./ROADMAP.md)）：native-index 修复物理卷更换后陈旧缓存不失效；scoutd 新增 SCM 失败恢复策略（崩溃后 10s/30s 自动重启）+ 修复恒报成功 exit code；Desktop 修复"内置原生索引可用性检测"仍探测桌面进程自身管理员权限（BETA-78 前的过期判据）导致长期误导用户的核心 bug。**未尽事宜**：connection.json ACL、journal 失效自动重建、连接状态专门 UI、NSIS 失败提示——均记录未做；本会话改动尚未 bump 版本/提交，待用户确认。

### 2026-08-21 — Claude Code (Sonnet 5) — BETA-81：USN tail 线程错误分类 + 可观测性

**承接**：v0.9.59 已发布、等待用户真机验证 BETA-80 修复期间，鉴于 `/goal` 原文"彻底检查Windows Scout Service的问题"覆盖范围不止服务启动本身，顺带处理 BETA-79 评审已记录但未修的另一项发现：`spawn_tail_worker`（`packages/search-backends/native-index/src/service.rs`）遇任意 `read_usn_journal` 错误一律永久 `break` 且零日志，journal 真失效（不可恢复）与瞬时 I/O 抖动（该重试）被同等对待。**修复**：`NativeIndexError` 新增 `JournalInvalidated` 变体；`sys::read_usn_journal` 新增 `is_journal_invalidated` 分类函数，精确识别 `ERROR_JOURNAL_DELETE_IN_PROGRESS`/`ERROR_JOURNAL_NOT_ACTIVE`/`ERROR_JOURNAL_ENTRY_DELETED` 三个 Win32 错误码；tail 线程据此分流——journal 失效直接 `tracing::error!` 后停止（语义不变，只是从静默变可观测），其余错误退避重试、连续失败 5 次才放弃并记终态日志。新增 `tracing` 依赖到该 crate。**验证**：`is_journal_invalidated` 用 `windows::core::Error::from_hresult` 构造合成错误码单测覆盖，不需要真实 USN Journal 环境即可在本机（真实 Windows 11，非交叉编译）跑通；`scout-native-index` 30→31 单测；workspace `check/clippy -D warnings/fmt --check/test` 全绿。详见 [ROADMAP BETA-81](./ROADMAP.md)。**未尽事宜**：退避重试上限/判定边界为保守默认值，未在真实抖动场景校准；放弃 tail 后不会自动触发全量重建，仍需手动重启服务。

### 2026-08-21 — Claude Code (Sonnet 5) — BETA-80：修复 v0.9.58 真机反馈的 Scoutd 服务启动失败

**承接**：用户经 `/goal` 下达真机反馈——v0.9.58 装机后 `Scoutd` 服务已正确注册但启动失败，手动拉起也失败，Windows 事件查看器中无具体错误信息，要求彻底排查并确保装后能自启动。**发现**：会话开始 `git status` 时注意到 `apps/daemon/src/{main.rs,service.rs,personal.rs}` 已有未提交的本地改动——推测是上一轮会话诊断到一半、未完成验证与提交就结束的遗留工作，其诊断结论与本次症状完全吻合：v0.9.58 里 `build_runtime_ctx` 对 `load_embedder(&config.model_path)` 用硬 `?`，个人模式首次启动需下载约 300MB embedding 模型，`LocalSystem` 服务账户网络路径（代理/DNS/防火墙出站策略）常与交互用户会话不同、下载更易失败，一旦失败即终止整个 `service_main`；而 service 模式当时的 tracing 只写 stdout——Windows Service 进程无 console，真实报错完全丢失，只剩"启动失败、事件查看器无信息"。**本轮完成**：① 补齐验证遗留改动——`init_tracing_to_file`（`<data_dir>\scoutd.log` 按日滚动）+ `install_panic_log_hook`（panic 落盘）+ `load_embedder` 失败降级为 `UnavailableEmbedder`（FTS-only，非终止）+ `ensure_embedding_model` 增加 `hf-mirror.com` 镜像兜底；② 新增：`run_dispatcher`（`service.rs`）里 SCM 握手失败分支补 `tracing::error!`，此前该分支的错误只会交给 `main()` 默认打到 stderr，对服务模式同样无意义；③ 修复遗留代码里 3 处 `clippy -D warnings` 违规（`doc_markdown` 缺反引号 ×2、`unnecessary_literal_bound`）——这正是此前未提交的原因之一。**验证**：`cargo build/test -p scoutd` 全绿；workspace `cargo check/clippy --all-targets -D warnings/fmt --all --check/test --workspace` 全绿（仅 `scout-platform-macos` 2 个既有失败，`git stash` 确认干净 HEAD 上同样失败、与本轮无关，是本机 Windows 而非 macOS 运行导致的路径分隔符断言问题）。bump 到 v0.9.59；用户确认后 push main + tag，CI/Release macOS/Release Windows 三个 workflow 全绿，`gh release edit` 已补真实 changelog（含根因说明与 `scoutd.log` 排查指引）。**未尽事宜**：本会话无管理员权限，无法本机真实注册/拉起 Windows Service 复现原始故障或验证修复（创建/启动系统服务超出本环境可执行范围）；仍需用户重装 v0.9.59 后真机验证，若仍失败请提供 `scoutd.log` 内容以便继续诊断。

### 2026-08-20 — Claude Code (Sonnet 5) — BETA-79：全面评审 native-index/scoutd 重构对照 Everything

**承接**：用户经 `/goal` 下达全面评审要求，对照 Everything 公开功能/关键技术实现逐项核对 BETA-76~78 是否构成完整替换。**方法**：WebSearch/WebFetch 核实 voidtools 官方 FAQ/searching 文档（而非仅凭训练知识），逐项比对现有代码。**结论**：核心索引机制（MFT 批量枚举+USN Journal tail）与 ReFS 不支持均确认对等（Everything 自身也不支持 ReFS）；评审开始时判断的"最大缺口"——桌面进程本身需要管理员权限——核对后发现 BETA-78 已解决（scoutd LocalSystem service + 桌面非管理员经 token 连接，对齐 Everything Service 免 UAC 模式）；查询语法（通配符/正则/布尔 NOT）判定为跨 backend 抽象的设计取舍，非缺口。**修复的真实 bug**：`MemIndex::full_path`（`packages/search-backends/native-index/src/index.rs`）祖先链断裂时静默拼出看似合法实则错误的路径（如误报到卷根），而非返回"未找到"——修复为断链一律 `None`，新增回归测试，环状防御性熔断分支保持不变（不破坏既有测试契约）。**评审中发现但审慎未修的两项**：connection.json 明文 token 的 ACL 加固（验证过"仅 SYSTEM+Administrators"方案会因 UAC token 过滤反而打断桌面客户端自己的读取，需按装机用户 SID 精确授权，需真机多用户环境验证）；USN tail 线程遇任意错误永久停止且无日志（需错误分类+可观测性）——均判断为"记录待跟进优于无法验证的仓促修复"，非疏漏。**验证**：workspace `cargo check/clippy -D warnings/test/fmt --check` 全绿（native-index 30 单测）。

