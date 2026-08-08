# 场景 playbook ③：离职员工材料归档检索

> 共用底座（组件 / 拓扑 / 接入 / audit / 验证清单）见 [README](./README.md)；本篇只写离职归档场景差异。

## 1. 场景画像

- 离职者留下**一整块没人熟悉的语料**：项目文档、交接邮件（含账号清单 / 遗留问题附件）、入职离职手续扫描件、共享盘副本——**语义召回优势最大化的场景**：继任者连"文件大概叫什么"都不知道，只能按意思问；
- 高频提问发生在离职**数月后**（"当时那个接口是怎么鉴权的？"），原作者已不可问；
- 涉 HR 敏感件（保密协议、离职证明扫描）与技术交接件混在一起，**看的人不该是同一拨**；
- 材料天然多**近重复**（本人目录原件 + 共享盘副本 + 迁移盘），找"全部版本"与"别漏"同样重要。

## 2. collection 划分与权限矩阵

**一名离职员工一个 collection**，`subject_kind = "employee"`；HR 敏感件单独成集合与技术件隔离。归档完成即 `read_only = true`。

```toml
[[collections]]
id = "offboarding-lishili-tech"
display_name = "李示例 · 技术交接归档"
subject_kind = "employee"
roots = [
  "/archive/offboarding/lishili/projects",
  "/archive/offboarding/lishili/mailbox",       # 交接邮件 eml
  "/archive/offboarding/lishili/shared-copy",   # 共享盘副本（近重复来源）
]
read_only = true
audit_tags = ["offboarding", "tech-handover"]

[[collections]]
id = "offboarding-lishili-hr"
display_name = "李示例 · HR 手续归档"
subject_kind = "employee"
roots = ["/archive/offboarding/lishili/hr"]     # 保密协议/离职确认扫描件
read_only = true
audit_tags = ["offboarding", "hr-sensitive"]

[[tokens]]
token = "<继任者 token>"
subject = "wang.yangben"              # 继任工程师：只见技术件
collections = ["offboarding-lishili-tech"]

[[tokens]]
token = "<HR token>"
subject = "hr.zhao"                   # HR：只见手续件
collections = ["offboarding-lishili-hr"]

[[tokens]]
token = "<admin token>"
subject = "it.ops"
collections = ["*"]
admin = true
```

技术/HR 分仓是本场景的"信息墙"：继任者搜"保密协议"不会命中（物理隔离），HR 也看不到代码交接细节。

## 3. 示例 query（10 条，继任者/HR 视角）

| # | query | 考验点 |
|---|---|---|
| 1 | 离职同事发的交接安排邮件 | eml 正文 + subject 映射标题 |
| 2 | 走之前留下没修完的问题清单邮件 | 意图描述（"没修完"↔"未结缺陷"） |
| 3 | 交接时给的数据库账号权限清单 | 邮件**附件**内容命中 |
| 4 | 对外接口怎么对接怎么鉴权的说明文档 | 按用途找、不知文件名 |
| 5 | 遗留 bug 的复现步骤明细 | 附件明细 |
| 6 | 月度报表口径是怎么定义的说明 | 领域术语 ↔ 口语 |
| 7 | 员工入职时签的保密协议 | 扫描件 OCR（HR 集合） |
| 8 | Lighthouse 项目的架构设计 | **跨语言别名**（文档里写"灯塔项目"） |
| 9 | Kunpeng 结算系统的运维手册 | 跨语言别名反向（英文 query 中文文档） |
| 10 | 离职交接清单原件和共享盘副本 | **近重复**全召回、别漏版本 |

## 4. LLM 工作流示例（继任者接手）

```text
继任者：我下周接鲲鹏结算的值班，把交接材料里值班要知道的事整理成 checklist。

Claude（经 MCP）：
  1. list_collections() → offboarding-lishili-tech
  2. search("鲲鹏 结算 值班")
  3. search("Kunpeng on-call runbook")          ← 别名补召回
  4. search("遗留 问题 缺陷")                    ← 值班要背的坑
  5. Read 命中的交接邮件 / 运维手册 / 遗留问题附件
  6. 产出 checklist，每条标出处；对"账号清单"类命中提示：
     口令不在归档里（走密码保管系统），只有账号名与用途
```

数月后随时可重复这套流程——归档 + 索引冻结保证答案可复现（同样的问题得到同样的出处）。

## 5. 场景注意

- **近重复不去重**：搜索按相关性返回，同组副本都会出现——"哪份是权威原件"由归档目录约定（如 `projects/` 优先于 `shared-copy/`），LLM 工作流里可要求模型按 path 前缀标注版本来源；
- **敏感边界**：账号清单类文档入索引没问题（归档规范应保证**不含口令明文**——Scout 不做内容脱敏，入档前的清洗是归档流程职责）；
- **批量装载**：新离职归档 = 建目录 → TOML 加 collection + token → 重启 daemon（或下个维护窗口统一加）；单集合材料后补用 `POST /admin/reindex?collection=<id>`；
- **保留期到期**：从 TOML 移除 collection + 删除 `data_dir/collections/<id>/` 即完成索引侧清除（原始材料的销毁走档案管理流程）。
