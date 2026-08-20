# Scout 项目状态

> **每次会话开始**：必读本文件 + [PROJECT.md](./PROJECT.md) + [CONVENTIONS.md](./CONVENTIONS.md)；[ROADMAP.md](./ROADMAP.md) 按 [CONVENTIONS §2](./CONVENTIONS.md) 定向读取。  
> **每次“收工”**：按 [CONVENTIONS §3](./CONVENTIONS.md) 维护本文件固定骨架（速览 / 当前 Task / 下一步 / 阻塞 / 会话日志）。
> 会话日志只保留最近 ≤5 条摘要；完整历史见 `git log`。

## 📍 速览

- **阶段**：B（Beta）进行中；P ✅ / M 代码层 ✅，M→B 正式切换仍待 [ROADMAP §8](./ROADMAP.md) 长周期项；总体 parser-only evals 已达 99.4%（994/6/0、fail=0）。
- **版本**：**v0.9.57 已发布**（BETA-78 服务化拆分 + 装机 bug 修复）；BETA-79 全面评审对照 Everything 已完成，发现并修复 native-index 一处真实路径重建 bug，即将随下一版本发布，见下方「当前 Task」。仓库：[github.com/huibinma/Scout](https://github.com/huibinma/Scout)（public，完整历史归档于 private 的 `huibinma/scout-archive`）。
- **定位**：开源免费（MIT）本地语义检索底座——**面向 agent 的本地文件搜索工具**（经 MCP 接入 Claude Code / Codex 等），同时提供桌面应用供人直接使用；不做分析层，分析经 MCP daemon + 外部 LLM 组合。口号 **Deep Local Search**。以 [PROJECT.md](./PROJECT.md) 为准。
- **当前 task**：**BETA-79：全面评审 native-index/scoutd 重构对照 Everything，修复发现的 bug + 发版**——详见下方「当前 Task」节。
- **下一步 top-3**：① 用户真机验证 v0.9.57 服务注册是否成功（`services.msc` 能看到 `Scoutd` Running）；② connection.json 明文 token 的 ACL 加固（需按装机用户 SID 精确授权，需真机多用户环境验证，本会话条件不具备）；③ 继续 BETA-64~75 真机验证积压。
- **阻塞**：无；Class A 仅剩双平台 evals 真机 + BETA-78/79 真机装机验证。

## 当前 Task

**2026-08-20（最新，Claude Code）— BETA-79：全面评审 native-index/scoutd 重构，对照 Everything 逐项核对 + 修复发现的 bug + 发版**

用户经 `/goal` 下达："全面评审这次重构的架构和代码实现，对照'everything'的公开功能和关键技术实现做逐项比对，以确保Scout实现了对everything的全面替换；修复、优化评审过程中发现的bug"。用 WebSearch/WebFetch 核实 voidtools 官方文档后逐项核对：核心索引机制（MFT+USN Journal）、ReFS 不支持（Everything 自身同样不支持，非缺口）均确认对等；原以为的最大缺口"权限隔离架构"核对后发现 BETA-78 已经解决（`scoutd` LocalSystem service + 桌面非管理员经 token 连接，对齐 Everything Service 模式）；查询语法（通配符/正则/布尔 NOT）差异判定为设计取舍（`SearchIntent` 是跨 4 个 backend 共用的后端无关抽象，非 es.exe DSL 克隆）非 bug。**修复的真实 bug**：[index.rs](../packages/search-backends/native-index/src/index.rs) 的 `MemIndex::full_path` 祖先链断裂时会静默拼出一个看似合法实则完全错误的路径（如误报成卷根下的错误位置），而非返回"未找到"——已修复为断链一律 `None`，新增回归测试。详见 [ROADMAP BETA-79](./ROADMAP.md)。

## 下一步

1. **v0.9.57 真机验证**：用户确认 `services.msc` 里 `Scoutd` 服务已注册且 Running、桌面能连上（承接自 BETA-78，尚未有用户反馈）。
2. **connection.json ACL 加固**（BETA-79 评审发现，未修）：明文 admin token 当前继承 `%ProgramData%` 默认 ACL，本机任意标准用户可读；正确修法需按装机时的交互用户 SID 精确授权（简单粗暴的"仅 SYSTEM+Administrators"方案会因 UAC token 过滤反而连桌面客户端自己都读不到，已验证过不可行），需要真实多用户/提权环境验证。
3. **USN tail 线程健壮性**（BETA-79 评审发现，未修）：遇任意错误永久停止且无日志，索引会静默停留在旧快照；需要错误分类（journal 失效 vs 瞬时 I/O）+ 可观测性。
4. **BETA-78 后续任务**：本地 reindex 循环 / `mcp_service.rs` / 设置页 roots 编辑迁移到调用 scoutd；desktop 原生窗口人工点击复测。
5. **真机验证积压（BETA-64~75）**：按各 ROADMAP 卡片清单走查。

**流程备忘**：桌面发版 = bump `apps/desktop/src-tauri/tauri.conf.json` + `apps/desktop/src-tauri/Cargo.toml` + `Cargo.lock` → 推 `main` → 推 `v*` tag → Release 产物完成后补真实 changelog。**Windows-only 代码的 cfg 分支不会被本机 Windows clippy 看到**——`#[cfg(not(windows))]` 分支的 lint 问题只有 Linux CI 编译到该分支时才会现形。Windows 编带 llama 的 scoutd 用 `scripts\build-scoutd-llama.bat`（本机开发态）；CI release-windows.yml 现在也会编一份带 llama-cpp 的 scoutd.exe 打进桌面安装包（BETA-78）。**本机 Rust/Node 工具链路径（2026-08-20，Windows 实机）**：`cargo`/`rustc` 在 `%USERPROFILE%\.cargo\bin`、`node`/`npm` 在 `%ProgramFiles%\nodejs`、`gh` 在 `C:\Program Files\GitHub CLI`，均不在默认 PATH，需显式补全后才能跑 `cargo`/`npm`/`npx`/`gh`。

## 阻塞 / 待用户决策

- **Class A（外部条件，阻塞出场评测、不阻塞代码）**：BETA-09(a)/MVP-26/28 双平台 evals——需 Windows 真机 + 完整 Spotlight 索引 macOS；**BETA-78 管理员权限/真实安装包验证**——需管理员权限会话或用户自行手测。
- **Class B（产品决策）**：已全部清零。
- **SignPath 集成暂缓**：2026-08-09 用户确认证书申请暂搁置；本次只做静态 CRT/PE 导入验证，不恢复代码签名流程。

## 会话日志

> 摘要 ≤5 条；更早历史见 `git log`。

### 2026-08-20 — Claude Code (Sonnet 5) — BETA-79：全面评审 native-index/scoutd 重构对照 Everything

**承接**：用户经 `/goal` 下达全面评审要求，对照 Everything 公开功能/关键技术实现逐项核对 BETA-76~78 是否构成完整替换。**方法**：WebSearch/WebFetch 核实 voidtools 官方 FAQ/searching 文档（而非仅凭训练知识），逐项比对现有代码。**结论**：核心索引机制（MFT 批量枚举+USN Journal tail）与 ReFS 不支持均确认对等（Everything 自身也不支持 ReFS）；评审开始时判断的"最大缺口"——桌面进程本身需要管理员权限——核对后发现 BETA-78 已解决（scoutd LocalSystem service + 桌面非管理员经 token 连接，对齐 Everything Service 免 UAC 模式）；查询语法（通配符/正则/布尔 NOT）判定为跨 backend 抽象的设计取舍，非缺口。**修复的真实 bug**：`MemIndex::full_path`（`packages/search-backends/native-index/src/index.rs`）祖先链断裂时静默拼出看似合法实则错误的路径（如误报到卷根），而非返回"未找到"——修复为断链一律 `None`，新增回归测试，环状防御性熔断分支保持不变（不破坏既有测试契约）。**评审中发现但审慎未修的两项**：connection.json 明文 token 的 ACL 加固（验证过"仅 SYSTEM+Administrators"方案会因 UAC token 过滤反而打断桌面客户端自己的读取，需按装机用户 SID 精确授权，需真机多用户环境验证）；USN tail 线程遇任意错误永久停止且无日志（需错误分类+可观测性）——均判断为"记录待跟进优于无法验证的仓促修复"，非疏漏。**验证**：workspace `cargo check/clippy -D warnings/test/fmt --check` 全绿（native-index 30 单测）。

### 2026-08-20 — Claude Code (Sonnet 5) — v0.9.57：修复 v0.9.56 装机后 Scoutd 服务未注册

**承接**：用户真机装完 v0.9.56 反馈 `services.msc` 里找不到 `Scoutd` 服务。**排查**：先读 `hooks.nsh`/`service.rs`/`cli.rs`/`main.rs` 全链路代码逻辑，均无问题（子命令 kebab-case 匹配、`ServiceManager::create_service` 调用、`LocalSystem` 账户配置都对），怀疑过未签名二进制被 Defender 拦截；让用户实机核对三件事：安装目录下有没有 `resources` 子目录、`scoutd.exe` 实际在哪、Windows 安全中心有没有相关拦截记录。**根因**：用户回报安装目录下根本没有 `resources` 子目录，`scoutd.exe` 直接躺在 `Scout` 安装目录根——即 Tauri NSIS 打包器把 `bundle.resources` 平铺到 `$INSTDIR` 根，而 `hooks.nsh` 里两处 `nsExec::ExecToLog` 硬编码的路径是 `$INSTDIR\resources\scoutd.exe`，根本不存在；`nsExec` 找不到文件直接失败，紧接着 `Pop $0` 又把返回码扔掉不检查（设计上"失败只记日志、不中断安装"），导致装机界面完全正常，服务却从未被创建。桌面自身加载同义词词典的代码（`main.rs` 的 `resource_dir().join("synonyms/zh.yaml")`）本来就没加 `resources/` 前缀，是对的；`hooks.nsh` 是唯一一处路径拼错的地方。**修复**：`hooks.nsh` 两处路径去掉多余的 `resources\` 前缀，改成 `$INSTDIR\scoutd.exe`；bump `apps/desktop/src-tauri/{Cargo.toml,tauri.conf.json}` + `Cargo.lock` 到 v0.9.57。**未尽事宜**：本次修复未经真机验证（本环境无 Windows 管理员会话），需要用户重新下载 v0.9.57 安装包实机确认 `services.msc` 能看到 `Scoutd` 服务且 Running。

### 2026-08-20 — Claude Code (Sonnet 5) — BETA-78：Scout 拆分为后台 Windows Service + 前端瘦客户端桌面

**承接**：用户经 `/goal` 下达服务/桌面拆分需求（见上「当前 Task」），选择一次性打通端到端。**关键决策**：① 桌面 harness 管线（policy/refine/同义词/多类型均衡/tracer）里程碑式复杂，早期研究阶段误判过其"可整体丢弃改走远程 `/search`"，实际读全 `search.rs` 后发现代价太大——改为更小侵入的方案：只把 `search.local`/`search.semantic`/`search.native_file_index` 三个 `SearchBackend` 换成 `RemoteSearchBackend`（经新增 `POST /backend/search` 代理 `search_expanded()`），桌面其余管线零改动，产品体验零回归。② `scoutd` 新增 `windows-service` crate 支持，`Cli` 加可选子命令（`bootstrap-personal-config`/`install-service`/`uninstall-service`/`service`），不给子命令走今天的前台团队部署路径，零迁移。③ 语义相似度下限过滤原在桌面本地 `SemanticIndexBackend` 内部执行，backend 挪服务端后服务不知道桌面这个个性化设置，改为在 `RemoteSearchBackend` 拿到结果后本地 filter，行为不变。④ `/admin/personal/roots` 因 `CollectionRuntime.meta.roots` 无运行时热更新路径，只做落盘 + `restart_required` 标志，不做实时局部 reindex（避免半吊子不一致状态）。⑤ tauri.conf.json 不直接加 `scoutd.exe` 到 `bundle.resources`（会让所有人本地 `cargo check`/`tauri dev` 因文件不存在而报错），改为 CI release-windows.yml 用 `tauri build --config` 传内联 JSON 只在 CI 注入。**产出**：`apps/daemon/src/{personal,service}.rs`（新增）、`cli.rs`/`main.rs` 改造；`packages/scout-server` 新增 `quick_search.rs`/`search_http.rs`，`admin.rs`/`app.rs`/`collections.rs`/`tools/search.rs` 加 `/search`/`/search/quick`/`/backend/search`/`/admin/status`/`/admin/personal/roots`；`apps/desktop/src-tauri/src/service_client.rs`（新增）+ `main.rs` 改造；`nsis/uninstall-hooks.nsh` 改名 `hooks.nsh` 加 POSTINSTALL/PREUNINSTALL；`.github/workflows/release-windows.yml`/`apps/daemon/README.md` 同步。**验证**：workspace `cargo check/clippy -D warnings/fmt --check/test`（scoutd 25 + scout-server 99 + scout-desktop 211 = 335 测试）全绿；手动起真实 scoutd 前台实例（真实索引一份中英文测试语料）+ `curl` 验证 5 个端点端到端返回正确命中。**未尽事宜**：无管理员权限/无法弹 UAC，`--install-service`/真实 Windows Service/真实 NSIS 安装包链路未验证；桌面本地 reindex 循环/`mcp_service.rs`/设置页 roots 编辑/`reindex`/`reindex_root` 命令均未迁移到调用 scoutd（刻意延后，避免预览功能因索引不一致出新 bug）；无原生桌面自动化工具，Tauri 原生窗口下的搜索 UI 未做人工点击复测。

**追加（同日，发版收尾）**：按用户指示推进 commit + CI + Release。bump 到 v0.9.56 → push main + tag → CI/Release macOS 一次过、**Release Windows 首次失败**：`tauri-action` 的 `args` 字段里内联 JSON（`--config "{\"bundle\":...}"`）经 GitHub Actions 把 `args` 当 YAML 纯量传给 action，`\"` 未被当转义引号处理，action 内部按空白拆 argv 时把 `{\` 拆成独立 token 当文件路径去找，报 `Provided config path ...\{\ does not exist`——**根因**：内联 JSON 经多层字符串传递（YAML → action 内部 argv 拆分）时转义规则不透明，不该在 CI 里这样传结构化数据。**修复**：CI 里先 `cat > apps/desktop/tauri.beta78.patch.json` 写一个真实 JSON 文件，`--config` 改传文件路径，彻底绕开转义问题；顺带把这步挪到耗时的 llama.cpp patch/build 之前，配置类错误几秒内暴露、不用等 8-9 分钟编译。移动 `v0.9.56` tag 到修复后的 commit 重新触发，三个 workflow 全绿。**验证**：从实际 CI 日志核实 `tauri build --config tauri.beta78.patch.json ...` 无报错跑完并产出 NSIS 包（Tauri 对不存在的声明 resource 会 fail-fast，能跑完即证明 `scoutd.exe` 确实被收进 `bundle.resources`）；`gh release view` 确认两个平台安装包均已上传到同一个 v0.9.56 release。用户追问 Windows 安装包体积从 7.5MB 涨到 13.9MB 的原因——已用 `gh release view --json assets` 精确核对（7,938,214→14,559,331 字节）+ 构建日志双重确认是新打包的 llama-cpp 版 `scoutd.exe`，非异常膨胀，已在会话中向用户说明。已用 `gh release edit --notes-file` 补真实 changelog（BETA-78 变更点 + 已知限制 + 放行说明）。

### 2026-08-20 — Claude Code (Sonnet 5) — v0.9.55 发版：BETA-76+77 完整提交 + CI/Release + 本地验收

**承接**：用户经 `/goal` 下达"完整commit一次，并完成一轮CI和Release，并对release结果在本地进行完整验收、对验收发现的bug进行修改"。流程：bump 版本到 v0.9.55 → 本地全量校验 → push main + tag → CI 首次失败（Linux runner 编译 `scout-native-index` 触发 clippy 死代码/未用 import——`#[cfg(windows)]` 生产路径代码本机 Windows clippy 永远看不到）→ 修复后 CI/Release macOS/Release Windows 三个 workflow 全绿 → 本地下载真实 Release 安装包验证：4 个 backend 全部注册成功，原生索引非管理员会话优雅降级，无异常日志 → 补真实 changelog。**本轮验收范围边界**：无原生桌面自动化工具、非管理员会话无法弹 UAC，管理员权限下的真实 MFT 全盘枚举/USN 实时监控行为、桌面 GUI 快速查找下拉的真机点击交互，本轮未做端到端验证。

