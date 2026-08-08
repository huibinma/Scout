# docs/ — 文档导航索引

Scout 详细设计、规划、审阅与历史记录。

> **重要：本目录全部是「按需查阅」，不进会话必读流程**（会话必读集见 [CONVENTIONS.md §2](../CONVENTIONS.md)：PROJECT / STATUS / CONVENTIONS + 定向读取 ROADMAP）。
> 因此 docs/ 体量大小**不影响每次开会话的上下文加载**——需要某份历史设计/计划/审阅时再单独打开。

## 主计划书（定稿，方向性疑问优先翻这里）

- [local-personal-search-agent-project-plan.md](./local-personal-search-agent-project-plan.md) — 完整产品/技术计划（跨平台架构主文档）
- [Scout知识产权保护计划书.md](./Scout知识产权保护计划书.md) — 商标、域名、第三方授权、Apple/Microsoft 品牌规范
- [Scout项目注意事项与风险清单.md](./Scout项目注意事项与风险清单.md) — 搜索后端、隐私、Agent 安全、跨平台分发风险
- [product-positioning-and-competitors.md](./product-positioning-and-competitors.md) — 市场定位与竞品

## 核心设计文档

- [search-intent-schema.md](./search-intent-schema.md) — **Search Intent JSON schema v1.0**（含验收用例）
- [search-backend-trait.md](./search-backend-trait.md) — **SearchBackend Trait v0.1**（翻译规则、stub 后端）
- [harness-design.md](./harness-design.md) — Agent Harness 接口与控制流
- [privacy-security.md](./privacy-security.md) — 隐私边界与安全策略
- [schema/search-intent.v1.json](./schema/search-intent.v1.json) — schema 的机器可校验 JSON

## 安装 / 平台

- [windows-setup.md](./windows-setup.md) — Windows 环境准备（Rust / Tauri / 模型 GGUF 获取 + sha256）
- [third-party-licenses.md](./third-party-licenses.md) — 第三方组件清单与许可（随依赖引入实时更新）

## 测试 / 工作清单

- [manual-test-scenarios.md](./manual-test-scenarios.md) — 真机 UI 手测场景（按特性组织，用户驱动）
- [mac-session-todo.md](./mac-session-todo.md) — **只能/最适合在 Mac（Apple Silicon + Metal）上跑**的任务清单，切到 Mac 会话时照做（§2 双平台 evals / §3 Vision OCR + bundle / §4 DMG 签名仍 pending；§1 LLM fallback 实验已核销，被 BETA-23/24 覆盖）

## 历史记录（按需，体量大，勿全量加载）

- **[session-logs/](./session-logs/)** — STATUS.md 的历史会话日志归档。STATUS 瘦身后，更早的会话日志滚动到此（含截至 2026-06-03 的完整 78 条快照 + 2026-06 滚动归档）。查"某个特性当时怎么做的"来这里。其中 [gemini-mvp-15-16-summary.md](./session-logs/gemini-mvp-15-16-summary.md) 是 Gemini 在 MVP-15/16 的独立会话总结。
- **[reviews/](./reviews/)** — 阶段性审阅与出场报告（proto-exit / mvp-exit / parser 各版本 / bake-off / fallback evals 等）。查"某阶段验收结论 / 评测数据"来这里。
- **[superpowers/specs/](./superpowers/specs/)** — 各特性的设计 spec（brainstorming 后、实现前的设计快照）。文件名格式 `YYYY-MM-DD-<特性>-design.md`。
- **[superpowers/plans/](./superpowers/plans/)** — 各特性的实现 plan（task 拆分）。文件名格式 `YYYY-MM-DD-<特性>.md`。

> specs / plans 是历史实现过程的记录，**不是当前状态的单一信源**——当前状态看 [STATUS.md](../STATUS.md)，任务地图看 [ROADMAP.md](../ROADMAP.md)。它们随特性完成而沉淀，保留用于追溯"为什么这么设计"。
