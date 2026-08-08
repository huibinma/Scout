# scout-local-index-backend

BETA-04 `LocalIndexBackend`：把 [`scout-indexer`](../../indexer) 的音乐 / 文档本地索引
包成 `SearchBackend`（`BackendKind::NativeIndex`），让本地索引参与 fan-out 多源搜索——
补上系统搜索按 artist / 正文搜不到的命中（「找周华健的歌」「找含某词的文档」）。

> 设计见 spec。

## API

```rust
use scout_local_index_backend::LocalIndexBackend;

let backend = LocalIndexBackend::new(db_path);     // 音乐+文档表共用一个 sqlite 文件

// 手动索引（reindex 命令调用）：扫音乐 + 文档 + 图片 OCR（BETA-03）目录写入索引。
let (music_stats, doc_stats, image_stats) =
    backend.reindex(&music_roots, &doc_roots, &image_roots)?;
// 图片轮经 default_ocr_engine()：引擎不可用（无 OCR 语言包 / 无 tesseract）则跳过、统计为零。

// 作为 SearchBackend 注册进 ToolRegistry（id "search.local"），由 harness fan-out 驱动。
```

## intent → 索引查询翻译

| intent | 索引 | 说明 |
|---|---|---|
| `MediaSearch{ media_type: Audio }` | `MusicIndex` | text=keywords/title，artist/album 走结构化过滤；结果 `match_type=Metadata`，填 artist/title/album/duration |
| `FileSearch{ keywords }` | `DocumentIndex` | text=keywords（同一 FTS 天然也覆盖图片 OCR 文字）；结果 `match_type=Content` |
| `MediaSearch{ media_type: Image/Screenshot, keywords }` | `DocumentIndex` | OCR 文字 FTS，`doc_types` 框定只返图片类型（BETA-03） |
| 视频媒体、无 keyword 的图片/纯扩展名查询 | — | 空流（不贡献，交系统后端） |
| Refine / FileAction / Clarify | — | `UnsupportedIntent` |

## 关键约束

- **rusqlite `Connection` 是 `!Sync`，而 `SearchBackend: Send + Sync`** → 本 backend
  **不持久持有连接**，持 db 路径、每次 `search()` 内部开连接查完即关。
- **路径规范化**在产出 `SearchResult` 时做（`fs::canonicalize`，与 Spotlight 一致），
  保证跨源去重（[`scout-result-normalizer`](../../result-normalizer)）的 path 一致。
- **未 reindex（db 不存在）→ 空流**（非错误），系统后端正常服务。

## known limitation

- 图片经 BETA-03 OCR 入库（视频仍无本地索引）；
- 不在本地索引层做同义词扩展（系统后端已覆盖）；
- 未做时间/大小等 PostFilter（v1 仅 text + artist/album/doc_type 过滤）；
- CJK 查询需 ≥3 字符（继承 indexer 的 trigram tokenizer 限制）——`fts_match_from_groups`
  已剔除词组内 <3 字纯 CJK 词项使其不参与 AND 匹配（BETA-42，避免短词拖垮多词组合查询
  结构性 0 命中），但该短词本身仍无法单独被本地 FTS 匹配到。
- 多词查询组间 AND：`search_results_expanded` 先按组间 AND 查，**0 命中且 ≥2 有效词组时**
  经 `fts_or_relax_from_groups` 放宽成组间 OR 兜底重试一次（BETA-57，修多词自然语言泛查
  「缺任一词即整条归零」的召回缺陷）——仅 AND 空时触发，已命中查询行为不变、零精确性回归。

## 测试

9 单测：纯 query-builder / mapper（`build_music_query` / `build_doc_query` /
`music_entry_to_result` / `doc_hit_to_result`）+ 端到端文档搜索（reindex 真 txt → search）+
空 db / 图片媒体 / 无 keyword / unsupported intent 分支。
