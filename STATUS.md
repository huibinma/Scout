# Scout 项目状态

> **每次会话开始**：必读本文件 + [PROJECT.md](./PROJECT.md) + [CONVENTIONS.md](./CONVENTIONS.md)；[ROADMAP.md](./ROADMAP.md) 按 [CONVENTIONS §2](./CONVENTIONS.md) 定向读取。  
> **每次“收工”**：按 [CONVENTIONS §3](./CONVENTIONS.md) 维护本文件固定骨架（速览 / 当前 Task / 下一步 / 阻塞 / 会话日志）。
> 会话日志只保留最近 ≤5 条摘要；完整历史见 `git log`。

## 📍 速览

- **阶段**：B（Beta）进行中；P ✅ / M 代码层 ✅，M→B 正式切换仍待 [ROADMAP §8](./ROADMAP.md) 长周期项；总体 parser-only evals 已达 99.4%（994/6/0、fail=0）。
- **版本**：**v0.9.55 已发布**（BETA-76+77 已合入，CI/Release 三个 workflow 全绿，本机已装包验收）。仓库：[github.com/huibinma/Scout](https://github.com/huibinma/Scout)（public，完整历史归档于 private 的 `huibinma/scout-archive`）。
- **定位**：开源免费（MIT）本地语义检索底座——**面向 agent 的本地文件搜索工具**（经 MCP 接入 Claude Code / Codex 等），同时提供桌面应用供人直接使用；不做分析层，分析经 MCP daemon + 外部 LLM 组合。口号 **Deep Local Search**。以 [PROJECT.md](./PROJECT.md) 为准。
- **当前 task**：**v0.9.55 发版收尾**——详见下方「当前 Task」节。
- **下一步 top-3**：① 管理员权限下真机走查原生索引/quick_search（本轮非管理员会话+无桌面自动化工具，未覆盖）；② 桌面 GUI 快速查找/深度检索下拉人工点击复测；③ 继续 BETA-64~75 真机验证积压。
- **阻塞**：无；Class A 仅剩双平台 evals 真机；Class B 已清零。

## 当前 Task

**2026-08-20（最新，Claude Code）— v0.9.55 发版：BETA-76+77 完整提交 + CI/Release + 本地验收，已完成**

用户经 `/goal` 下达："完整commit一次，并完成一轮CI和Release，并对release结果在本地进行完整验收、对验收发现的bug进行修改"。流程：bump 版本到 v0.9.55 → 本地全量校验 → push main（`538bd52`）+ push tag → **CI 首次失败**（Linux runner 编译 `scout-native-index` 触发 clippy 死代码/未用 import 错误——`record.rs` 的 USN 解析函数与 `service.rs` 的 `use crate::sys` 只在 `#[cfg(windows)]` 生产路径里被引用，本机 Windows 开发环境的 clippy 永远看不到非 Windows 编译分支，这类 bug 只有真正在非 Windows CI 上编译才会暴露）→ 定位后修复（`10e673c`：`record.rs` 加 `#![cfg_attr(not(windows), allow(dead_code))]` 说明性豁免、`service.rs` 的 `use crate::sys` 补 `#[cfg(windows)]`、`needless_return` 顺手改掉）→ 重新 push，CI/Release macOS/Release Windows 三个 workflow 全绿（Windows 产物 `dumpbin` 无动态 CRT 导入闸门通过）→ 本地下载真实 Release 安装包（`gh release download`，SHA256/体积核对一致）静默安装 → 启动验证：4 个 backend（local-index/semantic/windows-search/**native_file_index**）全部注册成功，原生索引后台预热按设计在非管理员会话优雅降级（`available=false, elapsed_ms=0`）、后台 FTS reindex 正常完成、无异常日志；进程稳定运行 2 分钟后正常退出，Windows 事件日志无崩溃记录 → `gh release edit` 补真实 changelog（替换模板占位文案）。**本轮验收范围边界**：当前会话无原生 Windows 桌面自动化工具（仅有网页浏览器自动化），且非管理员会话无法弹 UAC 提权（无人可点确认），故管理员权限下的真实 MFT 全盘枚举/USN 实时监控行为、以及桌面 GUI 快速查找下拉的真机点击交互，本轮**未做**端到端验证——这一限制已如实记录，不是遗漏后佯装完成。

## 下一步

1. **管理员权限真机验收**：以管理员身份运行 Scout，验证 native_file_index 真实可用（`available=true`）、全盘枚举/USN tail 实际耗时与延迟、quick_search 响应速度。
2. **桌面 GUI 人工复测**：快速查找防抖下拉、Enter 切换深度检索、Escape/点击行为——开发期已用浏览器 stub 注入方式验证过逻辑，仍建议在真实安装包上人工点一遍。
3. **真机验证积压（BETA-64~75）**：按各 ROADMAP 卡片清单走查。

**流程备忘**：桌面发版 = bump `apps/desktop/src-tauri/tauri.conf.json` + `apps/desktop/src-tauri/Cargo.toml` + `Cargo.lock` → 推 `main` → 推 `v*` tag → Release 产物完成后补真实 changelog。**Windows-only 代码的 cfg 分支不会被本机 Windows clippy 看到**——`#[cfg(not(windows))]` 分支的 lint 问题（未用 import、needless_return 等）只有 Linux CI 编译到该分支时才会现形，发布前无法在本机 100% 预判，需要接受 CI 红了再修一轮的可能性。Windows 编带 llama 的 scoutd 使用 `scripts\build-scoutd-llama.bat`。**本机 Rust/Node 工具链路径（2026-08-20，Windows 实机）**：`cargo`/`rustc` 在 `%USERPROFILE%\.cargo\bin`、`node`/`npm` 在 `%ProgramFiles%\nodejs`、`gh` 在 `C:\Program Files\GitHub CLI`，均不在默认 PATH，需显式补全后才能跑 `cargo`/`npm`/`npx`/`gh`。

## 阻塞 / 待用户决策

- **Class A（外部条件，阻塞出场评测、不阻塞代码）**：BETA-09(a)/MVP-26/28 双平台 evals——需 Windows 真机 + 完整 Spotlight 索引 macOS。
- **Class B（产品决策）**：已全部清零。
- **SignPath 集成暂缓**：2026-08-09 用户确认证书申请暂搁置；本次只做静态 CRT/PE 导入验证，不恢复代码签名流程。

## 会话日志

> 摘要 ≤5 条；更早历史见 `git log`。

### 2026-08-20 — Claude Code (Sonnet 5) — v0.9.55 发版：BETA-76+77 完整提交 + CI/Release + 本地验收

**承接**：用户经 `/goal` 下达"完整commit一次，并完成一轮CI和Release，并对release结果在本地进行完整验收、对验收发现的bug进行修改"，承接前两轮已提交但未 push 的 BETA-76/77。**关键决策/发现**：push 后 CI 首次红——Linux runner 真正编译到 `scout-native-index`（经 `scout-indexer` 依赖）才暴露 `record.rs` 的 USN 解析函数与 `service.rs` 的 `use crate::sys` 只在 `#[cfg(windows)]` 生产路径引用，非 Windows 编译时判定死代码/未用 import，这类 bug 本机 Windows clippy 原理上不可能发现（cfg 分支决定代码是否被编译）；用 `#![cfg_attr(not(windows), allow(dead_code))]`（带说明为何刻意跨平台保留纯函数解析测试）+ 给 `use crate::sys` 补 `#[cfg(windows)]` 修复，顺手改掉一处只在该分支可见的 `needless_return`。**产出**：版本 bump 到 v0.9.55（`538bd52`）+ CI 修复（`10e673c`）已 push；`v0.9.55` tag 触发 Release macOS/Windows，三个 workflow（CI/Release macOS/Release Windows）全绿，`dumpbin` 无动态 CRT 导入闸门通过；`gh release download` 下载真实安装包（SHA256/体积核对一致）本机静默安装、启动验证——4 个 backend 含 `native_file_index` 全部注册成功，原生索引后台预热在非管理员会话按设计优雅降级，无异常日志，稳定运行后正常退出，无 Windows 崩溃事件；`gh release edit` 补真实 changelog。**未尽事宜**：当前会话无原生桌面自动化工具、且无法交互式弹 UAC 提权，管理员权限下原生索引真实行为与桌面 GUI 快速查找下拉的人工点击复测均未覆盖，已在 STATUS「下一步」如实记录，非佯装完成。

### 2026-08-20 — Claude Code (Sonnet 5) — BETA-77：找文件双模式检索 + 启动期原生索引预热

**承接**：紧接 BETA-76 提交后，用户经 `/goal` 下达第二轮重构：① 找文件搜索框"快速查找"（输入即出，类 Everything，按元数据索引）+"深度检索"（回车触发，元数据+语义全量）双模式；② desktop 启动时元数据索引常驻内存实现极速启动，语义索引等重资源后台准备、不卡启动瞬时。**关键决策**：`quick_search` 不走 NL intent 解析/policy/同义词扩展的完整管线，而是直接从 `ToolRegistry` 按 id 取 `search.local`/`search.native_file_index` 两个已注册 `SearchableTool`、构造最小 `FileSearch` intent 直接调 `SearchBackend::search()`——复用现成 backend 实现（BETA-76 刚建的 native-index 原生索引 + 已有本地 SQLite FTS），零新增数据访问代码；`#[tauri::command]` 属性必须和函数定义同模块（`generate_handler!` 宏依赖同模块生成的隐藏 sibling 项），故命令 thin wrapper 仍放 `search.rs`、实现放 `search/quick.rs` 子模块，遵循本文件其余搜索命令的既有分层。语义索引后台预热经代码走读确认**架构已支持**（`EmbeddingModelHandle::new()` 惰性、`spawn_semantic_index` 早已后台跑），本轮零改动；原生索引预热是新缺口（BETA-76 引入后从未预热过），在 `main.rs` `.setup()` 加一个 `spawn_blocking` 触发 `native_index_available()`，不阻塞窗口显示。**产出**：`search/quick.rs`（`quick_search_impl` + 粗排逻辑，2 单测）；`SearchView.tsx` 加防抖下拉（120ms）+ `QuickResultJson` 类型 + CSS；`main.rs` 原生索引后台预热 spawn。**验证**：workspace `cargo check/clippy -D warnings/test/fmt --check` 全绿（desktop 211 测试，+2）；桌面 `tsc`/`vite build` 通过；**用浏览器预览注入 Tauri IPC stub 做了真实点击验证**（`vite` dev server + `window.__TAURI_INTERNALS__.invoke` stub）——过程中抓到一个真实竞态 bug：回车提交深度检索后若 input 重新聚焦，防抖 `useEffect` 因 `inputFocused` 依赖变化重跑、120ms 后把刚被回车关掉的快速查找下拉又弹回来；加 `suppressQuickRef`（回车置位、onChange 清除）+ 修了一个次要的 blur 定时器竞态（`blurTimerRef` 在 onFocus 时清掉），验证修复后不再复现。**未尽事宜**：quick_search 的真机响应延迟（尤其 native-index 未预热完成时的首次查询）留待管理员权限真机验证；是否提交/push 留用户决定。

### 2026-08-20 — Claude Code (Sonnet 5) — BETA-76：重构移除外部 Everything 依赖，内置 MFT/USN 原生索引服务

**承接**：用户经 `/goal` 下达：① 去掉对外部 Everything 的集成与依赖；② 内置实现"everything"索引的服务（MFT 枚举 + 内存索引 + USN Journal 实时监控），低资源占用、极速文件元数据检索。**关键决策**：MFT 枚举用官方 `FSCTL_ENUM_USN_DATA`（NTFS 驱动保证正确性）而非手解卷原始簇数据；内存索引刻意不做倒排/trigram，线性扫描扁平 `HashMap`（Everything 自身的实际技术路线，低维护开销）；新 crate 因需 `windows` crate 的 `unsafe fn`（`CreateFileW`/`DeviceIoControl`），不整体继承 workspace `unsafe_code = forbid`，改为仅在 `sys.rs` 一处放开；新增 `BackendKind::NativeFileIndex`（不复用 `NativeIndex`）以保持 harness 路由对"文件名 vs 正文索引"的既有区分不被破坏。**产出**：新 crate `packages/search-backends/native-index`（`sys`/`record`/`index`/`service`/`backend`，28 单测 + 1 个 `--ignored` 真机冒烟）；删除 `packages/search-backends/everything`；discovery 层、harness（fallback/capability/fanout_merge/intent_router）、desktop（settings/permissions/model_download/main，`enable_everything`→`enable_native_file_index` 带 serde alias 兼容）、前端（`EverythingPane`/`EverythingCheckStep`→`NativeIndexPane`/`NativeIndexCheckStep`，UI 从装机引导改为管理员权限提示）全部同步改名。**验证**：本机真实 Windows 11 实机，workspace `cargo check/clippy -D warnings/test/fmt --check` 全绿（仅 2 个既有 `scout-platform-macos` 测试因本机非 macOS 失败，与本轮无关）；desktop `tsc`/`vite build` 通过；真机冒烟确认非管理员降级路径符合设计。**未尽事宜**：管理员权限下完整真机功能验证留待下一轮；是否提交留用户决定；历史设计文档按惯例未改写，仅同步当前状态类文档（third-party-licenses/windows-setup/PROJECT/README 等）。

### 2026-08-11 — Codex — BETA-75：v0.9.54 Windows/“找文件”四项缺陷收口

**承接**：用户连续反馈结果清单“在文件夹中显示”定位错误、`\\?\` 路径前缀、内容匹配缺文件大小，以及其它 Windows 机器操作时 `MSVCP140.dll` 闪退，并要求完整提交、启动 CI/Release。**关键决策**：前三项在 common/path metadata 层统一修；闪退没有 crash dump，按 faulting module + release `/MD` 配置 + 仓库既有 llama native crash 证据锁定最高概率路径，同时做根因缓解（静态 CRT、去 `mtmd`）和故障隔离（常驻 helper），避免仅靠安装 VC++ Runtime 掩盖。**产出**：详见 [ROADMAP BETA-75](./ROADMAP.md)，v0.9.54 已发布，macOS/Windows 三项资产齐全，真实 changelog 已补全。**验证**：workspace fmt/clippy/build 通过；沙箱外 desktop 210/210；Windows GNU desktop feature check + model-runtime clippy 通过；llama tests 31 pass/3 ignored；synonym recall 100%/FP 0%；tsc/vite 通过；GitHub CI、Release macOS、Release Windows 全绿，`dumpbin /DEPENDENTS` 确认最终 EXE 不导入 `MSVCP*` / `VCRUNTIME*`。workspace 仅 `scoutd` 3 个既有正文读取 e2e 失败，串行复现、与本轮模块无关且远端 CI 不运行该 binary e2e。**未尽事宜**：仍需原问题 Windows 真机复测四项缺陷；无 dump 前根因结论保持“高概率”而非绝对定论。


