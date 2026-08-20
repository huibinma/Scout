# Scout 项目状态

> **每次会话开始**：必读本文件 + [PROJECT.md](./PROJECT.md) + [CONVENTIONS.md](./CONVENTIONS.md)；[ROADMAP.md](./ROADMAP.md) 按 [CONVENTIONS §2](./CONVENTIONS.md) 定向读取。  
> **每次“收工”**：按 [CONVENTIONS §3](./CONVENTIONS.md) 维护本文件固定骨架（速览 / 当前 Task / 下一步 / 阻塞 / 会话日志）。
> 会话日志只保留最近 ≤5 条摘要；完整历史见 `git log`。

## 📍 速览

- **阶段**：B（Beta）进行中；P ✅ / M 代码层 ✅，M→B 正式切换仍待 [ROADMAP §8](./ROADMAP.md) 长周期项；总体 parser-only evals 已达 99.4%（994/6/0、fail=0）。
- **版本**：**v0.9.56 发版中**（BETA-78 服务化拆分）。用户经 `/goal` 明确要求"完整 commit + 跑一轮 CI 和 Release"——在管理员权限/真实 NSIS 安装包链路未经真机验证的已知缺口下按用户指示推进，验证责任转移到发布后的本地装包验收（与既往发版流程一致）。仓库：[github.com/huibinma/Scout](https://github.com/huibinma/Scout)（public，完整历史归档于 private 的 `huibinma/scout-archive`）。
- **定位**：开源免费（MIT）本地语义检索底座——**面向 agent 的本地文件搜索工具**（经 MCP 接入 Claude Code / Codex 等），同时提供桌面应用供人直接使用；不做分析层，分析经 MCP daemon + 外部 LLM 组合。口号 **Deep Local Search**。以 [PROJECT.md](./PROJECT.md) 为准。
- **当前 task**：**BETA-78 服务化拆分**——scoutd 新增 Windows Service 个人模式、桌面三个索引类 backend 改为远程代理，代码与本地 HTTP 集成已验证，真机安装/管理员权限/CI 验证未做。详见下方「当前 Task」节。
- **下一步 top-3**：① 管理员权限下真机走查 `--install-service`/真实 NSIS 安装包装机流程（本轮非管理员会话未覆盖）；② 桌面本地 reindex 循环 / `mcp_service.rs` / 设置页 roots 编辑迁移到调用 scoutd（BETA-78 明确延后的后续任务）；③ 继续 BETA-64~75 真机验证积压。
- **阻塞**：无；Class A 仅剩双平台 evals 真机 + BETA-78 真机装机验证。

## 当前 Task

**2026-08-20（最新，Claude Code）— BETA-78：Scout 拆分为后台 Windows Service（scoutd）+ 前端瘦客户端桌面，已完成**

用户经 `/goal` 下达："因为读取NTFS MFT依赖管理员权限，因此需要将Scout重构为一个后台service和一个前端desktop……安装时自动安装、配置好后台的service，并自动启动；scout desktop启动后，可以自动连接到后台service"。选择"本次一次性打通端到端"。详细改动内容/文件清单见 [ROADMAP BETA-78](./ROADMAP.md)。**核心结论**：代码层完整（`scoutd` Windows Service 个人模式 + `scout-server` 三个新端点 + 桌面 `RemoteSearchBackend`），workspace 335 个测试全绿，手动起真实 scoutd 前台实例 + `curl` 验证了 `/health`/`/admin/status`/`/search`/`/search/quick`/`/backend/search` 五端点端到端正确；但当前会话**无管理员权限、无法交互式弹 UAC**，`--install-service`/真实 Windows Service 注册/真实 NSIS 安装包这条链路**未经真机验证**，如实记录，下一轮需要真机走查。同时刻意保留桌面本地 reindex 循环未删（避免预览/OCR 片段功能出现"搜到但预览不到"的新 bug），`mcp_service.rs`/设置页 roots 编辑/reindex 命令均未迁移到调用 scoutd——这些是明确的后续任务。

## 下一步

1. **管理员权限真机验收 BETA-78**：`scoutd.exe --install-service` 真实注册 Windows Service（LocalSystem、AutoStart）+ SCM StartPending/Running 状态流转；真实 NSIS 安装包端到端（`installMode: perMachine` 触发 UAC、post-install/pre-uninstall 钩子真实调通）；`release-windows.yml` 新增的 scoutd 构建步骤跑一次真实 workflow。
2. **BETA-78 后续任务**（桌面侧尚未做但已在 ROADMAP 记录）：本地 reindex 循环 / `mcp_service.rs` / 设置页 roots 编辑迁移到调用 scoutd 的 `/admin/reindex`/`/admin/personal/roots`；desktop 原生窗口下的搜索 UI 人工点击复测（本环境仅浏览器自动化，测不到 Tauri 原生 IPC）。
3. **真机验证积压（BETA-64~75）**：按各 ROADMAP 卡片清单走查。

**流程备忘**：桌面发版 = bump `apps/desktop/src-tauri/tauri.conf.json` + `apps/desktop/src-tauri/Cargo.toml` + `Cargo.lock` → 推 `main` → 推 `v*` tag → Release 产物完成后补真实 changelog。**Windows-only 代码的 cfg 分支不会被本机 Windows clippy 看到**——`#[cfg(not(windows))]` 分支的 lint 问题只有 Linux CI 编译到该分支时才会现形。Windows 编带 llama 的 scoutd 用 `scripts\build-scoutd-llama.bat`（本机开发态）；CI release-windows.yml 现在也会编一份带 llama-cpp 的 scoutd.exe 打进桌面安装包（BETA-78）。**本机 Rust/Node 工具链路径（2026-08-20，Windows 实机）**：`cargo`/`rustc` 在 `%USERPROFILE%\.cargo\bin`、`node`/`npm` 在 `%ProgramFiles%\nodejs`、`gh` 在 `C:\Program Files\GitHub CLI`，均不在默认 PATH，需显式补全后才能跑 `cargo`/`npm`/`npx`/`gh`。

## 阻塞 / 待用户决策

- **Class A（外部条件，阻塞出场评测、不阻塞代码）**：BETA-09(a)/MVP-26/28 双平台 evals——需 Windows 真机 + 完整 Spotlight 索引 macOS；**BETA-78 管理员权限/真实安装包验证**——需管理员权限会话或用户自行手测。
- **Class B（产品决策）**：已全部清零。
- **SignPath 集成暂缓**：2026-08-09 用户确认证书申请暂搁置；本次只做静态 CRT/PE 导入验证，不恢复代码签名流程。

## 会话日志

> 摘要 ≤5 条；更早历史见 `git log`。

### 2026-08-20 — Claude Code (Sonnet 5) — BETA-78：Scout 拆分为后台 Windows Service + 前端瘦客户端桌面

**承接**：用户经 `/goal` 下达服务/桌面拆分需求（见上「当前 Task」），选择一次性打通端到端。**关键决策**：① 桌面 harness 管线（policy/refine/同义词/多类型均衡/tracer）里程碑式复杂，早期研究阶段误判过其"可整体丢弃改走远程 `/search`"，实际读全 `search.rs` 后发现代价太大——改为更小侵入的方案：只把 `search.local`/`search.semantic`/`search.native_file_index` 三个 `SearchBackend` 换成 `RemoteSearchBackend`（经新增 `POST /backend/search` 代理 `search_expanded()`），桌面其余管线零改动，产品体验零回归。② `scoutd` 新增 `windows-service` crate 支持，`Cli` 加可选子命令（`bootstrap-personal-config`/`install-service`/`uninstall-service`/`service`），不给子命令走今天的前台团队部署路径，零迁移。③ 语义相似度下限过滤原在桌面本地 `SemanticIndexBackend` 内部执行，backend 挪服务端后服务不知道桌面这个个性化设置，改为在 `RemoteSearchBackend` 拿到结果后本地 filter，行为不变。④ `/admin/personal/roots` 因 `CollectionRuntime.meta.roots` 无运行时热更新路径，只做落盘 + `restart_required` 标志，不做实时局部 reindex（避免半吊子不一致状态）。⑤ tauri.conf.json 不直接加 `scoutd.exe` 到 `bundle.resources`（会让所有人本地 `cargo check`/`tauri dev` 因文件不存在而报错），改为 CI release-windows.yml 用 `tauri build --config` 传内联 JSON 只在 CI 注入。**产出**：`apps/daemon/src/{personal,service}.rs`（新增）、`cli.rs`/`main.rs` 改造；`packages/scout-server` 新增 `quick_search.rs`/`search_http.rs`，`admin.rs`/`app.rs`/`collections.rs`/`tools/search.rs` 加 `/search`/`/search/quick`/`/backend/search`/`/admin/status`/`/admin/personal/roots`；`apps/desktop/src-tauri/src/service_client.rs`（新增）+ `main.rs` 改造；`nsis/uninstall-hooks.nsh` 改名 `hooks.nsh` 加 POSTINSTALL/PREUNINSTALL；`.github/workflows/release-windows.yml`/`apps/daemon/README.md` 同步。**验证**：workspace `cargo check/clippy -D warnings/fmt --check/test`（scoutd 25 + scout-server 99 + scout-desktop 211 = 335 测试）全绿；手动起真实 scoutd 前台实例（真实索引一份中英文测试语料）+ `curl` 验证 5 个端点端到端返回正确命中。**未尽事宜**：无管理员权限/无法弹 UAC，`--install-service`/真实 Windows Service/真实 NSIS 安装包链路未验证；桌面本地 reindex 循环/`mcp_service.rs`/设置页 roots 编辑/`reindex`/`reindex_root` 命令均未迁移到调用 scoutd（刻意延后，避免预览功能因索引不一致出新 bug）；CI 新增的 scoutd 构建步骤未跑过真实 workflow；无原生桌面自动化工具，Tauri 原生窗口下的搜索 UI 未做人工点击复测。

### 2026-08-20 — Claude Code (Sonnet 5) — v0.9.55 发版：BETA-76+77 完整提交 + CI/Release + 本地验收

**承接**：用户经 `/goal` 下达"完整commit一次，并完成一轮CI和Release，并对release结果在本地进行完整验收、对验收发现的bug进行修改"。流程：bump 版本到 v0.9.55 → 本地全量校验 → push main + tag → CI 首次失败（Linux runner 编译 `scout-native-index` 触发 clippy 死代码/未用 import——`#[cfg(windows)]` 生产路径代码本机 Windows clippy 永远看不到）→ 修复后 CI/Release macOS/Release Windows 三个 workflow 全绿 → 本地下载真实 Release 安装包验证：4 个 backend 全部注册成功，原生索引非管理员会话优雅降级，无异常日志 → 补真实 changelog。**本轮验收范围边界**：无原生桌面自动化工具、非管理员会话无法弹 UAC，管理员权限下的真实 MFT 全盘枚举/USN 实时监控行为、桌面 GUI 快速查找下拉的真机点击交互，本轮未做端到端验证。

### 2026-08-20 — Claude Code (Sonnet 5) — BETA-77：找文件双模式检索 + 启动期原生索引预热

**承接**：紧接 BETA-76，用户经 `/goal` 下达第二轮重构：① 找文件搜索框"快速查找"（输入即出，类 Everything）+"深度检索"（回车触发，元数据+语义全量）双模式；② desktop 启动时元数据索引常驻内存极速启动，语义索引后台准备不卡启动。**关键决策**：`quick_search` 不走 NL intent 解析/policy/同义词扩展的完整管线，直接从 `ToolRegistry` 按 id 取已注册 `SearchableTool` 构造最小 intent 直调 `SearchBackend::search()`。**产出**：`search/quick.rs`（quick_search_impl + 粗排）；`SearchView.tsx` 防抖下拉；`main.rs` 原生索引后台预热。**验证**：workspace 全绿；浏览器预览注入 Tauri IPC stub 做了真实点击验证，抓到并修复一个真实竞态 bug（回车提交后重新聚焦触发防抖把刚关的下拉又弹回来）。

### 2026-08-20 — Claude Code (Sonnet 5) — BETA-76：重构移除外部 Everything 依赖，内置 MFT/USN 原生索引服务

**承接**：用户经 `/goal` 下达：① 去掉对外部 Everything 的集成与依赖；② 内置实现"everything"索引的服务（MFT 枚举 + 内存索引 + USN Journal 实时监控）。**关键决策**：MFT 枚举用官方 `FSCTL_ENUM_USN_DATA`；内存索引线性扫描扁平 `HashMap`（Everything 自身的实际技术路线）；新 crate 因需 `unsafe fn` 单独放开 `unsafe_code`。**产出**：新 crate `packages/search-backends/native-index`；删除 `packages/search-backends/everything`；desktop/harness 全链路同步改名。**验证**：本机真实 Windows 11 实机全绿；真机冒烟确认非管理员降级路径符合设计。**未尽事宜**：管理员权限下完整真机功能验证留待下一轮——本轮（BETA-78）已解决这个缺口。
