# Scout 项目目标

> 单一信源：本项目的目标、定位、范围、阶段路线图。
> 本文件相对稳定。涉及"做到哪/接下来做什么"的动态信息在 [STATUS.md](./STATUS.md)。
> 详细计划书在 [docs/](./docs/)。

## 一句话定位

Scout 是一个本地优先、跨平台（macOS + Windows）的**本地语义检索底座**：对个人，它是用自然语言**按意思**查找电脑里文件、文档、音乐、图片的个人搜索 Agent——**哪怕记不清确切文件名或用词、甚至跨中英文，也能找到**；对团队/企业，它是**数据不出门的冷归档检索底座**（headless daemon 形态，经 MCP 接入外部 LLM 工作流）。

英文：

> Scout is a local-first, cross-platform personal search agent for your files, documents, media, and memories on macOS and Windows — it finds them by meaning, even when you don't recall the exact name or wording, and across languages. For teams, the same retrieval stack runs headless as a privacy-preserving archive search daemon.

口号：

> Deep Local Search.

## 目标场景

> 2026-07-02 定位收敛（方案与评审见当时的会话记录）。

- **个人桌面搜索**（既有主线）：本地文件按意思找、跨语言模糊召回；获客与打磨入口。
- **企业冷归档检索**（三场景，ROADMAP §3.3 B7 并行子线）：
  1. **律所案件卷宗检索**——多格式卷宗（含扫描件）按意思找，信息墙隔离（BETA-35/36）。
  2. **企业内部审计取证检索**——凭证 / 合同 / 邮件跨格式检索 + 检索留痕（BETA-36/37）。
  3. **离职员工材料归档检索**——检索者不熟悉语料组织方式，语义召回优势最大化（BETA-36/38）。
- 三场景共同画像：**敏感数据不出门 + 冷归档 + 检索者不熟悉语料组织方式 + 需留痕**——OS 原生语义搜索（锁新硬件、管不到归档服务器）覆盖不到的缝隙。

## 核心原则

- **开源免费**（2026-07-04 拍板；2026-07-29 由 MIT OR Apache-2.0 双许可改为仅 MIT）：MIT 许可，任何人可自由使用、修改、再分发代码与软件；不做商业分发前置（商标注册 / 代码签名证书 / Apple Developer / 付费域名），分发走 GitHub Releases 与包管理器渠道。
- **本地优先**：默认不上传文件名、路径、内容、搜索词、索引数据。
- **轻量可用**：普通 16GB Mac 或 Windows 电脑可流畅运行。
- **跨平台一致**：macOS 与 Windows 共享同一份 Agent Harness、Search Intent JSON、UI、模型。
- **后端可插拔**：系统搜索（Spotlight / Windows Search）是默认后端，Everything 是 Windows 上的可选加速。
- **可解释可控**：Agent 每一步工具调用、权限判断、错误状态可追踪。
- **渐进扩展**：先做好系统搜索的自然语言前端，再发展为完整本地个人搜索 Agent。

## 核心架构（精简版）

```text
User Input
  ↓
Agent Harness（Context / Intent Router / Tool Loop / Policy / Schema / Tracing / Evals）
  ↓
Planner（规则解析 + 本地小模型）
  ↓
Search Intent JSON（统一中间层，模型不直接生成查询语法）
  ↓
Tool Registry
  └─ SearchBackend（trait）
       ├─ SpotlightBackend       [macOS 默认 — mdfind / NSMetadataQuery]
       ├─ WindowsSearchBackend   [Windows 默认 — OLE DB SystemIndex]
       ├─ EverythingBackend      [Windows 可选加速 — ES / SDK]
       └─ NativeIndexBackend     [未来]
  ↓
Result Normalizer + Ranker
  ↓
Streaming Results UI（Tauri，跨平台）
```

同一检索栈另有 **headless daemon 形态**（`apps/daemon` 的 `scoutd`，复用 `packages/scout-server`）：以 MCP streamable-HTTP 服务把 hybrid 检索暴露给团队内网的 LLM 客户端（BETA-32）。

详细架构、Search Intent schema 设计、Harness 能力清单见 [docs/local-personal-search-agent-project-plan.md](./docs/local-personal-search-agent-project-plan.md)。

## 阶段路线图

| 阶段 | 时长 | 目标 |
|---|---|---|
| **技术原型** | 1-2 周 | macOS 上跑通：自然语言 → SearchIntent → mdfind → 结果 |
| **MVP** | 3-5 周 | macOS + Windows 双平台 Tauri 应用；三套 SearchBackend；基础 Harness；500 条 evals |
| **Beta** | 8-12 周 | 音乐 metadata / Office/PDF 内容 / OCR；多源合并；安装包开源分发（GitHub Releases + Homebrew / winget / Scoop，未签名安装口径文档化） |
| **1.0** | 4-6 月 | 完整客户端、插件系统、本地活动洞察、隐私/权限 UI、自动更新、跨平台稳定发布 |

## 当前阶段

见 [STATUS.md](./STATUS.md) 顶部。

## 三份关键计划书（不要丢失上下文时跳过）

- [docs/local-personal-search-agent-project-plan.md](./docs/local-personal-search-agent-project-plan.md) — 完整产品/技术计划，跨平台架构主文档
- [docs/Scout知识产权保护计划书.md](./docs/Scout知识产权保护计划书.md) — 商标、域名、第三方授权、Apple/Microsoft 品牌规范（**历史记录**：2026-07-04 开源免费拍板后，商标注册 / 域名采购 / 代码签名部分不再执行；第三方授权台账与品牌使用规范〔不暗示 Apple/MS/voidtools 背书〕仍有效）
- [docs/Scout项目注意事项与风险清单.md](./docs/Scout项目注意事项与风险清单.md) — 搜索后端、隐私、Agent 安全、跨平台分发风险

## 关键技术决策

- **桌面框架**：Tauri 2 + React/TypeScript（首选；Electron 作为备用）
- **本地服务/适配层**：Rust（与 Tauri 同语言，跨平台编译）
- **模型推理**：llama.cpp（macOS Metal / Windows CPU·Vulkan·CUDA），GGUF 格式跨平台共用
- **训练**：MLX / mlx-lm（仅 Mac 训练侧）
- **基座模型**：Qwen2.5-1.5B-Instruct（首版），Qwen3-1.7B 备选
- **索引存储**：SQLite + FTS5（跨平台一致）

## 不做什么（防止范围蔓延）

- 不做云端 AI 搜索。
- **不做分析层**（2026-07-02 定位收敛）：内容关联分析、摘要、比对、起草等"理解/生成"类能力一律不自建——经 **BETA-32 MCP daemon + 外部 LLM（Claude 等）组合**实现，Scout 守住"数据不出门的检索"这一层。评估新特性时，凡属"理解/生成/分析文档内容"的需求引导到 MCP 工作流（ROADMAP BETA-40），不往产品里加。ROADMAP V10-13/15/16 已相应重定性。**2026-07-02 起，定位/范围以本文件为准**；早期计划书（docs/）中涉及摘要、比对、起草、内容关联分析等分析层展望，仅作为历史设计记录，不代表当前自建范围。
- 不做*替代系统搜索的*完整全文搜索引擎（系统搜索仍是默认后端，不从零重建全文索引体系）；**但会在其上叠加一层本地语义召回索引**（embedding 住进 SQLite + 与 FTS5 hybrid 融合），把"按意思 / 跨语言模糊召回"做成差异化主打能力——这是 BETA-26 探针 2026-06-15 验证 GO 后用户选定的"进取档"方向（详 ROADMAP BETA-15B / BETA-26 + go/no-go 备忘）。
- 不做强制依赖 Everything 的方案（Everything 仅作可选加速）。
- 不做商业分发前置（2026-07-04 开源免费拍板）：不注册商标、不购代码签名证书、不注册 Apple Developer；接受 Gatekeeper / SmartScreen 未签名提示，以安装文档 + 包管理器渠道（Homebrew / winget / Scoop）+ 从源码构建缓解。
- 不做 Linux 桌面（架构预留，但短期不投入）。
- 不做删除/批量修改的 Agent 自动执行（MVP 不支持，必须强确认）。
