# packages/intent-parser

自然语言 → Search Intent JSON。

**状态**：未开始。技术原型阶段第一项工作。

## 计划职责

- 规则解析器：覆盖常见模式（扩展名、时间、大小、路径、关键词、排序、limit）
- 本地小模型调用（兜底，仅在规则不足时触发）
- JSON Schema 校验 + 修复重试
- 多语言支持：中文、英文、中英混合
- 时间表达只输出语义（yesterday / last_7_days），由程序按本地时区/locale 计算具体范围

## 关键原则

- 模型**不直接生成任何后端查询语法**，只输出 SearchIntent JSON
- 规则优先、模型兜底，避免对模型 100% 依赖

详细设计见 [docs/search-intent-schema.md](../../docs/search-intent-schema.md)（占位）和 [docs/local-personal-search-agent-project-plan.md §5 §7](../../docs/local-personal-search-agent-project-plan.md)。
