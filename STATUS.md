# Scout 项目状态

> **每次会话开始**：必读本文件 + [PROJECT.md](./PROJECT.md) + [CONVENTIONS.md](./CONVENTIONS.md)；[ROADMAP.md](./ROADMAP.md) 按 [CONVENTIONS §2](./CONVENTIONS.md) 定向读取。  
> **每次“收工”**：按 [CONVENTIONS §3](./CONVENTIONS.md) 维护本文件固定骨架（速览 / 当前 Task / 下一步 / 阻塞 / 会话日志）。
> 会话日志只保留最近 ≤5 条摘要；完整历史见 `git log`。

## 📍 速览

- **阶段**：B（Beta）进行中；P ✅ / M 代码层 ✅，M→B 正式切换仍待 [ROADMAP §8](./ROADMAP.md) 长周期项；总体 parser-only evals 已达 99.4%（994/6/0、fail=0）。
- **版本**：**v0.9.48 已发布 ✅**（BETA-72：易用性四项——全局快捷键改 Ctrl+Alt+S、模型下载支持手动选本地文件、路径覆盖"检测"给具体信息、自动发现 gguf 扩展到 Windows 搜索/Spotlight）。
- **定位**：开源免费（MIT）本地语义检索底座——个人桌面搜索 + 企业冷归档检索；不做分析层，分析经 MCP daemon + 外部 LLM 组合。口号 **Deep Local Search**。以 [PROJECT.md](./PROJECT.md) 为准。
- **当前 task**：**BETA-72 已完成**——用户提四项易用性问题：全局快捷键 `Ctrl+Space` 改 `Ctrl+Alt+S`（防冲突）；快速入门模型下载步骤新增手动选本地文件入口（此前 macOS 上完全没有本地导入手段）；设置页语义/生成模型路径覆盖的"检测"按钮不再回静态文案、改为真的探测默认槽位给具体信息；"扫描本机 gguf"从仅 Everything 扩展到 Everything/Windows 搜索/Spotlight 三选一。另核实"Everything 未装给 winget 安装提示"已在既有实现里覆盖，未改动。详见 [ROADMAP BETA-72](./ROADMAP.md)。当前无新的进行中 task，下一步见下方。
- **下一步 top-3**：① 真机验证积压（BETA-64~72：新设置页结构/数据目录迁移/optimize_fts/UI 进度/过滤/托盘/启动提速/新设计视觉/图标回退/按钮配色/Onboarding 修复/本轮四项易用性改动，均需真机走查）；② `SCOUT_ENABLE_EMBED` 默认禁用是否重新评估；③ 获取设计伙伴/首个真实部署。
- **阻塞**：Class A 仅剩双平台 evals 真机；Class B 已清零。

## 当前 Task

**2026-08-08（最新）— BETA-72：易用性四项（快捷键 / 模型本地导入 / 路径检测提示 / gguf 多后端发现）**

用户提四项易用性问题，逐项落地：①全局快捷键 `Ctrl+Space` 易与其他软件冲突，Windows/Linux 默认改 `Ctrl+Alt+S`（macOS `Option+Space` 不变），`shortcut.rs` 新增单测防手滑打错格式。②快速入门第 3/4 步（`ModelDownloadStep.tsx`，与设置页语义召回内嵌复用）新增"选择本地已下载的文件…"按钮，走原生文件选择器 + 复用既有 `import_local_model` 命令；同轮统一"下载语义模型"→"下载嵌入模型"文案（此前与步骤标题不一致）。③`probe_model_file` 新增 `kind` 参数，路径留空时真的探测默认槽位文件（复用 `resolve_target_paths`，与实际加载路径同一信源），报告"已使用默认模型 · 文件名 · 体积"或"默认模型尚未下载"，不再是静态"未指定路径"文案；设置页两个路径覆盖字段同步加"浏览…"按钮。④`discover_gguf_models` 从仅 Everything（Windows 专属需装软件）扩展到 Everything → Windows 搜索（Windows 自带）→ Spotlight（macOS 自带）三选一，`windows-search`/`spotlight` 两个 crate 新增 `find_files_by_extension`（镜像 everything crate 既有写法）。详见 [ROADMAP BETA-72](./ROADMAP.md)。

**本轮发版**：v0.9.48（BETA-72）**已发布 ✅**——本地 `scripts/ci.sh` 全套（fmt/clippy/build/test/synonym-recall）+ 两个新 crate 的 Windows 交叉编译验证均通过，`scoutd` e2e 3 个用例复现既有本机沙盒问题（非本轮引入，本轮改动未触及 `apps/daemon`/`packages/scout-server`）；bump + push + tag，CI（4m52s）/ Release macOS（10m31s）/ Release Windows（33m9s）均成功，`gh release edit` 已补真实 changelog。

## 下一步

1. **真机验证积压（BETA-64~72）**：新设置页结构（Everything/Windows 子分区、隐私数据合并块）、数据目录统一迁移、`optimize_fts` 大库提速、真百分比/ETA UI、多条件检索过滤、Windows 关闭到托盘、启动提速、发现层枚举、设计视觉/图标回退/按钮配色/Onboarding 修复、本轮快捷键/模型本地导入/gguf 多后端发现——均需 Windows/macOS 真机走查，清单见各 ROADMAP BETA 卡片。
2. **`SCOUT_ENABLE_EMBED` 默认禁用是否重新评估**：语义索引在生产默认配置下完全不工作，是否值得专门验证修复能否覆盖 v0.8.5 那次原始崩溃场景（需要真机 + 小范围受控测试，不能直接改默认值）。
3. **embeddinggemma-300m 的 cosine_threshold 正式评审**：按 bge-m3 同款流程确定校准值后回填 `CALIBRATED_COSINE_THRESHOLDS`。
4. **设计伙伴 / 首个真实部署获取**：律所卷宗、内部审计或离职归档任一场景。
5. **P3（跨阶段并行等架构改动）**：`last_run_stage_ms` 埋点是为了积累真机数据，留到有数据后再决定是否投入。

**流程备忘**：桌面发版 = bump `apps/desktop/src-tauri/tauri.conf.json` + `apps/desktop/src-tauri/Cargo.toml` + `Cargo.lock` → 推 `main` → 推 `v*` tag → Release 产物完成后补真实 changelog。Windows 编带 llama 的 scoutd 使用 `scripts\build-scoutd-llama.bat`。**本机 Rust 工具链路径订正（2026-07-30）**：`~/.cargo/bin` 实际不存在，`cargo`/`rustc` 在 `~/.rustup/toolchains/stable-aarch64-apple-darwin/bin`——非交互式 Bash 工具的 PATH 不含该目录（`.zshrc` 的 PATH 设置对该场景不生效），需要每次显式 `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"` 后才能跑 `cargo`/`scripts/ci.sh`。

## 阻塞 / 待用户决策

- **Class A（外部条件，阻塞出场评测、不阻塞代码）**：BETA-09(a)/MVP-26/28 双平台 evals——需 Windows 真机 + 完整 Spotlight 索引 macOS。
- **Class B（产品决策）**：已全部清零。
- **发布操作项**：无；GitHub CLI keyring 登录与 SSH push 均已验证。

## 会话日志

> 摘要 ≤5 条；更早历史见 `git log`。

### 2026-08-08 — Claude Code (Sonnet 5) — BETA-72：易用性四项（快捷键 / 模型本地导入 / 路径检测提示 / gguf 多后端发现）

**承接**：用户提四项易用性问题：Everything 未装安装提示 + 快捷键防冲突；模型下载支持指定本地文件 + 统一"嵌入模型"措辞；"扫描本机 gguf"扩展多后端 + 路径覆盖"检测"给具体信息。**关键决策**：走读发现 winget 安装提示（`EverythingCheckStep.tsx`/`EverythingPane.tsx`）已在 BETA-71 前完整实现，报"已存在"而非重做，只改了确有缺口的快捷键默认值；模型本地导入复用已有 `import_local_model` 命令接原生文件选择器，不新建流程；gguf 多后端发现给 `windows-search`/`spotlight` 各加一个 `find_files_by_extension`，镜像 everything crate 已跑通的同名写法；`probe_model_file` 加 `kind` 参数复用 `resolve_target_paths`（单一信源），避免探测口径与加载口径 drift。**产出**：见 [ROADMAP BETA-72](./ROADMAP.md) 完整清单。**验证**：`scripts/ci.sh` 全套 + `--target x86_64-pc-windows-gnu` 交叉编译验证 Windows 专属分支 + `tsc`/`vite build` 全绿；`scoutd` e2e 3 个失败复现既有本机沙盒问题（本轮未触及该模块）。**未尽事宜**：均未做真机验证，需下一轮确认。

### 2026-07-30 — Claude Code (Sonnet 5) — BETA-71：v0.9.46 Windows 真机回归四项修复

**承接**：用户 v0.9.46 发布后在 Windows 真机连续四轮反馈：新图标不合适、按钮配色四种混用、Everything 检测按钮多余、快速入门第 3/4 步莫名跳过；之后又追加删除第 6 步示例板块与更新关于弹窗文案两项小需求。**关键决策**：图标直接 `git checkout v0.9.45 --` 逐文件回退而非重新设计，最小化改动面；按钮配色统一前先枚举全部实际用到的 `<button>`/内联样式点（而非只改样式表里的类），发现 Onboarding 六个文件里散落同款内联黑/橙样式，一并处理；诊断"步骤跳过"没有停在表面猜测"可能已下载过"，而是走读 `ModelDownloadStep.tsx` 的两个 `useEffect`，定位到只有它比其余 onboarding 步骤多一条"检测到已存在就静默推进"的 effect——这是唯一例外，据此判定是设计不一致而非用户环境问题，移除该 effect 而不是简单加长延时掩盖；顺带在同一文件发现 Rust 侧幂等短路缺体积校验的独立 bug，一并修复。删除示例板块前先验证其 `onPickExample` 的 `/?q=` 从未被 `SearchView.tsx` 消费，确认功能本就不生效、删除无损失。**产出**：见 [ROADMAP BETA-71](./ROADMAP.md) 完整清单（图标 9 文件回退、`styles.css`/Onboarding 6 文件/`SynonymsPane.tsx` 按钮配色统一、`EverythingPane.tsx`/`WindowsPane.tsx` 检测口径对齐、`ModelDownloadStep.tsx`+两平台 Onboarding 页 Onboarding 跳过修复、`model_download.rs` 幂等校验修复、`FirstIndexStep.tsx` 示例板块删除 + `ExampleQueries.tsx` 孤儿组件删除、`AboutDialog.tsx` 文案）。**验证**：`scripts/ci.sh` 全套（fmt/clippy/build/test/synonym-recall）跑通，`scoutd` e2e 3 个失败复现既有本机沙盒问题（非本轮引入）；`tsc`/`vite build` 全绿；浏览器预览逐项截图确认。**未尽事宜**：均未做真机验证，需下一轮确认实际效果。

### 2026-07-30 — Claude Code (Sonnet 5) — BETA-70：删除 LoRA 微调 / training 全部资产

**承接**：用户判断 Scout 定位不涉及模型 LoRA 优化，要求删除 `training/` 及相关资产聚焦项目范围。**关键决策**：删前先核实实际使用情况而非直接照做——用户此前一轮对话中已问过"training 目录是否完全没用"，走读发现 `training/mlx-lora/README.md` 自称生产模型来自 BETA-24 LoRA 融合产物，一度误判为仍在用；进一步查代码后修正结论：`apps/desktop/src-tauri/src/model_download.rs` 的 `GENERATION_HF_URL` 实测直接拉取 HuggingFace 原版 `unsloth/Qwen3-0.6B-GGUF`（未微调），BETA-24 的 LoRA-fused GGUF 产物本就 `.gitignore`、从未真正进入分发链路；结合 ROADMAP BETA-11B/15B 已明确弃"LoRA 在线扩词"选 embedding 路径，确认删除安全、无在建工作依赖，遂按用户指示执行。**产出**：删除 `training/` 全目录（21 文件，datasets/evals/generators/mlx-lora）+ `packages/evals/src/bin/build_lora_dataset.rs`（含 Cargo.toml `[[bin]]` 注册）+ `fixtures.rs` 内 `GenerateLoraAugKeywords` 子命令与配套 `AugSeed`/`aug_case_from_seed`/`template_seeds`/`split_train_heldout`/`check_unique_ids`/`generate_lora_aug_keywords` 整块 + `lora_aug_tests` 测试模块（连带清 2 个变为未用的 import）+ `packages/evals/fixtures/lora-aug-keywords/` + 已核销的 `docs/mac-session-todo.md`；`.gitignore` 移除 `training/` 专属条目（保留通用 `*.gguf`/`*.safetensors`）；`README.md`/`scripts/README.md` 移除 `training/` 提及；`docs/windows-setup.md` §5 本地模型获取指南改写为指向生产实际使用的 HF 直链（原文档"从 Mac 训练机拷贝 + 校验 sha256"流程本就是未落地的过时描述）；`media_search.rs` 两处"留给 LoRA 阶段处理"注释改为"已知遗留，不再规划专门修复"。**不动**：ROADMAP 历史决策条目（BETA-08/17/24/11B/15B 等）按时间戳保留、不重写历史；`docs/local-personal-search-agent-project-plan.md`/`docs/product-positioning-and-competitors.md`/`docs/manual-test-scenarios.md`/`packages/search-backends/common/src/lib.rs` 的历史叙事性 LoRA 提及非可执行资产，不删改。**验证**：`cargo check/clippy -D warnings/fmt --check/test` 对 `scout-evals`/`scout-intent-parser`/`scout-search-backend` 及 `cargo check --workspace` 全绿。**未尽事宜**：本轮纯代码库清理，未 git commit（等用户确认），非 UI/可预览改动，无需真机验证。

### 2026-07-30 — Claude Code (Sonnet 5) — BETA-69：桌面端 UI 深度设计优化 + 品牌图标重绘 + 许可证改为纯 MIT

**承接**：用户参照 microsoft/Ontology-Playground 的明亮愉快风格要求对 scout-desktop 做一轮视觉优化，随后陆续追加三项延伸要求——字号协调性检查、图标"有必要的话调用 codex 生成优化"、许可证仅保留 MIT。**关键决策**：设计改动定性为 token 化 + 一致性收口而非重新设计，保留 Scout 暖色橙色品牌识别（不照搬 Ontology Playground 的 MS 蓝）；图标环节先用 ToolSearch 确认本环境无独立 image-gen 工具，转而用本机已装的 `codex` CLI 原生 `image_gen`（其 `imagegen` skill 文档明确建议不要用生成式图片替换已有 SVG 图标系统），因此只重绘品牌 logo、不动 phosphor-icons 功能图标；两版生成候选（3D 光泽丝带 / 扁平几何）用 AskUserQuestion 交给用户选，而非自行拍板；许可证改动排除了第三方依赖清单（`docs/third-party-licenses.md`）与 ROADMAP 历史决策日志，只改 Scout 自身的许可声明。**产出**：`styles.css` 新增 8 档字号 token + 圆角/过渡/点缀色 token，~50 处硬编码值替换；`src-tauri/icons` 全套派生资源 + `public/scout-icon.png` 换新图标；`LICENSE-APACHE` 删除，7 处许可证声明文档同步改写。**验证**：本机 Rust 工具链路径有出入（`~/.cargo/bin` 不存在，需从 `~/.rustup/toolchains/.../bin` 显式加 PATH，已记入下方"流程备忘"），修正后 `scripts/ci.sh` 全套跑通，仅 `scoutd` e2e 3 个用例复现既有本机沙盒问题（非本轮引入）；`npm run build`/浏览器预览确认视觉改动。**v0.9.46 已发布 ✅**：CI/Release macOS/Release Windows 三个 workflow 全绿，`gh release edit` 已补真实 changelog。**未尽事宜**：本轮全部改动均未做真机验证（尤其新图标在 macOS/Windows 系统级图标位置的实际显示效果）。
