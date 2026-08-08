# 场景 playbook ①：律所案件卷宗检索

> 共用底座（组件 / 拓扑 / 接入 / audit / 验证清单）见 [README](./README.md)；本篇只写律所场景差异。

## 1. 场景画像

- 卷宗按**案件**归档：起诉状、判决书、庭审笔录、证据、往来函件、和解草稿——多为**扫描件 PDF**（法院文书、盖章签署页）+ 邮件（对方律师 / 客户往来）+ Office 文档；
- 接手律师 / 复核合伙人**不熟悉原承办人的归档习惯**（文件名如 `扫描件_20240315.pdf`，按名字找等于没找）；
- **信息墙（ethical wall）是硬要求**：代理 A 案的律师不得接触 B 案卷宗，尤其存在利益冲突隔离时；
- 检索行为本身要**可留痕**（合规复核、冲突调查时回答"谁看过什么"）。

## 2. collection 划分与权限矩阵

**一案一 collection**，`subject_kind = "case"`；结案归档后置 `read_only = true`。

```toml
[[collections]]
id = "case-2026-blueharbor"
display_name = "蓝湾贸易合同纠纷案（一审）"
subject_kind = "case"
roots = ["/archive/cases/2026-blueharbor"]
read_only = true                      # 已结案冷冻
audit_tags = ["litigation", "contract-dispute"]

[[collections]]
id = "case-2026-northfield"
display_name = "北原并购尽调项目"
subject_kind = "case"
roots = ["/archive/cases/2026-northfield"]
read_only = false                     # 进行中，材料仍在增
audit_tags = ["ma", "due-diligence"]

# —— 权限矩阵：按承办团队授权，信息墙即"不出现在列表里" ——
[[tokens]]
token = "<zhang 的 token>"
subject = "zhang.san"                 # 蓝湾案承办律师
collections = ["case-2026-blueharbor"]

[[tokens]]
token = "<li 的 token>"
subject = "li.si"                     # 并购组，与蓝湾案对方当事人有利冲、严禁接触
collections = ["case-2026-northfield"]

[[tokens]]
token = "<admin token>"
subject = "km.admin"                  # 知识管理员：全权 + reindex/audit
collections = ["*"]
admin = true
```

信息墙语义：li.si 的 `list_collections` **看不到**蓝湾案存在；显式猜 id 请求 → `access denied`（未知与未授权同文案，不泄漏存在性）+ audit denied 记录。

## 3. 示例 query（10 条，接手律师视角）

| # | query | 考验点 |
|---|---|---|
| 1 | 一审法院最后是怎么判的 | 扫描版判决书 OCR + 语义（"怎么判"↔"判决如下"） |
| 2 | 开庭时双方争交货时间责任的笔录 | 扫描版庭审笔录、按争点找 |
| 3 | 催对方赶紧交货不然要追责的律师函 | 口语描述 ↔ 函件正式措辞 |
| 4 | 对方律师提调解方案的邮件 | eml 正文检索 |
| 5 | 客户说和解能接受到什么程度的邮件 | 意图描述、无关键词重合 |
| 6 | 和解协议的草稿 | 邮件**附件**内容命中 |
| 7 | 迟延损失是怎么算出来的那张表 | 附件表格、"那张表"式模糊指称 |
| 8 | Northridge 那个案子的一审判决 | **跨语言别名**（中文卷宗、英文案件代称） |
| 9 | Bluebay 发过来谈调解的邮件 | 跨语言别名 + 邮件 |
| 10 | 一审判决书所有归档版本 | **近重复**（原件 / 复印再扫 / 移动盘副本）该全召回 |

## 4. LLM 工作流示例（Claude Code 会话）

```text
律师：帮我梳理蓝湾案里对方在交货时间问题上的立场变化，按时间排。

Claude（经 MCP）：
  1. list_collections() → 确认可检索 case-2026-blueharbor
  2. search("交货时间 责任 争议", collections=["case-2026-blueharbor"])
  3. search("对方律师 调解 交货")           ← 换角度补一轮召回
  4. 对命中的笔录扫描件 / 律师函 / 邮件逐个 Read(path)
  5. 综合作答，每条立场标注出处（文件名 + 页码/邮件日期）
```

要点：**综合、比对、起草是 Claude 做的，Scout 只负责"找得到 + 有出处"**。要求模型"每个结论给出处 path"，即可把回答锚回卷宗原文（取证可复核）。

## 5. 场景注意

- **扫描件质量**：命中预览带「扫描版 · N 页」与失败页列表；OCR 失败页不静默丢（复核时人工补看失败页）；
- **结案冻结**：`read_only = true` 后误触 reindex 返 409——卷宗封存状态与索引状态一致，audit 亦留 reindex 记录；
- **离职/换岗**：从 `[[tokens]]` 删除对应条目并重启 daemon 即吊销（token 无自过期，轮换靠配置管理）；
- **利冲调查**：`GET /admin/audit` 按 subject 过滤即得"某人查过哪些案件、搜过什么"。
