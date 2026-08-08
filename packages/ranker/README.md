# scout-ranker

BETA-05 多源结果排序。对 BETA-04 fan-out 合并集（`MergedResult` 列表）做排序——填补
「合并集本无全局排序」的缺口。

> 设计见 spec。

## API

```rust
use scout_ranker::{rank, RankContext};

let ctx = RankContext::from_expanded(&expanded);  // 提取 keywords + sort
let ranked = rank(merged, &ctx);                  // Vec<MergedResult> 排序后
```

## 两种排序

| 场景 | 行为 |
|---|---|
| **显式 sort**（modified/created/accessed/size/name 各 asc/desc） | 按对应 `metadata` 字段跨源排序，缺失字段排末尾。**不写 score** |
| **相关性**（sort = `RelevanceDesc` / `None`） | 启发式打分写入 `result.score`，降序 + tiebreak（modified 新→前 → name 升序） |

### 相关性启发式（纯函数、无 IO、跨源可比）

`score = 0.5·name_match + 0.3·match_type_weight + 0.2·source_boost` ∈ [0,1]：

- **name_match**：命中 `keywords` 的比例（文件名小写子串）；无 keyword → 0。
- **match_type_weight**：`match_types` 取最大——Filename 1.0 / Metadata 0.85 / Content 0.7 / Ocr 0.6。
- **source_boost**：`min(sources.len()-1, 2)/2`（单源 0，3+ 源 1）。

## 实际默认排序（与 parser 配合）

parser 对 **file_search 默认 `modified_desc`**、**media_search 默认 `relevance_desc`**：
- 文档/文件查询 → 跨源**按修改时间**排序（之前 fan-out 完全无全局排序，本 crate 补上）；
- 媒体查询（如「找周华健的歌」）/ 无 sort 查询 → **相关性**排序（artist/正文命中靠前）。

## known limitation

- **不接真 BM25 / 跨源 IDF**（不可得：系统后端无 score，FTS bm25 仅局部、跨语料不可比）；
  纯启发式相关性是诚实可比的近似，BM25 留未来细化。
- **仅作用于 fan-out 路径**（BETA-04 多源合并）；fallback 单后端维持后端自身排序。
- **权重为文档化常量**（无可配置 / 学习排序；商业秘密保护见风险清单 §5.3，当前为启发式 baseline）。
- 只排序不截断（limit 由后端/调用方）。

纯函数，11 单测（相关性 + 显式排序 + from_expanded 提取）。
