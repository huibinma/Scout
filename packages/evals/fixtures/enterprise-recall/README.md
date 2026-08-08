# 企业场景评测语料 fixture（BETA-41）

- dataset_name: enterprise-recall
- version: v1
- generation_method: LLM 生成虚构企业文档 + 复核，全合成零 PII
- privacy_review_status: reviewed —— 无真实人名/公司/邮箱/账号/案号/路径
- created_at: 2026-07-02
- reviewer: Claude Code
- corpus: corpus.json（104 篇合成文档，zh 90 / en 14；三场景 lawfirm 34 / audit 35 / offboarding 35）
- cases: cases.json（50 条 graded 相关性，五桶各 10）
- vectors: vectors.json（`--fixture-set enterprise --embed` 生成；未提交前 gate skip）
- baseline: baseline.json（bootstrap 后生成提交）
- files/: 文件层 fixture（扫描 PDF / eml / 近重复副本，驱动 indexer 真实提取管线；见下）
- spec: 2026-07-02-beta-41-enterprise-eval-fixture-design.md

服务对象：BETA-35（扫描 PDF 子集命中率）/ BETA-37（邮件与附件子集）/ BETA-38（近重复 dup_group + 语义召回不回归）/ BETA-40（三场景示例 query 来源）。

## 三场景

| scenario | 数量 | 虚构主体与实体 |
| --- | --- | --- |
| lawfirm（律所案件卷宗） | 34 | 北岭机械（Northridge Machinery）诉 蓝湾贸易（Bluebay Trading）买卖合同纠纷 |
| audit（企业内部审计取证） | 35 | 猎户座采购项目（Project Orion）× 晨星办公用品（Morningstar Office Supplies） |
| offboarding（离职员工材料归档） | 35 | 李示例离职交接：灯塔项目（Lighthouse）+ 鲲鹏结算系统（Kunpeng Settlement System） |

每场景都有一份「名称对照」文档（e00021 / e00051 / e00080）承载中英文别名映射，crosslang-alias 桶的检索既考语义配对也考对照文档召回。

## 桶分布（cases.json）

| 桶 | 条数 | 含义 | 验收挂钩 |
| --- | --- | --- | --- |
| scanned-pdf | 10 | ground truth 是扫描件 OCR 文本（body 带轻度拟真识别误差，如个别字识别为「口」），query 描述内容 | BETA-35 |
| email | 10 | ground truth 是邮件（正文 + from/subject headers 文本化） | BETA-37 |
| attachment | 10 | ground truth 是邮件附件文档，query 只描述附件内容不提邮件 | BETA-37 |
| crosslang-alias | 10 | 实体中英文别名不同，query 用另一语言的别名指称 | 三场景共同 |
| near-dup | 10 | ground truth 是一组近重复副本（`dup_group`），主本 grade 3、副本 grade 2——检索该召回、去重不该丢 | BETA-38 |

## corpus 扩展字段（相对 semantic-recall）

- `scenario`: `lawfirm | audit | offboarding`
- `doc_type`: `scanned | email | attachment | plain`
- `dup_group`: 近重复组 id（同组 = 同一材料的近似副本；共 10 组，每组 2-3 篇）

三字段均可选（serde default），semantic-recall 主 fixture 不受影响。

## 拟真 OCR 特征的度

scanned 文档的 body 是"提取后文本"：干净可读为主 + 少量字符级误识（「口」占位、个别形近字），**必须**能过 BETA-33 双层门槛（meaningful_ratio ≥ 0.6）——语料若自带乱码会被生产管线 A 层拦掉，评测失真。

## 隐私自查清单（commit 前）

- [x] corpus/cases 无真实人名/公司/邮箱/电话/精确金额/真实路径/真实案号
- [x] 人名一律「李示例 / 王样本」式占位；公司/项目均为虚构中英文配对
- [x] 邮箱一律 `@example.com` / `@example.org`（完整性门机器校验）
- [x] 金额一律"约整数 / 约三成"占位，无精确大额数字；单号/流水号带「示例」字样
- [x] doc_id / case id 唯一；dup_group 组内 ≥ 2 篇（机器校验）

## 跑法

```bash
# 完整性测试（常跑，无需向量）
cargo test -p scout-evals --test enterprise_recall_fixtures_integrity

# 评测（需先 bootstrap vectors.json）
cargo run -p scout-evals --bin semantic_quality -- --fixture-set enterprise

# 生成向量（一次，需 embedding 模型 + cmake/llama-cpp 构建环境；Mac Metal 或装了 VS Build Tools 的 Windows）
cargo run -p scout-evals --bin semantic_quality --features semantic-recall -- --fixture-set enterprise --embed --model models/bge-m3-q8_0.gguf

# 写 baseline（bootstrap 收尾）
cargo run -p scout-evals --bin semantic_quality -- --fixture-set enterprise --write-baseline
```

vectors.json / baseline.json 未提交时，enterprise gate（`tests/enterprise_recall_gate.rs`）保持 skip（与 semantic-recall Phase D 同款语义）。

**bootstrap 状态（2026-07-02）**：待跑——Windows 主力机无 cmake/VS Build Tools（BETA-31-v2 同一阻塞），建议在 Mac 上按上述命令一次性生成后提交；模型选 `bge-m3-q8_0`（与 semantic-recall v5 baseline 对齐；未来 STATUS 待办「baseline 切 embeddinggemma」落地时两套 fixture 一起换）。

## files/ 文件层（驱动 indexer 真实提取管线）

| 目录 | 内容 | doc_id 对应 |
| --- | --- | --- |
| files/lawfirm/ | 扫描 PDF：判决书 ×3（近重复组 g-law-01、不同扫描质量）、庭审笔录、合同 ×2 页；文本层 PDF 对照（英文 Helvetica） | e00001 / e00002 / e00005 / e00006 / e00007 / e00019 |
| files/audit/ | eml ×4（e00035 含合同附件 part）+ 扫描凭证 PDF（发票 / 入库验收单） | e00035 / e00036 / e00037 / e00050 / e00041 / e00043 |
| files/offboarding/ | eml ×2（含附件 part）+ 扫描 PDF（保密协议 / 交接确认单）+ 近重复副本（md/txt 两组） | e00074 / e00093 / e00078 / e00079 / e00085 / e00086 / e00096 / e00097 |

文件名即 `<doc_id>-<slug>.<ext>`，正文与 corpus.json 对应 doc_id 同文。扫描 PDF（image-only，DCTDecode）与 eml 由 [scripts/gen-enterprise-file-fixtures.ps1](../../../../scripts/gen-enterprise-file-fixtures.ps1) 一次性生成后入仓（生成物为准，换机重跑字体渲染有像素差、OCR 结果可能漂移）；近重复 md/txt 手写入仓。合计 9 份扫描 PDF（12 页、约 1MB）+ 1 份文本层 PDF + 6 封 eml + 4 份近重复文本。

端到端测试：`cargo test -p scout-indexer --test real_pdf -- --ignored`（rasterize+OCR 需 pdftoppm 与 OCR 引擎装机；`is_scanned_pdf` 判定与文本层对照不需要）。
