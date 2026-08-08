# Scout 项目状态

> **每次会话开始**：必读本文件 + [PROJECT.md](./PROJECT.md) + [CONVENTIONS.md](./CONVENTIONS.md)；[ROADMAP.md](./ROADMAP.md) 按 [CONVENTIONS §2](./CONVENTIONS.md) 定向读取。  
> **每次“收工”**：按 [CONVENTIONS §3](./CONVENTIONS.md) 维护本文件固定骨架（速览 / 当前 Task / 下一步 / 阻塞 / 会话日志）。
> 会话日志只保留最近 ≤5 条摘要；完整历史见 `git log`。

## 📍 速览

- **阶段**：B（Beta）进行中；P ✅ / M 代码层 ✅，M→B 正式切换仍待 [ROADMAP §8](./ROADMAP.md) 长周期项；总体 parser-only evals 已达 99.4%（994/6/0、fail=0）。
- **版本**：**v0.9.49 已 push + tag ⏳**（CI / Release macOS / Release Windows 已触发，结果未 poll 确认）。**仓库已转公开**：[github.com/huibinma/Scout](https://github.com/huibinma/Scout)（public，完整历史归档于 private 的 `huibinma/scout-archive`）。
- **定位**：开源免费（MIT）本地语义检索底座——**面向 agent 的本地文件搜索工具**（经 MCP 接入 Claude Code / Codex 等），同时提供桌面应用供人直接使用；不做分析层，分析经 MCP daemon + 外部 LLM 组合。口号 **Deep Local Search**。以 [PROJECT.md](./PROJECT.md) 为准。
- **当前 task**：**仓库转公开 + BETA-73 已完成**——详见下方「当前 Task」节。
- **下一步 top-3**：① 确认 v0.9.49 CI / Release 产物结果并补 changelog；② 真机验证积压（BETA-64~73，均需真机走查）；③ 获取设计伙伴/首个真实部署。
- **阻塞**：Class A 仅剩双平台 evals 真机；Class B 已清零。

## 当前 Task

**2026-08-08（最新）— 仓库转公开 + BETA-73：全局快捷键可自定义、与托盘驻留联动禁用**

**仓库转公开**：`huibinma/Scout` 改名归档为 `huibinma/scout-archive`（private，保留完整 122 提交历史）；新建 `huibinma/Scout`（public，orphan 单提交 `c10f5e3` 起步，LociFind 时期内容与提交历史清零、作者身份统一 `huibinma@users.noreply.github.com`）；README 定位改写为"面向 agent 的本地文件搜索工具（经 MCP 接入）+ 供人直接用的桌面应用"，不再是"个人搜索 Agent"措辞。

**BETA-73**：全局唤起快捷键改为真正可自定义——此前 UI 是禁用输入框、注册逻辑完全无视 settings.json；新增 `update_global_shortcut` 命令校验 + 重新注册 + 落盘，与其他程序冲突时当场报错、不落盘。默认值改回 `Ctrl+Space`（跨平台统一；BETA-72 刚改成 `Ctrl+Alt+S` 防冲突，本轮用户明确要求改回，已知取舍）。「关闭窗口时驻留系统托盘」从 WindowsPane 底部挪到常规面板顶部与快捷键合并一个分区，取消勾选（Windows 默认态）时快捷键录制器联动禁用——关窗不驻留托盘＝进程真退出，快捷键唤不起已退出的程序。详见 [ROADMAP BETA-73](./ROADMAP.md)。

**本轮发版**：bump v0.9.49，已 push main + tag，CI / Release macOS / Release Windows 均已触发；用户明确无需等待结果，本轮未 poll 确认产物，下轮会话或用户自查。

## 下一步

1. **确认 v0.9.49 CI / Release 结果**：`gh run list`/`gh release view v0.9.49` 复查三个 workflow 是否全绿，成功后 `gh release edit v0.9.49 --notes` 补真实 changelog（本轮改动：全局快捷键可自定义 + 默认改回 Ctrl+Space + 与托盘驻留联动禁用）。
2. **SignPath 代码签名工作流**：`.github/workflows/release-windows.yml`/`PROJECT.md`/`docs/install.md`/`README.md` 有一份**未提交**的 SignPath Foundation 免费签名集成改动（新增 `CODE_SIGNING.md`/`.signpath/artifact-configuration.xml`），本轮会话未触碰、留给用户/相关会话自行核对并提交。
3. **真机验证积压（BETA-64~73）**：新设置页结构、数据目录统一迁移、`optimize_fts` 大库提速、多条件检索过滤、Windows 关闭到托盘、发现层枚举、快捷键/模型本地导入/gguf 多后端发现、本轮快捷键可自定义与托盘联动——均需 Windows/macOS 真机走查，清单见各 ROADMAP BETA 卡片。
4. **`SCOUT_ENABLE_EMBED` 默认禁用是否重新评估**：语义索引在生产默认配置下完全不工作，是否值得专门验证修复能否覆盖 v0.8.5 那次原始崩溃场景（需要真机 + 小范围受控测试，不能直接改默认值）。
5. **embeddinggemma-300m 的 cosine_threshold 正式评审**：按 bge-m3 同款流程确定校准值后回填 `CALIBRATED_COSINE_THRESHOLDS`。
6. **设计伙伴 / 首个真实部署获取**：律所卷宗、内部审计或离职归档任一场景。

**流程备忘**：桌面发版 = bump `apps/desktop/src-tauri/tauri.conf.json` + `apps/desktop/src-tauri/Cargo.toml` + `Cargo.lock` → 推 `main` → 推 `v*` tag → Release 产物完成后补真实 changelog。Windows 编带 llama 的 scoutd 使用 `scripts\build-scoutd-llama.bat`。**本机 Rust 工具链路径订正（2026-07-30）**：`~/.cargo/bin` 实际不存在，`cargo`/`rustc` 在 `~/.rustup/toolchains/stable-aarch64-apple-darwin/bin`——非交互式 Bash 工具的 PATH 不含该目录（`.zshrc` 的 PATH 设置对该场景不生效），需要每次显式 `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"` 后才能跑 `cargo`/`scripts/ci.sh`。

## 阻塞 / 待用户决策

- **Class A（外部条件，阻塞出场评测、不阻塞代码）**：BETA-09(a)/MVP-26/28 双平台 evals——需 Windows 真机 + 完整 Spotlight 索引 macOS。
- **Class B（产品决策）**：已全部清零。
- **发布操作项**：无；GitHub CLI keyring 登录与 SSH push 均已验证。

## 会话日志

> 摘要 ≤5 条；更早历史见 `git log`。

### 2026-08-08 — Claude Code (Sonnet 5) — 仓库转公开 + BETA-73：全局快捷键可自定义、与托盘驻留联动

**承接**：用户要求把 Scout 私有仓库转为 public，需清理历史中的旧品牌名 LociFind、把 README/description 定位改为"面向 agent 的工具"而非"服务人的搜索 agent"、排查其他公开风险；随后追加"全局唤起快捷键默认 Ctrl+Space 且允许用户改"的易用性需求；最后要求把「关闭窗口时驻留系统托盘」与快捷键合并一个分区、后者随前者联动禁用。**关键决策**：转公开走 orphan 单提交方案（复用本项目 LociFind 时代已验证过的先例）而非改写现有 122 提交历史——`huibinma/Scout` 改名归档为私有 `scout-archive`、新建同名 public 仓库；作者身份改用 GitHub noreply 邮箱避免暴露本机用户名/主机名。默认快捷键改回 `Ctrl+Space` 与 BETA-72 当天早些时候的决定（改 `Ctrl+Alt+S` 防冲突）矛盾，发现后用 AskUserQuestion 向用户核实，用户确认按本次要求改回。浏览器实测中发现并修复两个真实 bug：录制器对空 `KeyboardEvent.code` 缺兜底会拼出畸形快捷键字符串；Esc 取消录制未 `stopPropagation` 导致外层设置面板的 Esc-关闭监听器一并把整个弹窗关掉。**产出**：`shortcut.rs` 新增 `update_global_shortcut` 命令（校验 + 重新注册 + 落盘，冲突当场报错不落盘）；`settings.rs` 把 `global_shortcut` 纳入 `merge_backend_managed_fields`（同 MCP token 保护套路，防表单整体保存冲掉刚生效的值）；新增 `ShortcutRecorder.tsx`/`lib/shortcut.ts`；`GeneralPane.tsx` 把「关闭窗口时驻留系统托盘」从 `WindowsPane.tsx` 底部挪到顶部与快捷键合并、`shortcutDisabled = IS_WINDOWS && !close_to_tray` 联动置灰。**验证**：Rust 197 测试全绿、clippy/fmt 净；`tsc`/`vite build` 全绿；浏览器注入 `__TAURI_INTERNALS__.invoke` stub 实测录制/冲突报错/Esc 取消/禁用态四条路径。bump v0.9.49，push + tag，CI/Release 三个 workflow 已触发（用户明确不等结果）。**未尽事宜**：v0.9.49 CI/Release 结果未 poll 确认；发现 `PROJECT.md`/`README.md`/`docs/install.md`/`release-windows.yml` 有一份未提交的 SignPath Foundation 免费代码签名集成改动（非本会话产生），未触碰、已在「下一步」标注留给用户处理；commit message 曾误加 AI 签名（`Co-Authored-By`），发现 CONVENTIONS §8 禁止后续提交已改正，前两条已推送未改写。

### 2026-08-08 — Claude Code (Sonnet 5) — BETA-72：易用性四项（快捷键 / 模型本地导入 / 路径检测提示 / gguf 多后端发现）

**承接**：用户提四项易用性问题：Everything 未装安装提示 + 快捷键防冲突；模型下载支持指定本地文件 + 统一"嵌入模型"措辞；"扫描本机 gguf"扩展多后端 + 路径覆盖"检测"给具体信息。**关键决策**：走读发现 winget 安装提示（`EverythingCheckStep.tsx`/`EverythingPane.tsx`）已在 BETA-71 前完整实现，报"已存在"而非重做，只改了确有缺口的快捷键默认值；模型本地导入复用已有 `import_local_model` 命令接原生文件选择器，不新建流程；gguf 多后端发现给 `windows-search`/`spotlight` 各加一个 `find_files_by_extension`，镜像 everything crate 已跑通的同名写法；`probe_model_file` 加 `kind` 参数复用 `resolve_target_paths`（单一信源），避免探测口径与加载口径 drift。**产出**：见 [ROADMAP BETA-72](./ROADMAP.md) 完整清单。**验证**：`scripts/ci.sh` 全套 + `--target x86_64-pc-windows-gnu` 交叉编译验证 Windows 专属分支 + `tsc`/`vite build` 全绿；`scoutd` e2e 3 个失败复现既有本机沙盒问题（本轮未触及该模块）。**未尽事宜**：均未做真机验证，需下一轮确认。

### 2026-07-30 — Claude Code (Sonnet 5) — BETA-71：v0.9.46 Windows 真机回归四项修复

**承接**：用户 v0.9.46 发布后在 Windows 真机连续四轮反馈：新图标不合适、按钮配色四种混用、Everything 检测按钮多余、快速入门第 3/4 步莫名跳过；之后又追加删除第 6 步示例板块与更新关于弹窗文案两项小需求。**关键决策**：图标直接 `git checkout v0.9.45 --` 逐文件回退而非重新设计，最小化改动面；按钮配色统一前先枚举全部实际用到的 `<button>`/内联样式点（而非只改样式表里的类），发现 Onboarding 六个文件里散落同款内联黑/橙样式，一并处理；诊断"步骤跳过"没有停在表面猜测"可能已下载过"，而是走读 `ModelDownloadStep.tsx` 的两个 `useEffect`，定位到只有它比其余 onboarding 步骤多一条"检测到已存在就静默推进"的 effect——这是唯一例外，据此判定是设计不一致而非用户环境问题，移除该 effect 而不是简单加长延时掩盖；顺带在同一文件发现 Rust 侧幂等短路缺体积校验的独立 bug，一并修复。删除示例板块前先验证其 `onPickExample` 的 `/?q=` 从未被 `SearchView.tsx` 消费，确认功能本就不生效、删除无损失。**产出**：见 [ROADMAP BETA-71](./ROADMAP.md) 完整清单（图标 9 文件回退、`styles.css`/Onboarding 6 文件/`SynonymsPane.tsx` 按钮配色统一、`EverythingPane.tsx`/`WindowsPane.tsx` 检测口径对齐、`ModelDownloadStep.tsx`+两平台 Onboarding 页 Onboarding 跳过修复、`model_download.rs` 幂等校验修复、`FirstIndexStep.tsx` 示例板块删除 + `ExampleQueries.tsx` 孤儿组件删除、`AboutDialog.tsx` 文案）。**验证**：`scripts/ci.sh` 全套（fmt/clippy/build/test/synonym-recall）跑通，`scoutd` e2e 3 个失败复现既有本机沙盒问题（非本轮引入）；`tsc`/`vite build` 全绿；浏览器预览逐项截图确认。**未尽事宜**：均未做真机验证，需下一轮确认实际效果。

