# 企业三场景原始测试材料

> 本目录是依据当时的企业场景测试方案生成的原始测试语料包。
> 所有主体、公司、项目、邮箱、金额、案号、账号均为虚构或占位；不得替换为真实敏感信息后入仓。

## 目录结构

```text
enterprise-scenarios-raw/
  configs/                         # daemon collection/token 示例配置
  expected/                        # query 与预期命中说明
  lawfirm/                         # 律所案件卷宗场景
    case-2026-blueharbor/          # 授权正例案件
    case-2026-northfield/          # 信息墙干扰案件
  audit/
    audit-2026-procurement/        # 授权正例审计项目
    audit-2025-facilities/         # 负样本/越权干扰项目
  offboarding/
    lishili-tech/                  # 离职员工技术交接集合
    lishili-hr/                    # 离职员工 HR 敏感集合
    other-employee-tech/           # 越权/负样本员工集合
```

## 使用方式

1. 直接把各场景目录作为 `scoutd` collection roots。
2. `scan-source/*.txt` 表示扫描件的源文本，可后续批量渲染成 image-only PDF；当前以文本保留，便于人工审阅。
3. `mailbox/*.eml` 是邮件原始材料；附件内容以 `attachments/` 下独立文件表示。
4. `duplicates/` 下是近重复副本，用于验证副本召回、去重展示与 path 还原。
5. `expected/queries.tsv` 给出业务 query、场景、授权主体、预期命中文件和测试目的。

## 覆盖关系

| 场景 | 覆盖重点 |
|---|---|
| 律所案件卷宗 | 扫描判决书、庭审笔录、律师函、和解邮件、附件、跨语言别名、近重复、信息墙 |
| 内部审计 | 发票/银行回单/验收单、审批邮件、合同附件、报价对比表、异常事实、职责分离 |
| 离职员工 | 交接邮件、运维手册、接口鉴权、遗留 bug、账号用途清单、HR/技术隔离、共享盘副本 |

