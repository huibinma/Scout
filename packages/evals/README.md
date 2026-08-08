# packages/evals

固定评测集与评测脚本。

## 评测工具 (PROTO-08)

用于验证 `scout-intent-parser` 的解析准确率。

### 用法

```bash
# 跑全 50 条 fixture，输出报告
cargo run -p scout-evals --bin evals

# 跑 MVP-25 v0.5 的 500 条 fixture
cargo run -p scout-evals --bin evals -- --fixtures v0.5

# 跑 BETA-13 v0.9 的 1000 条 fixture（v0.5 500 + 覆盖驱动 500）
cargo run -p scout-evals --bin evals -- --fixtures v0.9

# 跑特定一条（fail 用例复现）
cargo run -p scout-evals --bin evals -- --case 15

# 输出 JSON 报告
cargo run -p scout-evals --bin evals -- --json > report.json

# 仅看失败 (fail 与 partial)
cargo run -p scout-evals --bin evals -- --only-failures
```

### 判定逻辑

- **Pass**: variant 匹配且所有字段值一致。
- **Partial**: variant 匹配，但部分字段（如关键词、时间、排序等）不一致。
- **Fail**: variant 不匹配（例如预期是 FileSearch，实际解析成了 Clarify）。

字段比对的既定豁免（judge 侧口径，均有拍板记录）：

- **`language`** 不参与严格匹配（2026-07-04 拍板：v0.5 标注自身口径矛盾、产品面仅影响
  location hint 语种；分语言统计仍按标注分桶，§6.2 语言子集指标不受影响）。
- **Clarify `question`** 文案完全忽略（v0.5 起：`reason` 已编码语义，question 是本地化呈现）。
- **Clarify `options`** 只校验结构（都是数组或都是 null），不比长度与内容。

## 覆盖驱动评测集 v0.9 (BETA-13)

`fixtures/v0.9/` = v0.5（500，逐字保留为回归锚点）+ `coverage-cases.json`（500，覆盖驱动手标 ground-truth）。
设计/baseline/gap 清单见 [fixtures/v0.9/README.md](./fixtures/v0.9/README.md)。

```bash
# 分片 → coverage-cases.json（确定性汇编）
cargo run -p scout-evals --bin fixtures -- assemble-coverage
# v0.5 + coverage → v0.9/cases.json（含 schema 合法性 + id 唯一断言）
cargo run -p scout-evals --bin fixtures -- generate-evals-v09
```

完整性门：`tests/v09_integrity.rs`（coverage schema 合法 / 全局 id 唯一 / cases.json = v0.5 逐字 + coverage 逐字）。

## Fixture 生成器 (PROTO-05A)

用于生成合成测试文件，以便在不读取用户真实数据的情况下验证后端查询。

```bash
cargo run -p scout-evals --bin fixtures

# 生成 MVP-25 v0.5 eval fixture JSON
cargo run -p scout-evals --bin fixtures -- generate-evals-v05
```

详见 `tests/fixtures/README.md`。

## 企业场景评测语料 (BETA-41)

三场景（律所卷宗 / 审计取证 / 离职归档）× 五桶（scanned-pdf / email / attachment / crosslang-alias / near-dup）的合成评测基线，BETA-35/37/38 共同验收靶。复用 `semantic_quality` harness：

```bash
# 完整性 + 隐私红线门（常跑，无需向量）
cargo test -p scout-evals --test enterprise_recall_fixtures_integrity

# 评测（需先 bootstrap vectors.json，见 fixture README）
cargo run -p scout-evals --bin semantic_quality -- --fixture-set enterprise
```

- **数据**：`fixtures/enterprise-recall/{corpus,cases}.json`（104 doc / 50 case，全合成零 PII）+ `files/`（扫描 PDF / eml / 近重复副本，驱动 indexer 真实提取管线，端到端见 `packages/indexer/tests/real_pdf.rs`）。
- **回归门**：`tests/enterprise_recall_gate.rs`（vectors/baseline 未提交前 skip）。
- 数据卡 / 桶分布 / bootstrap 步骤：[fixtures/enterprise-recall/README.md](./fixtures/enterprise-recall/README.md)。

## 企业三场景 daemon 端到端评测 (BETA-40)

真 `scoutd`（collection 模式 + per-subject token 信息墙）+ 真实 GGUF embedder 的端到端回归：
53 case（律所 18 / 审计 16 / 离职 19，含 11 条越权负样本 + 图片 OCR 语义 case O-09），
语料 = `test-materials/enterprise-scenarios-raw/`，期望集 = 其下 `expected/queries.tsv`。
`expected_paths` 列：正样本为分号分隔相对路径；越权负样本写
`ACCESS_DENIED:<墙目标相对路径>`（声明该 subject 无权触达的真实内容，供闸门校验墙非空洞）。

```bash
# fixture 完整性门（常跑 CI，无需模型）
cargo test -p scout-evals --test enterprise_scenarios_gate

# 端到端（需 llama-cpp feature 的 daemon + GGUF；~3-4 分钟）
cargo run -p scout-evals --bin enterprise_scenarios -- \
    --daemon-binary target/debug/scoutd \
    --model-path <embedder GGUF> --require-all [--semantic-weight <f>] [--json report.json]
```

- 评测语义 / baseline（2026-07-04 全 22/22，daemon 图片语义默认开）/ 权重 A/B 结论：见当时的评审记录。
- 设 `SCOUT_DAEMON_BIN` + `SCOUT_MODEL_PATH` 后 `enterprise_scenarios_gate` 的端到端用例自动启用（否则 skip）。
- `enterprise_scenarios_gate` 的离线断言（无需模型常跑）除路径存在 / 授权 root 归属外，另含两道防假绿护栏：**每个声明 collection 都被 ≥1 case 演练**（无死 collection）+ **每条越权墙目标非空洞**（真实存在且落在未授权 collection 内）。

## 同义词召回评测 (BETA-15A)

离线确定性衡量「手维护词典 + gazetteer」在合成痛点 query 上的召回率/假阳率。
不跑 Spotlight / mdfind / 模型；走真 `parse → expand` 管线 + 忠实 BETA-15D 的子串匹配模拟（组内 OR、组间 AND、大小写不敏感子串，命中域 = 文件名 + content_terms）。

### 用法

```bash
# 跑报告（按桶/按语言分桶 + 门槛退出码）
cargo run -p scout-evals --bin synonym_recall

# 仅看未达标 case
cargo run -p scout-evals --bin synonym_recall -- --only-failures

# JSON 报告
cargo run -p scout-evals --bin synonym_recall -- --json
```

- **门槛**：召回率 ≥ 70%、假阳率 ≤ 5%（`recall::RECALL_GATE` / `FP_GATE`）。退出码 0 达标 / 1 未过 / 2 加载错误。
- **回归门**：`tests/synonym_recall_gate.rs` 随 `cargo test --workspace` 强制执行；`scripts/ci.sh` 另跑 bin 出可读报告。
- **数据**：`fixtures/synonym-recall/{corpus,cases}.json`（手工标注，corpus 100 文件含 20 个显式干扰文件，cases 42 条 zh 28 / en 14，覆盖 office/document/personal 三个内容词桶）。注：假阳率分母是「每个 case 的全部非预期文件」，即所有从不出现在该 case `expected_hits` 的 corpus 文件（约 51 个非命中文件共同构成假阳测量池），测量面比 20 个显式干扰更宽、更严格。
- **当前 baseline**（2026-06-01，option 2 后）：总召回 **100.0%** / 假阳 **0.0%**；按语言 zh **100%** / en **100.0%**。
  - 初始 baseline（2026-05-30）= 总 88.2% / en 46.7%。en gap 经 systematic-debugging 定位为 parser 把疑问词（where）/动词（need/did/save）抽为 keyword → 抑制 gazetteer 兜底。**2026-06-01 fix 第一轮（停词）**：这些功能词加入英文 keyword 停词表，抽取跳到真正内容名词 → en 46.7%→80.0%，假阳仍 0.0%，evals parser-only 472/26/2 零回归。
  - **2026-06-01 fix 第二轮（option 2）**：闭合残留 3 例。**Fix A**（harness）`expand` 多词键覆盖——单 token keyword（cover/style）经词边界匹配升级为多词词典键组（cover letter→application / style guide→branding）；**Fix B**（parser）时长词需数字上下文才算强媒体信号（裸 "minutes" 不再漂移到 media_search）+ copula/助动词（are/was/were/been/being）加入英文 keyword 停词表（修 minutes case 第二层「are 挤掉 minutes」根因）。→ **en 80%→100% / 总→100% / 假阳仍 0%，evals parser-only 472/26/2 零回归**。此 baseline 为 BETA-15B（embedding / LoRA 在线扩词）升级提供对比锚点。
