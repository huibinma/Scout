# Scout 项目状态

> **每次会话开始**：必读本文件 + [PROJECT.md](./PROJECT.md) + [CONVENTIONS.md](./CONVENTIONS.md)；[ROADMAP.md](./ROADMAP.md) 按 [CONVENTIONS §2](./CONVENTIONS.md) 定向读取。  
> **每次“收工”**：按 [CONVENTIONS §3](./CONVENTIONS.md) 维护本文件固定骨架（速览 / 当前 Task / 下一步 / 阻塞 / 会话日志）。
> 会话日志只保留最近 ≤5 条摘要；完整历史见 `git log`。

## 📍 速览

- **阶段**：B（Beta）进行中；P ✅ / M 代码层 ✅，M→B 正式切换仍待 [ROADMAP §8](./ROADMAP.md) 长周期项；总体 parser-only evals 已达 99.4%（994/6/0、fail=0）。
- **版本**：**v0.9.54 已发布**；本机（Windows 11 实机）工作区另有 BETA-76 重构改动**未提交/未发布**。仓库：[github.com/huibinma/Scout](https://github.com/huibinma/Scout)（public，完整历史归档于 private 的 `huibinma/scout-archive`）。
- **定位**：开源免费（MIT）本地语义检索底座——**面向 agent 的本地文件搜索工具**（经 MCP 接入 Claude Code / Codex 等），同时提供桌面应用供人直接使用；不做分析层，分析经 MCP daemon + 外部 LLM 组合。口号 **Deep Local Search**。以 [PROJECT.md](./PROJECT.md) 为准。
- **当前 task**：**BETA-76 重构：移除外部 Everything 依赖，内置 MFT/USN 原生索引服务替代**——详见下方「当前 Task」节。
- **下一步 top-3**：① 用户决定是否提交 BETA-76 改动；② 管理员权限下真机验证 `scout-native-index`（全盘枚举/USN tail/搜索质量）；③ 继续 BETA-64~75 真机验证积压。
- **阻塞**：BETA-76 改动待用户决定是否提交；Class A 仅剩双平台 evals 真机；Class B 已清零。

## 当前 Task

**2026-08-20（最新，Claude Code）— BETA-76：重构移除 Everything 依赖，内置原生索引服务**

用户经 `/goal` 下达：① 去掉对外部 Everything 的集成与依赖；② 内置一个实现"everything"索引的服务（MFT 枚举、内存索引结构、USN Journal 实时监控），做到低资源占用、极速文件元数据检索。新增 `packages/search-backends/native-index`（`scout-native-index`）：`sys.rs` 封装 `CreateFileW`/`DeviceIoControl`（`FSCTL_ENUM_USN_DATA` 全量 MFT 枚举 + `FSCTL_QUERY_USN_JOURNAL`/`FSCTL_READ_USN_JOURNAL` 增量 tail），`MemIndex` 扁平 `HashMap` + 父子链路径重建（刻意不做倒排索引，与 Everything 实际技术路线一致），`SearchBackend` 实现承接原 Everything 查询角色。删除 `packages/search-backends/everything` crate 及全部引用（harness `BackendKind::Everything`→`NativeFileIndex`、desktop 设置/UI/permissions/model_download 改名，前端 `EverythingPane`/`EverythingCheckStep` 改名并去掉安装引导）。完整实现细节见 [ROADMAP BETA-76](./ROADMAP.md)。**本次未提交、未发布**，改动全部在工作区。

## 下一步

1. **BETA-76 是否提交**：用户决定——workspace `cargo check/clippy -D warnings/test/fmt --check` 与桌面 `tsc`/`vite build` 均已在本机 Windows 11 实机验证全绿。
2. **BETA-76 管理员权限真机验证**：以管理员身份运行一次 `cargo test -p scout-native-index --test real_volume -- --ignored --nocapture`，确认真实全盘 MFT 枚举与 USN tail 行为；非管理员降级路径已验证（`VolumeOpen`「拒绝访问」→ 回退目录扫描）。
3. **v0.9.54 真机回归**：Release、DMG、NSIS、changelog 与 Windows PE 闸门均已收口；Windows 上逐项复测 BETA-75，重点确认模型 helper 原生崩溃时 UI 主进程仍存活。
4. **自动更新端到端真机回归**：用 v0.9.53 装包实测发现 v0.9.54 → 下载 → 安装 → 保留数据 → 自动重启（macOS + Windows）。
5. **真机验证积压（BETA-64~75）**：按各 ROADMAP 卡片清单走查。

**流程备忘**：桌面发版 = bump `apps/desktop/src-tauri/tauri.conf.json` + `apps/desktop/src-tauri/Cargo.toml` + `Cargo.lock` → 推 `main` → 推 `v*` tag → Release 产物完成后补真实 changelog。Windows 编带 llama 的 scoutd 使用 `scripts\build-scoutd-llama.bat`。**本机 Rust/Node 工具链路径（2026-08-20，Windows 实机）**：`cargo`/`rustc` 在 `%USERPROFILE%\.cargo\bin`、`node`/`npm` 在 `%ProgramFiles%\nodejs`，均不在默认 PATH，需显式补全后才能跑 `cargo`/`npm`/`npx`。（此前 macOS 沙盒的 `~/.rustup/...` 路径订正已不适用于本机。）

## 阻塞 / 待用户决策

- **BETA-76 提交与否**：本次重构（移除 Everything、内置原生索引）已完成并验证，改动留在工作区未提交——按 git 安全协议，提交需用户明确要求。
- **Class A（外部条件，阻塞出场评测、不阻塞代码）**：BETA-09(a)/MVP-26/28 双平台 evals——需 Windows 真机 + 完整 Spotlight 索引 macOS。
- **Class B（产品决策）**：已全部清零。
- **SignPath 集成暂缓**：2026-08-09 用户确认证书申请暂搁置；本次只做静态 CRT/PE 导入验证，不恢复代码签名流程。

## 会话日志

> 摘要 ≤5 条；更早历史见 `git log`。

### 2026-08-20 — Claude Code (Sonnet 5) — BETA-76：重构移除外部 Everything 依赖，内置 MFT/USN 原生索引服务

**承接**：用户经 `/goal` 下达：① 去掉对外部 Everything 的集成与依赖；② 内置实现"everything"索引的服务（MFT 枚举 + 内存索引 + USN Journal 实时监控），低资源占用、极速文件元数据检索。**关键决策**：MFT 枚举用官方 `FSCTL_ENUM_USN_DATA`（NTFS 驱动保证正确性）而非手解卷原始簇数据；内存索引刻意不做倒排/trigram，线性扫描扁平 `HashMap`（Everything 自身的实际技术路线，低维护开销）；新 crate 因需 `windows` crate 的 `unsafe fn`（`CreateFileW`/`DeviceIoControl`），不整体继承 workspace `unsafe_code = forbid`，改为仅在 `sys.rs` 一处放开；新增 `BackendKind::NativeFileIndex`（不复用 `NativeIndex`）以保持 harness 路由对"文件名 vs 正文索引"的既有区分不被破坏。**产出**：新 crate `packages/search-backends/native-index`（`sys`/`record`/`index`/`service`/`backend`，28 单测 + 1 个 `--ignored` 真机冒烟）；删除 `packages/search-backends/everything`；discovery 层、harness（fallback/capability/fanout_merge/intent_router）、desktop（settings/permissions/model_download/main，`enable_everything`→`enable_native_file_index` 带 serde alias 兼容）、前端（`EverythingPane`/`EverythingCheckStep`→`NativeIndexPane`/`NativeIndexCheckStep`，UI 从装机引导改为管理员权限提示）全部同步改名。**验证**：本机真实 Windows 11 实机，workspace `cargo check/clippy -D warnings/test/fmt --check` 全绿（仅 2 个既有 `scout-platform-macos` 测试因本机非 macOS 失败，与本轮无关）；desktop `tsc`/`vite build` 通过；真机冒烟确认非管理员降级路径符合设计。**未尽事宜**：管理员权限下完整真机功能验证留待下一轮；是否提交留用户决定；历史设计文档按惯例未改写，仅同步当前状态类文档（third-party-licenses/windows-setup/PROJECT/README 等）。

### 2026-08-11 — Codex — BETA-75：v0.9.54 Windows/“找文件”四项缺陷收口

**承接**：用户连续反馈结果清单“在文件夹中显示”定位错误、`\\?\` 路径前缀、内容匹配缺文件大小，以及其它 Windows 机器操作时 `MSVCP140.dll` 闪退，并要求完整提交、启动 CI/Release。**关键决策**：前三项在 common/path metadata 层统一修；闪退没有 crash dump，按 faulting module + release `/MD` 配置 + 仓库既有 llama native crash 证据锁定最高概率路径，同时做根因缓解（静态 CRT、去 `mtmd`）和故障隔离（常驻 helper），避免仅靠安装 VC++ Runtime 掩盖。**产出**：详见 [ROADMAP BETA-75](./ROADMAP.md)，v0.9.54 已发布，macOS/Windows 三项资产齐全，真实 changelog 已补全。**验证**：workspace fmt/clippy/build 通过；沙箱外 desktop 210/210；Windows GNU desktop feature check + model-runtime clippy 通过；llama tests 31 pass/3 ignored；synonym recall 100%/FP 0%；tsc/vite 通过；GitHub CI、Release macOS、Release Windows 全绿，`dumpbin /DEPENDENTS` 确认最终 EXE 不导入 `MSVCP*` / `VCRUNTIME*`。workspace 仅 `scoutd` 3 个既有正文读取 e2e 失败，串行复现、与本轮模块无关且远端 CI 不运行该 binary e2e。**未尽事宜**：仍需原问题 Windows 真机复测四项缺陷；无 dump 前根因结论保持“高概率”而非绝对定论。

### 2026-08-09 — Claude Code (Sonnet 5) — BETA-74：桌面自动更新（提前实现 V10-04），发布 v0.9.50

**承接**：用户要求给桌面端做自动更新——定期检查 GitHub 新 Release、左下角提醒、点更新后台下载静默安装、保留配置数据 MCP token、装完自动重启；随后追加要求把「自动更新」「轮询间隔」做成设置项（默认开 + 4 小时，允许关闭 + 30 分钟~24 小时可调，原始需求是 8 小时后改 4 小时）。**关键决策**：技术方案用 AskUserQuestion 向用户核实后选「轻量自研」而非 `tauri-plugin-updater`——后者需生成新签名密钥对存 GitHub secret、且要改两个 Release workflow 生成合并 `latest.json`，两个 workflow 都标 `prerelease: true` 导致 GitHub `/releases/latest` 别名不可用还得另建固定 tag 托管 manifest，工作量和对发布流水线的改动明显更大；轻量方案直接调 GitHub Releases API + 下载既有安装包静默装，不碰 CI、不需要签名密钥。走读代码发现 `nsis/uninstall-hooks.nsh` 本就有 `$UpdateMode` 守卫，静默重装本就是官方支持的原地升级路径，settings.json/index.db/models/MCP token 全部自动保留，不需要自己另写保留逻辑。**产出**：新增 `update.rs`（镜像 `model_download.rs` 既有约定：reqwest stream 下载 + 进度 event + in-flight 守卫）+ `UpdateToast.tsx`/`useAutoUpdate.ts`（左下角四态 toast）；`settings.rs` 新增 `auto_update_enabled`/`auto_update_interval_minutes`（默认开 + 240 分钟，读取 clamp [30,1440]）+ `GeneralPane.tsx` 新增开关与间隔下拉，联动禁用。bump v0.9.50，push + tag，CI/Release macOS/Release Windows 三个 workflow 全部成功，`gh release edit` 补全真实 changelog。**插曲**：release 进行中用户提出"SignPath 签名暂时搁置，disable 掉 release workflow 里的签名 action"——排查发现 SignPath 集成从未提交/推送（只是 working tree 里一份未 commit 的 120 行 diff，此前 STATUS「下一步」条目已记录留给用户），实际跑在 CI 上的 `release-windows.yml` 本就是未签名版本（committed HEAD 从未含 SignPath 引用），无需任何改动，已向用户说明并保持原状不动。**验证**：Rust 新增 12 个单测全绿（版本比较/资产平台匹配/mock GitHub 响应解析/settings 默认值与 clamp 边界），`cargo test -p scout-desktop` 211 全绿，`clippy -D warnings`/`fmt --check` 在 macOS 与 `--target x86_64-pc-windows-gnu` 两目标均净；`tsc`/`vite build` 全绿；浏览器预览注入 `window.__TAURI_INTERNALS__` invoke/事件 stub，逐一截图验证左下角提醒四态定位与交互、设置页开关联动禁用、`update_settings` 保存回传正确值；针对真实 `api.github.com/repos/huibinma/Scout/releases` 拉取核对资产命名与匹配规则一致。**未尽事宜**：自动更新「发现新版本」真实路径未做端到端真机验证（需下一个真实版本发布后用旧版本装包测）；未加「手动检查更新」按钮、也未做失败时的权限提升重试，均超出用户原始需求范围、保持最小实现；SignPath 集成仍保持未提交搁置状态，等用户重新推进证书申请后再处理。

### 2026-08-08 — Claude Code (Sonnet 5) — 仓库转公开 + BETA-73：全局快捷键可自定义、与托盘驻留联动

**承接**：用户要求把 Scout 私有仓库转为 public，需清理历史中的旧品牌名 LociFind、把 README/description 定位改为"面向 agent 的工具"而非"服务人的搜索 agent"、排查其他公开风险；随后追加"全局唤起快捷键默认 Ctrl+Space 且允许用户改"的易用性需求；最后要求把「关闭窗口时驻留系统托盘」与快捷键合并一个分区、后者随前者联动禁用。**关键决策**：转公开走 orphan 单提交方案（复用本项目 LociFind 时代已验证过的先例）而非改写现有 122 提交历史——`huibinma/Scout` 改名归档为私有 `scout-archive`、新建同名 public 仓库；作者身份改用 GitHub noreply 邮箱避免暴露本机用户名/主机名。默认快捷键改回 `Ctrl+Space` 与 BETA-72 当天早些时候的决定（改 `Ctrl+Alt+S` 防冲突）矛盾，发现后用 AskUserQuestion 向用户核实，用户确认按本次要求改回。浏览器实测中发现并修复两个真实 bug：录制器对空 `KeyboardEvent.code` 缺兜底会拼出畸形快捷键字符串；Esc 取消录制未 `stopPropagation` 导致外层设置面板的 Esc-关闭监听器一并把整个弹窗关掉。**产出**：`shortcut.rs` 新增 `update_global_shortcut` 命令（校验 + 重新注册 + 落盘，冲突当场报错不落盘）；`settings.rs` 把 `global_shortcut` 纳入 `merge_backend_managed_fields`（同 MCP token 保护套路，防表单整体保存冲掉刚生效的值）；新增 `ShortcutRecorder.tsx`/`lib/shortcut.ts`；`GeneralPane.tsx` 把「关闭窗口时驻留系统托盘」从 `WindowsPane.tsx` 底部挪到顶部与快捷键合并、`shortcutDisabled = IS_WINDOWS && !close_to_tray` 联动置灰。**验证**：Rust 197 测试全绿、clippy/fmt 净；`tsc`/`vite build` 全绿；浏览器注入 `__TAURI_INTERNALS__.invoke` stub 实测录制/冲突报错/Esc 取消/禁用态四条路径。bump v0.9.49，push + tag，CI/Release 三个 workflow 已触发（用户明确不等结果）。**未尽事宜**：v0.9.49 CI/Release 结果未 poll 确认；发现 `PROJECT.md`/`README.md`/`docs/install.md`/`release-windows.yml` 有一份未提交的 SignPath Foundation 免费代码签名集成改动（非本会话产生），未触碰、已在「下一步」标注留给用户处理；commit message 曾误加 AI 签名（`Co-Authored-By`），发现 CONVENTIONS §8 禁止后续提交已改正，前两条已推送未改写。

