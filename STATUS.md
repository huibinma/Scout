# Scout 项目状态

> **每次会话开始**：必读本文件 + [PROJECT.md](./PROJECT.md) + [CONVENTIONS.md](./CONVENTIONS.md)；[ROADMAP.md](./ROADMAP.md) 按 [CONVENTIONS §2](./CONVENTIONS.md) 定向读取。  
> **每次“收工”**：按 [CONVENTIONS §3](./CONVENTIONS.md) 维护本文件固定骨架（速览 / 当前 Task / 下一步 / 阻塞 / 会话日志）。
> 会话日志只保留最近 ≤5 条摘要；完整历史见 `git log`。

## 📍 速览

- **阶段**：B（Beta）进行中；P ✅ / M 代码层 ✅，M→B 正式切换仍待 [ROADMAP §8](./ROADMAP.md) 长周期项；总体 parser-only evals 已达 99.4%（994/6/0、fail=0）。
- **版本**：**v0.9.54 已发布**；本机（Windows 11 实机）另有 BETA-76（已提交，未 push）+ BETA-77（工作区未提交）两轮重构。仓库：[github.com/huibinma/Scout](https://github.com/huibinma/Scout)（public，完整历史归档于 private 的 `huibinma/scout-archive`）。
- **定位**：开源免费（MIT）本地语义检索底座——**面向 agent 的本地文件搜索工具**（经 MCP 接入 Claude Code / Codex 等），同时提供桌面应用供人直接使用；不做分析层，分析经 MCP daemon + 外部 LLM 组合。口号 **Deep Local Search**。以 [PROJECT.md](./PROJECT.md) 为准。
- **当前 task**：**BETA-77 重构：找文件搜索框"快速查找 + 深度检索"双模式 + 启动期原生索引后台预热**——详见下方「当前 Task」节。
- **下一步 top-3**：① 用户决定是否提交 BETA-77 改动、是否 push BETA-76+77；② 管理员权限下真机验证 quick_search 实际响应延迟与结果质量；③ 继续 BETA-64~75 真机验证积压。
- **阻塞**：BETA-77 改动待用户决定是否提交；Class A 仅剩双平台 evals 真机；Class B 已清零。

## 当前 Task

**2026-08-20（最新，Claude Code）— BETA-77：找文件双模式检索 + 启动期原生索引预热**

用户经 `/goal` 下达：① 找文件搜索框支持"快速查找"（输入即按元数据索引出结果，类 Everything）与"深度检索"（回车后元数据+语义全量检索）；② desktop 启动时元数据索引常驻内存、极速启动，语义索引等重资源后台准备不卡顿。新增 `apps/desktop/src-tauri/src/search/quick.rs`：`quick_search` 命令跳过 NL 解析/policy/同义词扩展，直接并发查 `search.local`/`search.native_file_index` 两个已注册 backend 的原始 `SearchBackend::search()`，合并去重后按匹配紧密度粗排。前端 `SearchView.tsx` 加 120ms 防抖下拉（`quick-results-dropdown`），回车切换深度检索并压制下拉误弹回（真机浏览器验证时抓到一个真实竞态：回车后若 input 重新聚焦，防抖 effect 会因 `inputFocused` 变化重跑、把刚关掉的下拉弹回来——加 `suppressQuickRef` 修复）。`main.rs` `.setup()` 里新增 native-index 后台 `spawn_blocking` 预热（不阻塞窗口显示），语义索引沿用已有的 `spawn_semantic_index` 后台管线（原架构已满足，未新增代码）。完整实现细节见 [ROADMAP BETA-77](./ROADMAP.md)。**本次未提交**，改动全部在工作区。

## 下一步

1. **BETA-77 是否提交**：用户决定——workspace `cargo check/clippy -D warnings/test/fmt --check` 与桌面 `tsc`/`vite build` 均已在本机 Windows 11 实机验证全绿；quick_search 下拉交互（防抖/Enter 压制/Escape/点击打开）已用浏览器预览注入 Tauri IPC stub 真实点击验证。
2. **BETA-76 是否 push**：已本地提交（`11cfd7e`），未推远程。
3. **quick_search 管理员权限真机验证**：实际输入延迟感受、native-index 未预热完成时首次查询是否有感知卡顿。
4. **v0.9.54 真机回归**：Release、DMG、NSIS、changelog 与 Windows PE 闸门均已收口；Windows 上逐项复测 BETA-75。
5. **真机验证积压（BETA-64~75）**：按各 ROADMAP 卡片清单走查。

**流程备忘**：桌面发版 = bump `apps/desktop/src-tauri/tauri.conf.json` + `apps/desktop/src-tauri/Cargo.toml` + `Cargo.lock` → 推 `main` → 推 `v*` tag → Release 产物完成后补真实 changelog。Windows 编带 llama 的 scoutd 使用 `scripts\build-scoutd-llama.bat`。**本机 Rust/Node 工具链路径（2026-08-20，Windows 实机）**：`cargo`/`rustc` 在 `%USERPROFILE%\.cargo\bin`、`node`/`npm` 在 `%ProgramFiles%\nodejs`，均不在默认 PATH，需显式补全后才能跑 `cargo`/`npm`/`npx`。

## 阻塞 / 待用户决策

- **BETA-77 提交与否 / BETA-76+77 push 与否**：均已完成并验证，留待用户决定——按 git 安全协议，提交/push 需用户明确要求。
- **Class A（外部条件，阻塞出场评测、不阻塞代码）**：BETA-09(a)/MVP-26/28 双平台 evals——需 Windows 真机 + 完整 Spotlight 索引 macOS。
- **Class B（产品决策）**：已全部清零。
- **SignPath 集成暂缓**：2026-08-09 用户确认证书申请暂搁置；本次只做静态 CRT/PE 导入验证，不恢复代码签名流程。

## 会话日志

> 摘要 ≤5 条；更早历史见 `git log`。

### 2026-08-20 — Claude Code (Sonnet 5) — BETA-77：找文件双模式检索 + 启动期原生索引预热

**承接**：紧接 BETA-76 提交后，用户经 `/goal` 下达第二轮重构：① 找文件搜索框"快速查找"（输入即出，类 Everything，按元数据索引）+"深度检索"（回车触发，元数据+语义全量）双模式；② desktop 启动时元数据索引常驻内存实现极速启动，语义索引等重资源后台准备、不卡启动瞬时。**关键决策**：`quick_search` 不走 NL intent 解析/policy/同义词扩展的完整管线，而是直接从 `ToolRegistry` 按 id 取 `search.local`/`search.native_file_index` 两个已注册 `SearchableTool`、构造最小 `FileSearch` intent 直接调 `SearchBackend::search()`——复用现成 backend 实现（BETA-76 刚建的 native-index 原生索引 + 已有本地 SQLite FTS），零新增数据访问代码；`#[tauri::command]` 属性必须和函数定义同模块（`generate_handler!` 宏依赖同模块生成的隐藏 sibling 项），故命令 thin wrapper 仍放 `search.rs`、实现放 `search/quick.rs` 子模块，遵循本文件其余搜索命令的既有分层。语义索引后台预热经代码走读确认**架构已支持**（`EmbeddingModelHandle::new()` 惰性、`spawn_semantic_index` 早已后台跑），本轮零改动；原生索引预热是新缺口（BETA-76 引入后从未预热过），在 `main.rs` `.setup()` 加一个 `spawn_blocking` 触发 `native_index_available()`，不阻塞窗口显示。**产出**：`search/quick.rs`（`quick_search_impl` + 粗排逻辑，2 单测）；`SearchView.tsx` 加防抖下拉（120ms）+ `QuickResultJson` 类型 + CSS；`main.rs` 原生索引后台预热 spawn。**验证**：workspace `cargo check/clippy -D warnings/test/fmt --check` 全绿（desktop 211 测试，+2）；桌面 `tsc`/`vite build` 通过；**用浏览器预览注入 Tauri IPC stub 做了真实点击验证**（`vite` dev server + `window.__TAURI_INTERNALS__.invoke` stub）——过程中抓到一个真实竞态 bug：回车提交深度检索后若 input 重新聚焦，防抖 `useEffect` 因 `inputFocused` 依赖变化重跑、120ms 后把刚被回车关掉的快速查找下拉又弹回来；加 `suppressQuickRef`（回车置位、onChange 清除）+ 修了一个次要的 blur 定时器竞态（`blurTimerRef` 在 onFocus 时清掉），验证修复后不再复现。**未尽事宜**：quick_search 的真机响应延迟（尤其 native-index 未预热完成时的首次查询）留待管理员权限真机验证；是否提交/push 留用户决定。

### 2026-08-20 — Claude Code (Sonnet 5) — BETA-76：重构移除外部 Everything 依赖，内置 MFT/USN 原生索引服务

**承接**：用户经 `/goal` 下达：① 去掉对外部 Everything 的集成与依赖；② 内置实现"everything"索引的服务（MFT 枚举 + 内存索引 + USN Journal 实时监控），低资源占用、极速文件元数据检索。**关键决策**：MFT 枚举用官方 `FSCTL_ENUM_USN_DATA`（NTFS 驱动保证正确性）而非手解卷原始簇数据；内存索引刻意不做倒排/trigram，线性扫描扁平 `HashMap`（Everything 自身的实际技术路线，低维护开销）；新 crate 因需 `windows` crate 的 `unsafe fn`（`CreateFileW`/`DeviceIoControl`），不整体继承 workspace `unsafe_code = forbid`，改为仅在 `sys.rs` 一处放开；新增 `BackendKind::NativeFileIndex`（不复用 `NativeIndex`）以保持 harness 路由对"文件名 vs 正文索引"的既有区分不被破坏。**产出**：新 crate `packages/search-backends/native-index`（`sys`/`record`/`index`/`service`/`backend`，28 单测 + 1 个 `--ignored` 真机冒烟）；删除 `packages/search-backends/everything`；discovery 层、harness（fallback/capability/fanout_merge/intent_router）、desktop（settings/permissions/model_download/main，`enable_everything`→`enable_native_file_index` 带 serde alias 兼容）、前端（`EverythingPane`/`EverythingCheckStep`→`NativeIndexPane`/`NativeIndexCheckStep`，UI 从装机引导改为管理员权限提示）全部同步改名。**验证**：本机真实 Windows 11 实机，workspace `cargo check/clippy -D warnings/test/fmt --check` 全绿（仅 2 个既有 `scout-platform-macos` 测试因本机非 macOS 失败，与本轮无关）；desktop `tsc`/`vite build` 通过；真机冒烟确认非管理员降级路径符合设计。**未尽事宜**：管理员权限下完整真机功能验证留待下一轮；是否提交留用户决定；历史设计文档按惯例未改写，仅同步当前状态类文档（third-party-licenses/windows-setup/PROJECT/README 等）。

### 2026-08-11 — Codex — BETA-75：v0.9.54 Windows/“找文件”四项缺陷收口

**承接**：用户连续反馈结果清单“在文件夹中显示”定位错误、`\\?\` 路径前缀、内容匹配缺文件大小，以及其它 Windows 机器操作时 `MSVCP140.dll` 闪退，并要求完整提交、启动 CI/Release。**关键决策**：前三项在 common/path metadata 层统一修；闪退没有 crash dump，按 faulting module + release `/MD` 配置 + 仓库既有 llama native crash 证据锁定最高概率路径，同时做根因缓解（静态 CRT、去 `mtmd`）和故障隔离（常驻 helper），避免仅靠安装 VC++ Runtime 掩盖。**产出**：详见 [ROADMAP BETA-75](./ROADMAP.md)，v0.9.54 已发布，macOS/Windows 三项资产齐全，真实 changelog 已补全。**验证**：workspace fmt/clippy/build 通过；沙箱外 desktop 210/210；Windows GNU desktop feature check + model-runtime clippy 通过；llama tests 31 pass/3 ignored；synonym recall 100%/FP 0%；tsc/vite 通过；GitHub CI、Release macOS、Release Windows 全绿，`dumpbin /DEPENDENTS` 确认最终 EXE 不导入 `MSVCP*` / `VCRUNTIME*`。workspace 仅 `scoutd` 3 个既有正文读取 e2e 失败，串行复现、与本轮模块无关且远端 CI 不运行该 binary e2e。**未尽事宜**：仍需原问题 Windows 真机复测四项缺陷；无 dump 前根因结论保持“高概率”而非绝对定论。

### 2026-08-09 — Claude Code (Sonnet 5) — BETA-74：桌面自动更新（提前实现 V10-04），发布 v0.9.50

**承接**：用户要求给桌面端做自动更新——定期检查 GitHub 新 Release、左下角提醒、点更新后台下载静默安装、保留配置数据 MCP token、装完自动重启；随后追加要求把「自动更新」「轮询间隔」做成设置项（默认开 + 4 小时，允许关闭 + 30 分钟~24 小时可调，原始需求是 8 小时后改 4 小时）。**关键决策**：技术方案用 AskUserQuestion 向用户核实后选「轻量自研」而非 `tauri-plugin-updater`——后者需生成新签名密钥对存 GitHub secret、且要改两个 Release workflow 生成合并 `latest.json`，两个 workflow 都标 `prerelease: true` 导致 GitHub `/releases/latest` 别名不可用还得另建固定 tag 托管 manifest，工作量和对发布流水线的改动明显更大；轻量方案直接调 GitHub Releases API + 下载既有安装包静默装，不碰 CI、不需要签名密钥。走读代码发现 `nsis/uninstall-hooks.nsh` 本就有 `$UpdateMode` 守卫，静默重装本就是官方支持的原地升级路径，settings.json/index.db/models/MCP token 全部自动保留，不需要自己另写保留逻辑。**产出**：新增 `update.rs`（镜像 `model_download.rs` 既有约定：reqwest stream 下载 + 进度 event + in-flight 守卫）+ `UpdateToast.tsx`/`useAutoUpdate.ts`（左下角四态 toast）；`settings.rs` 新增 `auto_update_enabled`/`auto_update_interval_minutes`（默认开 + 240 分钟，读取 clamp [30,1440]）+ `GeneralPane.tsx` 新增开关与间隔下拉，联动禁用。bump v0.9.50，push + tag，CI/Release macOS/Release Windows 三个 workflow 全部成功，`gh release edit` 补全真实 changelog。**插曲**：release 进行中用户提出"SignPath 签名暂时搁置，disable 掉 release workflow 里的签名 action"——排查发现 SignPath 集成从未提交/推送（只是 working tree 里一份未 commit 的 120 行 diff，此前 STATUS「下一步」条目已记录留给用户），实际跑在 CI 上的 `release-windows.yml` 本就是未签名版本（committed HEAD 从未含 SignPath 引用），无需任何改动，已向用户说明并保持原状不动。**验证**：Rust 新增 12 个单测全绿（版本比较/资产平台匹配/mock GitHub 响应解析/settings 默认值与 clamp 边界），`cargo test -p scout-desktop` 211 全绿，`clippy -D warnings`/`fmt --check` 在 macOS 与 `--target x86_64-pc-windows-gnu` 两目标均净；`tsc`/`vite build` 全绿；浏览器预览注入 `window.__TAURI_INTERNALS__` invoke/事件 stub，逐一截图验证左下角提醒四态定位与交互、设置页开关联动禁用、`update_settings` 保存回传正确值；针对真实 `api.github.com/repos/huibinma/Scout/releases` 拉取核对资产命名与匹配规则一致。**未尽事宜**：自动更新「发现新版本」真实路径未做端到端真机验证（需下一个真实版本发布后用旧版本装包测）；未加「手动检查更新」按钮、也未做失败时的权限提升重试，均超出用户原始需求范围、保持最小实现；SignPath 集成仍保持未提交搁置状态，等用户重新推进证书申请后再处理。


