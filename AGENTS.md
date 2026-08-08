# Codex 入口（Scout 项目）

> 你是 **Codex**，正在协作开发 Scout。这是一个由 **Claude Code / Codex / Gemini** 三个工具轮换协作的项目，每次会话可能换工具。
> 本文件只做"指路"，**真正的项目状态、目标、规则在以下三份共享文档**。
>
> 注：本仓库使用 `AGENTS.md` 作为 Codex 的入口文件（Codex 与其他常见 agent CLI 共享这一约定）。Claude Code 用 `CLAUDE.md`，Gemini 用 `GEMINI.md`，三份入口结构对等、内容一致。

## 会话开始：必读

按顺序读完以下三份（都不大，全文）：

1. [PROJECT.md](./PROJECT.md) — 项目目标、定位、目标场景、范围、阶段路线、架构概览（**产品定位 / 目标场景 / "不做什么"一律以此为准，本文件不复述**）
2. [STATUS.md](./STATUS.md) — **当前阶段、当前 task、下一步、阻塞、最近会话日志**（开头「📍 速览」15 行内可答"目标 / 进展 / 待执行"三问）
3. [CONVENTIONS.md](./CONVENTIONS.md) — 三工具协作规则、收工流程、编码规范、安全/隐私底线

再**定向读取** [ROADMAP.md](./ROADMAP.md)（约 220KB，**勿全文**）：默认读 §2 阶段总览 + 当前阶段小节；涉及验收 / 阶段切换读 §6 / §8；仅改 ROADMAP 本身或需全局视图时才全文。详见 [CONVENTIONS.md §2](./CONVENTIONS.md)。

STATUS 告诉你"现在做什么"，ROADMAP 告诉你"这件事在整个项目里的位置与验收标准"。

如果当前阶段已进入代码工作，再读对应 package 的 README 和最近修改的源文件。

## 会话结束："收工"流程

用户说"**收工**"时，按 [CONVENTIONS.md §3 收工流程](./CONVENTIONS.md) 执行：

1. 更新 [STATUS.md](./STATUS.md)（速览 / 当前 task / 下一步刷新；会话日志**摘要**在顶部追加；体积守软 10-12KB / 硬 15KB）
2. 同步 [ROADMAP.md](./ROADMAP.md)（本会话动到的 task 状态、估时、新增子 task）
3. 同步可能影响到的 README、`docs/third-party-licenses.md` 等
4. 用一次中文 commit 落库（不要 AI 自夸签名）
5. 向用户确认提交内容

在 STATUS.md 会话日志中署名 `Codex`。

## Codex 特定备注

- 该项目有详细的中文计划书在 `docs/`，遇到方向性疑问优先翻 `docs/local-personal-search-agent-project-plan.md`。
- 与用户沟通用中文；代码注释、commit message 用中文；代码标识符用英文。
- 仓库内引用一律相对路径。
- Codex 在执行 shell 命令时优先使用项目根目录的相对路径，避免污染 `/tmp` 等系统目录。
