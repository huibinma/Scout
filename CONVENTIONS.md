# Scout 协作规范

> 本文件定义三个 AI 工具（Claude Code / Codex / Gemini）协作 Scout 项目时的共同规则。
> 单一信源原则：进度只在 [STATUS.md](./STATUS.md)，目标只在 [PROJECT.md](./PROJECT.md)，规则只在本文件。

## 1. 单一信源（Single Source of Truth）

| 信息 | 文件 | 更新者 | 节奏 |
|---|---|---|---|
| 项目目标 / 定位 / 范围 / 阶段路线 | [PROJECT.md](./PROJECT.md) | 用户主导，AI 协助 | 月级 |
| **任务级长期地图：4 阶段任务清单、依赖、估时、验收、里程碑、风险** | **[ROADMAP.md](./ROADMAP.md)** | **任一工具；重大方向调整需用户确认** | **周级 / 阶段切换时** |
| 进度 / 当前阶段 / 当前 task / 下一步 / 会话日志 | [STATUS.md](./STATUS.md) | 每次"收工"由当前会话工具更新 | 每次会话 |
| 协作规则 / 编码规范 / 收工流程 | 本文件 | 用户主导 | 月级 |
| 详细产品/技术/IP/风险计划、设计文档 | [docs/](./docs/) | 谨慎修改，重大变更需告知用户 | 按需 |
| 代码 | 各 package / platform 目录 | 协作开发 | 持续 |

**ROADMAP vs STATUS 的分工**（最容易混淆，必读）：

- **ROADMAP** 是"全程地图"：完整 4 阶段、所有 task ID、依赖、估时、阶段切换标准。看 ROADMAP 知道"项目要走到哪、整体怎么走"。
- **STATUS** 是"当前位置"：当前阶段、正在做哪个 task ID、下一步、阻塞、最近会话日志。看 STATUS 知道"现在做到哪、下一步做什么"。
- **不要把 task 详情写进 STATUS** — task 详情（依赖 / 估时 / 验收）放在 ROADMAP，STATUS 只用 task ID 引用。
- **不要把当前进度写进 ROADMAP** — task 状态可以在 ROADMAP 维护（done / in_progress / blocked），但"当前正在哪一步、为什么阻塞、下一步细节"放 STATUS。

任何信息**不要在多处复述**。需要引用的地方用相对路径链接。

## 2. 会话开始流程

任一工具开新会话时，**必须按顺序读完**以下文件，然后才开始任何工作：

1. 入口文件（CLAUDE.md / AGENTS.md / GEMINI.md，对应当前工具）— 提示协作模式
2. [PROJECT.md](./PROJECT.md) — 项目目标、定位、目标场景与架构（约 5KB，全文）
3. [STATUS.md](./STATUS.md) — 当前进度、当前 task、下一步、阻塞、最近会话日志（全文；**"全文读"成立的前提是 STATUS 守住 §3 的体积预算**——软目标 10-12KB、硬上限 15KB。开头的「📍 速览」块 15 行内回答"目标 / 当前进展 / 待执行"三问）
4. 本文件（CONVENTIONS.md）— 协作规则（约 9KB，全文）
5. [ROADMAP.md](./ROADMAP.md) — **定向读取，不必全文**（约 220KB，且随 task 卡片追加持续增长）：默认读 §2 阶段总览 + 当前阶段所在小节（如 B 阶段读 §3.3）；涉及验收 / 阶段切换时再读 §6 / §8。**只有**阶段切换、重排任务、改 ROADMAP 本身、处理历史争议时才全文读取。

如有正在进行中的代码工作，再读对应 package 的 README 和最近修改的源文件。

**不要**跳过 STATUS.md 就开始动手。STATUS 告诉你"做什么"，ROADMAP 告诉你"这件事在整个项目里的位置和它的验收标准"。

> **为什么 ROADMAP 改为定向读取**：ROADMAP 是长期全程地图（大部分是已完成阶段、模板、历史修订摘要），每次会话全文读取浪费大量上下文。按需读当前阶段小节即可获得"做什么 + 验收标准"，需要全局视图时再全文读。

## 3. 收工流程

用户说"**收工**"时，当前会话工具必须按以下步骤更新仓库，然后再结束：

1. **更新 [STATUS.md](./STATUS.md)**（固定骨架：**📍 速览 / 当前 Task / 下一步 / 阻塞 / 会话日志**，五节之外不加新节）：
   - 刷新「📍 速览」（≤15 行）：第一行阶段 + 版本；定位一句话（带 PROJECT.md 链接，随 PROJECT 演进刷新）；当前 task ID + 一句话；下一步 top-3；阻塞 top-N
   - 更新「当前 Task」——**只保留最新 1 条（≤15 行）**，被替换的旧条目并入会话日志摘要
   - 更新「下一步」——**单一列表 ≤10 条**，收工时去重刷新，不许同一列表复述两处
   - 更新「阻塞 / 待用户决策」（如有）
   - 在「会话日志」**顶部**追加**摘要**（5-15 行）：承接 / 关键决策 / 产出 / 未尽事宜：
     ```markdown
     ### YYYY-MM-DD — 工具名 — 主题
     **承接** … **关键决策** … **产出** … **未尽事宜** …
     ```
   - **会话日志单层制**：STATUS 只放摘要，保留最近 **≤5 条**；更早的摘要收工时直接删除——完整改动细节、验证命令与输出、对话流水始终可在 `git log` / commit diff 中查到，不需要额外维护归档文件。**当前 task / 下一步只在 STATUS 直接维护**，不与会话日志摘要复述，防止两处漂移。
   - **体积自检（收工闸门，已自动化）**：STATUS.md 软目标 **10-12KB**、硬上限 **15KB**。超过硬上限不得收工 commit——除非本次 commit 正是在做瘦身且结束后达标。**闸门由仓库 pre-commit hook 强制执行**（[scripts/hooks/pre-commit](./scripts/hooks/pre-commit)：STATUS 超 15KB 阻止提交、ROADMAP 超 230KB 预警提示 CLEAN-6；瘦身过渡期可 `SCOUT_ALLOW_FAT_STATUS=1` 放行一次）。新 clone 一次性启用：`git config core.hooksPath scripts/hooks`。
2. **更新 [ROADMAP.md](./ROADMAP.md)**：
   - 把本会话改动的 task 状态同步（done / in_progress / blocked / dropped）
   - 如有新增 task：按 §1 字段约定追加，分配新 ID（如 PROTO-10），更新依赖图
   - 如有估时调整：更新对应 task 的估时列
   - **重大方向调整**（关键路径任务删除、阶段范围变化）：先与用户确认再改
3. **更新相关文档**（仅当本会话改动了某个 package 的设计、引入了新依赖、改了 schema 等）：
   - 对应 package 的 README
   - `docs/third-party-licenses.md`（新增依赖时）
   - 其他被影响的设计文档
   - **踩坑沉淀检查**：本会话若修复了真机 / 评测暴露的缺陷，确认已**同轮**沉淀 fixture / 回归测试 / 闸门（enterprise `queries.tsv`、CI 门控等）——不允许只修不沉淀；「踩坑→闸门」是护城河核心资产
4. **git 提交**：
   - 用一次 commit 把本次会话的所有改动落库
   - commit message 用中文，简洁说明本次会话主题；不要包含 AI 自夸式签名

完成以上步骤后，向用户确认提交内容，等待用户最终确认才结束。

## 4. 工具识别

每次会话开始时在内部确认（不必输出给用户）：
- 当前是 Claude Code / Codex / Gemini 中哪一个
- 收工时在 STATUS.md 中用对应名字记录

简称约定：

- Claude Code → `Claude Code`（如需注明模型，写 `Claude Code (Opus 4.7)` / `Claude Code (Sonnet 4.6)`）
- Codex → `Codex`
- Gemini → `Gemini`

## 5. 跨工具一致性

- **路径**：仓库内部引用一律用相对路径，不用绝对路径。
- **行尾**：LF（macOS / Linux 与 Windows 共用，由 `.gitattributes` 统一）。
- **编码**：UTF-8。
- **文档语言**：中文为主；代码注释和 commit message 也用中文；代码标识符（变量、函数、类型）用英文。
- **Markdown 风格**：CommonMark；标题层级从 `#` 开始；不滥用 emoji。

## 6. 编码规范（按语言）

随技术选型确定，原则：

- **Rust**：`rustfmt` + `clippy --deny warnings`；公共 API 必须有文档注释。
- **TypeScript**：`prettier` + `eslint`；严格 `tsconfig`（`strict: true`）。
- **Python**（仅训练侧）：`ruff` + `black`；`pyproject.toml` 管理依赖。
- **跨平台**：禁止硬编码 `/` 或 `\` 路径分隔符；用平台 API（`std::path::PathBuf`、Node `path`、Python `pathlib`）。
- **平台特定代码**：集中在 `packages/search-backends/{spotlight,windows-search,everything}` 和 `platform/{macos,windows}`，不要散落到通用代码里。

## 7. 安全与隐私（贯穿所有代码）

- 训练数据严禁含真实用户文件名、路径、内容。
- 测试样例使用合成数据或公开数据集，commit 前自查。
- Tracing / log 默认脱敏（不记录完整路径、不记录文件正文片段）。
- 工具调用必须经过 SearchBackend / Tool Registry，不要绕过 schema 校验直接 shell out。
- 详细原则参见 [docs/Scout项目注意事项与风险清单.md](./docs/Scout项目注意事项与风险清单.md)。

## 8. 提交规范

- commit message 用中文，第一行 ≤ 50 字符，说明本次会话主题。
- 一次"收工"对应一个 commit，除非工作内容明显可拆。
- 不在 commit message 里加 AI 工具自夸签名（如 "Generated with X"）。
- 不要在没有用户授权时 push 到远程。
- **发布 GitHub Release 时，Release 说明（body）必须包含本次 changelog**：修复了哪些问题、新增 / 变更了哪些特性、必要的前置要求（如依赖安装、运行提示）。不要只用固定模板。可在打 tag 后用 `gh release edit <tag> --notes` 补全。

## 9. 文档变动政策

- 修改 [PROJECT.md](./PROJECT.md) 涉及范围/方向变化：先与用户确认。
- 修改 `docs/` 下三份计划书：先与用户确认。
- 修改 [STATUS.md](./STATUS.md)：收工时自动，无需确认。
- 修改 [ROADMAP.md](./ROADMAP.md)：
  - task 状态同步、新增子 task、估时调整 → 收工时自动，无需确认。
  - 删除关键路径 task、改变阶段范围、调整 §6 出场指标 → 先与用户确认。
- 修改本文件：先与用户确认。
- 修改 `docs/third-party-licenses.md`：引入/移除依赖时强制更新，不需要确认。

## 10. 模糊或冲突时

- AI 工具之间的判断冲突 → 以最新的 [STATUS.md](./STATUS.md) 与用户在会话中的决策为准。
- 文档之间的冲突：
  - 方向 / 范围类 → 以 [PROJECT.md](./PROJECT.md) 为准。
  - 任务 / 阶段 / 验收类 → 以 [ROADMAP.md](./ROADMAP.md) 为准。
  - 进度 / 当前状态类 → 以 [STATUS.md](./STATUS.md) 为准。
- 拿不准 → 在 STATUS.md 的「阻塞 / 待用户决策」加一条，向用户确认。
