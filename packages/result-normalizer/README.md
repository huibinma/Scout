# scout-result-normalizer

BETA-04 多源搜索结果归一化合并。把 fan-out 多后端（系统搜索 + 本地索引）返回的
`SearchResult` 按 canonical path 去重合并为 `MergedResult` 列表——同一文件被多个后端命中时
合成一条，保留全部来源与命中类型。

> **排序（BM25 / 打分）留 [BETA-05 Ranker](../../ROADMAP.md)**；本层只去重合并 + 保持首现序。
> 设计见 spec。

## API

```rust
use scout_result_normalizer::{merge_results, MergedResult};

let merged: Vec<MergedResult> = merge_results(all_results_from_multiple_backends);
// MergedResult { result, sources: Vec<BackendKind>, match_types: Vec<MatchType> }
```

合并规则：
- 按 `result.path` 去重（**路径规范化由各 backend 负责**——产出 `SearchResult` 时
  `canonicalize`，本层纯函数无 IO，按 path 字节相等去重）；
- `sources` / `match_types` 取并集（稳定去重序）；
- 代表结果取 `metadata_richness`（非空元数据字段数）最高者；
- `score` 取所有同 path 结果的最大值；
- 保持首现顺序。

纯函数、零外部依赖（仅 `scout-search-backend`），完全可单测（8 单测）。

## 关联

- 上游：[`scout-local-index-backend`](../search-backends/local-index)（本地索引源）+ 系统
  搜索后端（Spotlight / WindowsSearch / 内置原生索引 `scout-native-index`）；
- 调用方：`scout-harness::run_fanout_merge`（fan-out 多源查询后调本层合并）；
- 下游：BETA-05 Ranker（对合并集打分排序）。

## Hybrid 融合的模型感知阈值（2026-07-28）

`fuse_rrf_lists_with_fts_routing` 用 VEC 臂 top-1 cosine 判断是否跳过 FTS 臂，门槛值不再是
孤立的全局常量——`CALIBRATED_COSINE_THRESHOLDS`（`model_id` 前缀匹配，兼容
[`scout-model-runtime::canonical_model_id`](../model-runtime) 与 daemon 端裸 `file_stem()`
两种 model_id 形态）+ `cosine_threshold_for_model` 把阈值随生产 embedding 模型显式绑定，未收录
模型回落 `DEFAULT_COSINE_ROUTING_THRESHOLD` 并由调用方告警，取代此前"换模型阈值悄悄沿用上一代"
的隐患。
