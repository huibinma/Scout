# 语义召回质量评测集（BETA-15B-6）

- dataset_name: semantic-recall-quality
- version: v1
- generation_method: LLM 生成虚构文档 + 复核，全合成零 PII
- privacy_review_status: reviewed —— 无真实人名/公司/邮箱/财务/路径
- created_at: 2026-06-21
- reviewer: Claude Code
- corpus: corpus.json（124 篇合成多语言文档，zh 62 / en 62；BETA-15B-6 v2 扩 +7 doc、v3 再扩 +9 doc）
- cases: cases.json（78 条 graded 相关性，5 桶；BETA-15B-6 v2 扩 content-not-name 桶 +9 case、v3 再扩 +10 case）
- vectors: vectors.json（--embed 生成，合成文本 embedding，无 PII；尚未提交，Phase D 用户 bootstrap）
- baseline: baseline.json（hybrid 分桶锚点，gate 比对；Phase D 生成）
- vectors-multi-model: vectors-{qwen3-0.6b,bge-m3,qwen3-8b}.json（BETA-15B-7 v4 模型跨族 + 同族最大档探针、参 baseline 报告 v4 节；与 gate 守护对象 `vectors.json` 解耦、双轴均落 Branch IV-infra；本 cycle 实际产出 = model-runtime / llama-cpp-4 layer infrastructure 诊断）

## 桶分布（cases.json）

| 桶 | 条数 | 含义 |
| --- | --- | --- |
| synonym | 12 | query 用同义改述指向同一文档（与标题词面不重合） |
| concept | 12 | 概念/主题跳跃，高抽象描述特定内容 |
| crosslang | 13 | 中→英 或 英→中，配对主题、词面不共享 |
| content-not-name | 30 | query 描述正文要点而非文件名（BETA-15B-6 v2 扩 11→20、v3 再扩 20→30、T\* 真水位校验、bake T\* v1 0.60 → v2 0.70 → v3 0.70 鲁棒不动 Branch A 命中）|
| exact-name | 11 | query = 合成文档精确标题（守护桶） |

## 跨语言配对主题（crosslang 桶用）

每组同一虚构主题各出一篇 zh + 一篇 en，两篇正文用各自语言、关键词面不共享：

| 主题 | zh doc_id | en doc_id |
| --- | --- | --- |
| 年假与远程办公政策 | s00001 | s00002 |
| 订单服务分布式缓存设计 | s00003 | s00004 |
| 新员工入职第一周 | s00005 | s00006 |
| 深度专注 / 心流读书笔记 | s00007 | s00008 |
| 第三方依赖许可证合规 | s00009 | s00010 |
| 首页加载性能优化复盘 | s00011 | s00012 |
| 差旅报销规定 | s00013 | s00014 |
| 对外接口设计约定 | s00015 | s00016 |
| 季度部门预算编制 | s00017 | s00018 |
| 信息安全意识培训 | s00019 | s00020 |
| 高效会议纪要写法 | s00021 | s00022 |
| 客户支持常见问题手册 | s00023 | s00024 |
| 海滨周末两日游计划 | s00025 | s00026 |
| 搜索结果分组功能需求 | s00027 | s00028 |
| 技术债盘点与偿还 | s00029 | s00030 |
| 团队责任边界 / 不甩锅协作机制 | s00031 | s00032 |
| 项目周报模板 | s00033 | s00034 |

共 17 组配对（≥15）。

## 隐私自查清单（commit 前）

- [x] corpus/cases 无真实人名/公司/邮箱/电话/精确薪资/真实路径
- [x] 人名/公司均为明显虚构占位（如「李示例」「Acme」「sample@example.com」未出现真实主体）
- [x] 金额一律约整数占位（「约一成」「人均约整数」），无 7 位以上精确金额
- [x] 跨语言桶是虚构配对主题、词面不共享
- [x] doc_id / case id 唯一

## 跑法

- 评测（读缓存，需先有 vectors.json）：`cargo run -p scout-evals --bin semantic_quality`
- 生成向量（一次，需模型，Phase D）：`cargo run -p scout-evals --bin semantic_quality --features semantic-recall-metal -- --embed`
- 写 baseline：`... --write-baseline`
- 完整性测试（常跑，无需向量）：`cargo test -p scout-evals --test semantic_quality_fixtures_integrity`

## Phase D bootstrap（一次性，需 Mac + 模型 + Metal）

> 评测的向量缓存与 baseline 需先在有 embedding 模型的机器上生成一次并提交，回归门才从 skip 转常跑。
> 此后 CI/任意机器读缓存即可确定性跑，无需模型。

**前提**：embedding 模型在 `models/qwen3-embedding-0.6b-q8_0.gguf`（同 BETA-26）；已装 cmake（编 llama-cpp）。

```bash
cd <repo 根>
# ① 生成合成向量缓存（唯一需模型的一步，108 doc + 59 query）
cargo run -p scout-evals --bin semantic_quality --features semantic-recall-metal -- --embed
#   → fixtures/semantic-recall/vectors.json（合成文本 embedding，无 PII，可提交）

# ② 写 baseline + 看分桶 × 三臂表（不需模型，读缓存确定性跑）
cargo run -p scout-evals --bin semantic_quality -- --write-baseline   # → baseline.json
cargo run -p scout-evals --bin semantic_quality                       # 打印表格

# ③ 回归门此刻应从 skip 转真跑
cargo test -p scout-evals --test semantic_quality_gate                 # 应 1 passed（真断言）
```

**看 ② 的表时确认（评测是否"测出语义价值"的铁证）**：
- `crosslang` 桶：FTS_R ≈ 0、HYB_R 显著 > 0（中文 query 召回英文文档，FTS5 trigram 结构性打不中，唯语义能召回）。
- `exact-name` 桶：HYB_R ≥ FTS_R（语义不拖垮精确查询，守护成立）。
- `OVERALL`：HYB_R ≥ VEC_R、≥ FTS_R。

**baseline 报告**：把表格 + 配置（weight=2.0/floor=0.30/model_id）+ 与 BETA-26 真实集（crosslang +100pp）的定性对照写进本 README。记两条待跟进 Minor：① `EVAL_SIMILARITY_FLOOR=0.30` 是生产常量字面量复制（生产 `DEFAULT_SIMILARITY_FLOOR` 为 `pub(crate)` 无法导入），调优 floor 时须人工对齐或上提共享 crate；② 语料 108 < 计划 150，crosslang 区分度不足时优先扩该桶。

**提交激活**：
```bash
git add fixtures/semantic-recall/vectors.json fixtures/semantic-recall/baseline.json
git commit -m "BETA-15B-6 Phase D：合成向量缓存 + baseline + 报告（激活回归门）"
```

**现实校准锤（可选，对照趋势是否同向）**：`cargo run -p spike-retrieval --bin run-retrieval --features metal`（真实 home 数据，gitignored）。合成集若与真实集背离则警示合成集失真，记为下一 cycle 扩量信号。

**下一 cycle = 真正的质量调优**：据 baseline 调 RRF 权重（`result-normalizer/lib.rs` `DEFAULT_SEMANTIC_WEIGHT`）/ 下限精校 / 试更大 embedding 模型，用本评测度量。

## v5 升级说明（BETA-15B-10、2026-06-26）

- **dataset**：81 cases / 127 docs（新增 c079/c080/c081 + s00125/s00126/s00127 三条长文本）
- **model**：`bge-m3-q8_0`（与桌面默认对齐、BETA-15B-7-v2 切到生产）
- **cosine_threshold bake**：v3 T*=0.70 → v5 T*=0.70（保 v3 字面值不变、Branch B 变体保守选）
- **baseline.json**：守 bge-m3 + 81/127 + T*=0.70
- **vectors.json**：Mac Metal Q8_0 一次性产物、SHA256 `4f0de346b581d58d…`（Metal 浮点抖动注意：换 Mac 跑 SHA 不绝对 byte-equal、未来重跑需重 bake baseline）
- **长文本 case 设计意图**：覆盖 > 512 token BERT encode path、与 desktop indexer 真实路径对齐（历史 hotfix PR #16 已修 n_ubatch panic，本 cycle 进一步把 evals 路径与 desktop 路径对齐）；token 数 [513, 2048] 安全区间（hotfix n_ubatch=2048 上限内）
- **evals binary embed 路径解除 1200 char 截断**：`packages/evals/src/bin/semantic_quality.rs:165` 原 `text.chars().take(1200)` 删除、与 desktop indexer 等价
- 详见上方 v5 数据集节

## v6 升级说明（BETA-15B-11、2026-06-27）

**单点跨厂探针 cycle、Branch I-a GO ⭐⭐**：本 cycle 验证 **EmbeddingGemma-300M**（Google 2025-09、`gemma-embedding` arch / dim 768 / Mean pooling / 313 MB Q8_0 / 100+ lang）能否冲过 BETA-15B-10 v5 留下的 crosslang 字面 0.700 gap。结论 = **双过 spec 字面**（OVERALL 0.900 / crosslang 0.725、prefix mode sweep best）。

**新增 reference snapshot**（**入仓、不替换 vectors.json 主文件**）：
- `vectors-embeddinggemma-300m-no-prefix.json`：dim 768、81 query + 127 doc、L2 mean=1.0
- `vectors-embeddinggemma-300m-prefix.json`：同上、HF model card 契约（query 包 `task: search result | query: ...` / doc 包 `title: none | text: ...`）

**新增 binary flag**：`--prefix-mode {none, standard}`、default `none` 守 BETA-15B-10 及之前所有 cycle 向下兼容。本 cycle 双 mode 对照实验把「prefix 契约」对最终分数的真实影响切割开来。

**主文件不动**：本 cycle 范围 = 评测探针 only、不动 `vectors.json` / `baseline.json` / `cosine_threshold`、不动桌面 wiring。Follow-up cycle BETA-15B-11-v2 bake 推生产时再 rewrite 主文件。

**关键数据**（vs v5 bge-m3 baseline T=0.70）：
- OVERALL: 0.864 → embeddinggemma prefix sweep best **0.900**（+0.036 ⭐⭐）
- crosslang: 0.686 → embeddinggemma prefix sweep best **0.725**（+0.039 ⭐⭐）
- content-not-name: 0.869 → embeddinggemma prefix sweep best 0.928（+0.059）
- exact-name: 1.000 = 1.000 ✓

**prefix 契约价值（命题 2 答案）**：standard prefix 在所有桶上 +0.009 ~ +0.025 加成、但 **no-prefix mode 单独也已经双过字面**（OVERALL 0.882 / crosslang 0.716）→ **prefix 是 ROI 加分项、不是 GO 的必要条件**。bake 推生产时不需要在 model-runtime 层加 prefix API。

- 详见上方 v6 数据集节
