# evals v0.9 — 覆盖驱动评测集（1000 条）

> BETA-13 产物。

## 构成

- `cases.json`（1000 条）= `../v0.5/cases.json`（500，逐字保留）+ `coverage-cases.json`（500，手标 ground-truth）。
- `coverage-cases.json`（500）：**覆盖驱动、独立标注**的 ground-truth——按 schema 语义 + 功能设计意图标注「query *应该*解析成什么」，独立于 parser 当前行为，用于暴露 parser 在自然语言上的真实缺口。
- `_authoring/`：分域撰写分片（d1–d9）+ 标注指南；`assemble-coverage` 确定性汇编为 coverage-cases.json。

### 复现

```bash
cargo run -p scout-evals --bin fixtures -- assemble-coverage      # 分片 → coverage-cases.json
cargo run -p scout-evals --bin fixtures -- generate-evals-v09     # v0.5 + coverage → cases.json
cargo run -p scout-evals --bin evals -- --fixtures v0.9           # 跑 baseline
```

完整性由 `tests/v09_integrity.rs` 守门（coverage schema 合法 / 全局 id 唯一 / cases.json = v0.5 逐字 + coverage 逐字）。

> **更新（2026-06-04）**：BETA-13-G1~G7 parser 缺口已全部消化，v0.9 由 **514/1000 (51.4%) → 726/1000 (72.6%)**（variant 95.1%，fail 49），v0.5 锚点 472→473 零回归。下表为数据集建成时的初始 baseline，保留作历史对照。剩余未达 90% 为 v0.5↔coverage 契约冲突 + 标注不一致等固有上限（详见 ROADMAP BETA-13-G* 说明）。

## Baseline（parser-only，2026-06-03）

| 子集 | pass | partial | fail | pass% |
|---|---|---|---|---|
| v0.5（500，锚点） | 472 | 26 | 2 | 94.4% |
| **coverage（500，新）** | 42 | 321 | 137 | **8.4%** |
| **合计（1000）** | **514** | 347 | 139 | **51.4%** |

> v0.5 段维持 472/26/2 **byte-equal**（未被触碰）。本次含一处 parser 修复（见下「已修」），对 v0.5 零回归。

### 核心结论

**parser 在模板化 v0.5 上 94.4%，但在覆盖驱动的自然语言 query 上仅 8.4%。** 这是 v0.9 的核心产出：量化了 parser 的**自然语言鲁棒性**缺口。**§6 出场指标「总体 evals 通过率 > 90%」当前未达成**（51.4%），缺口集中在自然语言抽取/路由，属设计级、非低风险可修；按 spec §8「不为凑指标强标 case，如实报告达成率 + gap 清单」，登记为 parser 改进 backlog（见 ROADMAP BETA-13-G*）。

### 按覆盖域

| 域 | 覆盖能力 | pass% | 主要缺口 |
|---|---|---|---|
| d1 同义词/自然关键词 | BETA-15/15E | 7% | 自然句子里的名词短语 keyword 抽不出 |
| d2 跨范畴多类型 | BETA-18/19 | 4% | 多类型路由 + 类型词识别 |
| d3 内容检索 | BETA-02/03 | 1% | 内容词 keyword 抽不出 + 类型词 |
| d4 音乐 metadata | BETA-01/01A | 12% | artist/genre/album/时长 措辞鲁棒性 |
| d5 时间/尺寸/位置/排序 | 核心 schema | 6% | 显式排序、绝对/before/after 时间、类型词 |
| d6 file_action | MVP-19 | 20% | 自然 action 识别（误判为 file_search）|
| d7 refine | refine 族 | 13% | refine 标记词覆盖不全（误判为 file_search）|
| d8 clarify | clarify 族 | 10% | clarify 触发（设计性，部分可议）|
| d9 bug 回归锚点 | — | 20% | 跨范畴/多扩展名（部分已是历史修复点）|

### variant 误路由 Top

| 误路由 | 数 | 含义 |
|---|---|---|
| file_search → MediaSearch | 30 | 含媒体强词的 file_search 被吞为 media |
| refine → FileSearch | 27 | refine 标记词未覆盖 → 当成新搜索 |
| file_action → FileSearch | 26 | 自然 action 未识别 |
| media_search → FileSearch | 23 | 媒体 metadata 措辞未识别 |
| clarify → FileSearch | 22 | clarify 未触发（设计性） |

### partial diff 字段频率 Top（coverage）

```
183 keywords      123 file_type     40 extensions    26 sort       25 artist
 18 modified_time  16 location      12 size          10 created_time  8 genre/duration
```

## Gap 分类与处置

### 已修（本 task，低风险）

- **location 子串误报 bug**：中文 location 关键词走纯 substring 匹配，`演示文稿`(presentation) 含 `文稿`(documents) → 误报 `location.hint="文稿"`。修复：中文 location 关键词若是查询中某个更长且存在的类型关键词的子串则抑制（`parsers/common.rs::cjk_location_shadowed`）。回归单测 `presentation_does_not_falsely_trigger_documents_location`；v0.5 零回归。

### 登记 parser 改进 backlog（设计级，ROADMAP BETA-13-G*）

- **G1 自然语言关键词抽取**：自然句子里的名词短语（`工作汇报`/`年度预算`/`marketing plan`）抽不出 keyword（最大头，~159 例）。关联 BETA-15E/15B。
- **G2 音乐 metadata 措辞鲁棒性**：artist/genre/album/title/duration 在多样自然措辞下抽取脆弱（`周杰伦的歌`/`找邓紫棋的歌曲` 抽不出 artist；裸开头/「歌曲」vs「歌」差异）。
- **G3 中文类型词 → file_type**：`表格`/`幻灯片`/`演示文稿` 等非字面扩展名的类型词未映射到 file_type。
- **G4 refine 标记词覆盖**：`只要`/`换成`/`再加上`/`去掉`/`不限…了` 等自然 refine 措辞未识别（parser 现仅认 `只看`/`排除`/`清空` 等）。
- **G5 file_action 自然识别**：`打开第一个`/`删除这些` 等在无显式列表上下文时被路由为 file_search。
- **G6 显式排序词覆盖**：`按名字排序` 等显式排序表达被忽略（退回默认 modified_desc）。
- **G7 clarify 触发阈值（设计性）**：`昨天的`/`最近的` 等纯约束查询是否该 clarify，需产品决策——当前 parser 倾向 file_search(match-all)，部分 coverage 标为 clarify，属可议项。

### 标注约定修正（本 task，使 baseline 可信）

coverage 标注中我方的约定错已外科修正（不改真 gap 标签）：① `language` 采用 parser 检测值（含跨脚本 token 的 query 判 mixed）；② 字面 ASCII 格式词（ppt/pdf/docx…）的 `extensions` 采用 parser 输出（格式表示是约定非 gap）。中文类型词、keyword、媒体 metadata 等语义标签保持「应然」不动。

## 与 v0.5 的关系

v0.5 仍是模板生成的回归锚点（472/26/2 byte-equal），衡量「parser 在结构化模板 query 上不回归」。v0.9 的 coverage 段衡量「parser 在自然语言上的鲁棒性」，二者互补：v0.5 防回归、v0.9 指方向。
