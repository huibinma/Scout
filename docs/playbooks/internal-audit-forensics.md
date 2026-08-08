# 场景 playbook ②：企业内部审计取证检索

> 共用底座（组件 / 拓扑 / 接入 / audit / 验证清单）见 [README](./README.md)；本篇只写审计场景差异。

## 1. 场景画像

- 取证材料**跨格式散落**：扫描凭证（发票 / 银行回单 / 入库验收单）、审批与举报**邮件**（含合同 / 报价对比表**附件**）、台账表格——线索藏在"凭证链"里而非单一文档；
- 审计员是**外来者**：不熟悉被审计部门的文件组织，只知道"应该存在一封暂缓付款的邮件"这类**事实描述**；
- **审计过程本身要经得起审计**：查了什么、什么时候查的、有没有越权——`audit.jsonl` 是本场景的一等公民而不只是合规装饰；
- 项目制：一次审计一个语料快照，结项后封存。

## 2. collection 划分与权限矩阵

**一个审计项目一个 collection**，`subject_kind = "audit_project"`；取证快照落定后即置 `read_only = true`（**证据固定**：审计期间材料不应再变，索引冻结与之对齐）。

```toml
[[collections]]
id = "audit-2026-procurement"
display_name = "2026 行政采购专项审计"
subject_kind = "audit_project"
roots = [
  "/archive/audit/2026-procurement/vouchers",   # 凭证扫描件
  "/archive/audit/2026-procurement/mailbox",    # 调取的邮箱导出（eml）
  "/archive/audit/2026-procurement/ledgers",    # 台账/合同
]                                               # 多 root 归组成一个取证边界
read_only = true
audit_tags = ["audit", "procurement", "q2-2026"]

[[tokens]]
token = "<auditor 的 token>"
subject = "auditor.wang"              # 主审
collections = ["audit-2026-procurement"]

[[tokens]]
token = "<reviewer 的 token>"
subject = "reviewer.chen"             # 复核（只读检索，同集合）
collections = ["audit-2026-procurement"]

[[tokens]]
token = "<admin token>"
subject = "audit.it"                  # IT 支持：装载新项目快照 + 导出留痕
collections = ["*"]
admin = true
```

要点：审计员**不给 admin**——取证人不应能触碰 reindex（改变索引状态）或读全量 audit（看到他人检索行为），职责分离由 `admin` 标志强制。

## 3. 示例 query（10 条，审计员视角）

| # | query | 考验点 |
|---|---|---|
| 1 | 供应商开过来的增值税发票 | 扫描凭证 OCR |
| 2 | 采购首期款打款的银行凭证 | 扫描回单、口语指称 |
| 3 | 设备到货验收数量不够的那张单子 | 事实描述 ↔ 验收单措辞 |
| 4 | 匿名举报采购和供应商有私人关系的邮件 | eml、举报线索定位 |
| 5 | 领导出差没签字事后补审批的邮件 | 流程异常线索、无关键词重合 |
| 6 | 财务发现付款收款账户不对暂缓打款的邮件 | 凭证链关键节点 |
| 7 | 审批邮件里附的那份采购合同 | 邮件**附件**内容命中（query 不提"邮件"也应召回） |
| 8 | 几家供应商报价对比打分的表 | 附件表格 |
| 9 | the email where the vendor sent us their quotation | 英文 query 中文语料环境 |
| 10 | Morningstar 开的发票 | **跨语言别名**（中文台账写"晨星办公用品"） |

## 4. LLM 工作流示例（凭证链取证）

```text
审计员：围绕猎户座采购项目，把"审批 → 合同 → 付款 → 验收"的凭证链拉出来，
        标出哪一环有异常。

Claude（经 MCP）：
  1. search("猎户座 采购 审批", collections=["audit-2026-procurement"])
  2. search("Orion procurement approval")      ← 跨语言别名补一轮
  3. search("付款 账户 暂缓") / search("验收 数量 不符")
  4. 逐个 Read(path)：审批邮件（含合同附件）、付款申请、银行回单扫描、验收单扫描
  5. 按时间线重建凭证链，异常环节（账户变更未附验收单 / 数量差异照常付款）
     逐条给出处 path + 原文摘录
```

结论必须**逐条带出处**——审计底稿要求每个异常指向可复核的原始凭证；Scout 保证"找得到、有 path、有留痕"，判断是审计员与模型协作的产物。

## 5. 场景注意

- **留痕即底稿**：结项时 `GET /admin/audit?tail=1000` 导出归入审计工作底稿——检索过程可复盘（查过什么、没查什么）；
- **query 明文默认开**：本场景正是"记明文"的受益方；若涉个人隐私专项，改 `log_query = false` 前先过合规评审；
- **邮箱导出**：`.eml` 直接入索引（正文 + headers + 附件递归提取）；`.pst` 不支持，需先用邮箱工具导成 eml；
- **失败页清单**：扫描凭证 OCR 失败页有记录（`document_failed_pages`），取证复核时人工补看，不会静默漏证。
