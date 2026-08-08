//! BETA-15B-1：本地语义召回后端。query embed → 暴力 cosine（身份分组、驻留缓存）→ topK。
//!
//! 与 FTS 臂并列进 BETA-04 fanout，由 harness 的加权 RRF 融合层合并。
//! 嵌入器缺失或探测不可用（`TextEmbedder::is_ready()`=false，如 feature 关 /
//! 模型文件缺失 / 加载失败）时 backend 不可用（`is_available()`=false），
//! 路由期即退出 fan-out、整链优雅降级 FTS-only（BETA-33 cycle 9）。
//!
//! 关键约束（同 `LocalIndexBackend`）：rusqlite `Connection` 是 `!Sync`，而
//! `SearchBackend: Send + Sync` → 本 backend **不持久持有连接**，而是持 db 路径、
//! 每次查询内部开连接查完即关。路径规范化在产出 `SearchResult` 时做，与三系统后端
//! 一致，保证跨源去重的 path 一致。
//!
//! **BETA-38 规模化 + 去重**：
//! - **进程级驻留缓存**（`VectorCache`）：签名 =（向量代次 + 模型 + 维度 + 对应行数）。
//!   签名未变则复用内存分组、免每查询把全部向量 BLOB 从 sqlite 全量重载（十万级 ~400MB/查询
//!   的真瓶颈）；reindex 写向量后签名变、下次查询自动重载。不引入 sqlite-vec / ANN（守"轻量
//!   可用"+ 许可洁癖），基准达标即止。
//! - **身份去重**（`group_by_identity`）：候选按 `content_hash`（文件身份，见 indexer BETA-38）
//!   归组，同内容多副本合一组、每组只算一次 cosine、结果只出一条代表（`paths[0]`），其余副本
//!   随组保留（留痕可查）；`content_hash=None`（老库）各自独立、不去重。

// 文档含 file_search / top_k 等领域词，沿用项目对 doc_markdown 的处理。
#![allow(clippy::doc_markdown)]

pub mod explain;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use scout_indexer::embed::TextEmbedder;
use scout_indexer::vectors::cosine;
use scout_indexer::DocumentIndex;
use scout_search_backend::{
    backend_stream_from_results, BackendKind, BackendSearchFuture, CancellationToken,
    ExpandedSearchIntent, KeywordGroup, MatchMode, MatchType, MediaType, SearchBackend,
    SearchError, SearchIntent, SearchResult, SearchResultMetadata,
};
// result_id 与三系统后端 + LocalIndexBackend 共用 common 的单一实现，保证跨源去重 ID 口径一致。
use scout_search_backend::result_id_for_path as result_id;

/// topK 截断上限：暴力 cosine 后取前 K 条。
const TOP_K: usize = 10;

/// BETA-38 cycle 3：按身份分组的驻留向量。同 `content_hash` 的多副本合并为一组——
/// 只算一次 cosine、结果只出一条代表（`paths[0]`），其余副本随组保留（留痕可查）。
#[derive(Debug, Clone)]
struct IdentityGroup {
    vector: Vec<f32>,
    /// 组内全部 path，首个为代表（结果展示 path）；其余为副本位置。
    paths: Vec<String>,
}

/// BETA-38 cycle 3：进程级驻留向量缓存。向量写事务递增 generation；签名同时纳入
/// 当前模型、维度和对应行数，换模/同量更新/WAL 写入都会在下次查询自动重载。
/// （现状十万级每查询重载 ~400MB 的真瓶颈）。
#[derive(Debug, Clone)]
struct VectorCache {
    signature: CacheSignature,
    /// `Arc` 包裹：缓存命中时按 `Arc` 克隆（O(1) 引用计数），免每查询把十万级向量
    /// （~400MB）整体 memcpy——BETA-38 cycle 4 基准实测该 memcpy 曾是缓存路径主耗时。
    groups: Arc<Vec<IdentityGroup>>,
}

/// 缓存失效签名：向量代次 + 模型 id + 查询维度 + 该模型维度的行数。
type CacheSignature = (u64, String, usize, u64);

/// 本地语义召回后端。持 db 路径（与文档索引共用一个 sqlite 文件）+ 注入的嵌入器。
///
/// `embedder` 为 `None` 或其 `is_ready()` 探测失败时本后端不可用（无法对 query 嵌入），
/// 查询返回空、`is_available()`=false。
#[derive(Clone)]
pub struct SemanticIndexBackend {
    db_path: PathBuf,
    embedder: Option<Arc<dyn TextEmbedder>>,
    /// 相似度下限来源（desktop 侧 live-read settings.json；每次查询调）。
    floor_provider: Arc<dyn Fn() -> f32 + Send + Sync>,
    /// BETA-38 cycle 3：进程级驻留向量缓存（长生命周期单例共享）。签名未变则复用、免重载。
    cache: Arc<Mutex<Option<VectorCache>>>,
}

impl std::fmt::Debug for SemanticIndexBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // dyn TextEmbedder 与 floor_provider 闭包均不要求 Debug，手写：只暴露 db_path +
        // 是否注入嵌入器，闭包字段以 finish_non_exhaustive 略过。
        f.debug_struct("SemanticIndexBackend")
            .field("db_path", &self.db_path)
            .field("has_embedder", &self.embedder.is_some())
            .field(
                "cache_loaded",
                &self.cache.lock().is_ok_and(|c| c.is_some()),
            )
            .finish_non_exhaustive()
    }
}

impl SemanticIndexBackend {
    /// 用索引数据库路径 + 可选嵌入器构造。
    /// `db_path` 不必预先存在；未索引 / 无向量前查询返回空。
    /// `embedder` 为 `None` → 后端不可用（交 FTS 臂兜底）。
    pub fn new(
        db_path: impl Into<PathBuf>,
        embedder: Option<Arc<dyn TextEmbedder>>,
        floor_provider: Arc<dyn Fn() -> f32 + Send + Sync>,
    ) -> Self {
        Self {
            db_path: db_path.into(),
            embedder,
            floor_provider,
            cache: Arc::new(Mutex::new(None)),
        }
    }

    /// 从 intent 取查询规格：`(查询文本, 是否只限图片)`。
    ///
    /// - `FileSearch`：非空 keywords 拼接，不限类型（原有行为）；
    /// - `MediaSearch(Image|Screenshot)`：非空 keywords 拼接 + **只限图片候选**——
    ///   「xx的截图 / 照片」类问法被 parser 路由成 MediaSearch，此前语义臂直接跳过，
    ///   图片语义索引（BETA-39）对最自然的图片问法反而失效（BETA-40 O-09 评测实锤）。
    ///   过滤按代表 path 扩展名 ∈ [`scout_indexer::IMAGE_EXTS`]（与索引口径一致）；
    ///   图片未入语义索引（默认关 / daemon `--disable-image-semantics`）时候选自然为空。
    /// - 其余 intent（音频 / 视频 / 动作类）/ 无 keyword → `None`。
    fn query_spec(intent: &SearchIntent) -> Option<(String, bool)> {
        let (keywords, images_only) = match intent {
            SearchIntent::FileSearch(fs) => (fs.keywords.as_ref(), false),
            SearchIntent::MediaSearch(ms)
                if matches!(ms.media_type, MediaType::Image | MediaType::Screenshot) =>
            {
                (ms.keywords.as_ref(), true)
            }
            _ => return None,
        };
        let text = keywords.map(|k| k.join(" "))?.trim().to_owned();
        if text.is_empty() {
            None
        } else {
            Some((text, images_only))
        }
    }

    /// 代表 path 是否图片（扩展名 ∈ `IMAGE_EXTS`，大小写不敏感）。
    fn is_image_path(path: &str) -> bool {
        Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                let e = e.to_ascii_lowercase();
                scout_indexer::IMAGE_EXTS.contains(&e.as_str())
            })
    }

    /// 语义检索执行：embed query → 暴力 cosine 全部身份分组（驻留缓存）→ 降序取 topK → 映射。
    /// 无嵌入器 / 无查询文本 / db 不存在 → 空结果（非错误），交 FTS 臂兜底。
    ///
    /// BETA-38 cycle 3：向量走进程级驻留缓存（签名未变免全量重载 BLOB）；同 `content_hash`
    /// 多副本已在缓存加载时归组，此处每组只算一次 cosine、结果只出一条代表。
    ///
    /// 公开的**同步**查询核心：`search()` 的 async/stream 包装即调此方法。诊断 / 评测
    /// （BETA-38 规模化基准）可直接调，免驱动异步流。
    pub fn search_results(&self, intent: &SearchIntent) -> Result<Vec<SearchResult>, SearchError> {
        // 无嵌入器 → 不可用，空结果。
        let Some(embedder) = self.embedder.as_ref() else {
            return Ok(Vec::new());
        };
        // 非文件/图片搜索 / 无关键词 → 空。
        let Some((text, images_only)) = Self::query_spec(intent) else {
            return Ok(Vec::new());
        };
        // 未索引（db 不存在）→ 空。
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }

        let query_vec = embedder.embed(&text).map_err(to_search_err)?;
        let groups = self.load_groups(embedder.model_id(), query_vec.len())?;
        let groups = groups.as_ref();

        // 暴力 cosine：每个身份分组算一次相似度，代表 path = 组内首个；
        // 图片类 MediaSearch 只保留图片候选（尊重 intent 的类型语义）。
        let scored: Vec<(f32, String)> = groups
            .iter()
            .filter_map(|g| {
                g.paths
                    .first()
                    .filter(|p| !images_only || Self::is_image_path(p))
                    .map(|p| (cosine(&query_vec, &g.vector), p.clone()))
            })
            .collect();
        let floor = (self.floor_provider)();
        let scored = filter_rank_topk(scored, floor, TOP_K);

        Ok(scored
            .into_iter()
            .map(|(score, path)| vector_hit_to_result(&path, score))
            .collect())
    }

    /// 2026-07-20：复合条件语义检索——按 `match_mode` 对 `keyword_groups` 做语义层面的
    /// AND/OR，而不是把所有关键词拼成一句整体 embed（旧行为丢失"这是几个独立条件"的
    /// 结构、导致「全部命中」模式下语义臂可能只贴合其中一个条件就把文档召回，绕开了
    /// FTS 臂的严格 AND）。
    ///
    /// 做法：每个词组（含同义词）**各自** embed 组内每个词，取该组对候选文档的**最大**
    /// cosine（组内 OR，同义词命中任一即算这个条件满足，语义近似词天然也算，如"研发部"
    /// 命中"开发部"这个条件）；再按 `match_mode` 汇总多个条件的分数：`All` 取**最小值**
    /// （每个条件都必须过关，一票否决）、`Any` 取**最大值**（任一条件过关即可，与现状
    /// 单一整句 embed 的召回宽松度相当）。
    ///
    /// **词组数 < 2 时沿用 [`Self::search_results`] 的整句 embed 行为**（单条件查询下
    /// `All`/`Any` 本就语义等价，拆分只会多算不换取任何额外精确性）。
    pub fn search_results_expanded(
        &self,
        expanded: &ExpandedSearchIntent,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let valid_groups: Vec<&KeywordGroup> = expanded
            .keyword_groups
            .iter()
            .filter(|g| !g.head.trim().is_empty())
            .collect();
        if valid_groups.len() < 2 {
            return self.search_results(&expanded.base);
        }

        let Some(embedder) = self.embedder.as_ref() else {
            return Ok(Vec::new());
        };
        let Some((_, images_only)) = Self::query_spec(&expanded.base) else {
            return Ok(Vec::new());
        };
        if !self.db_path.exists() {
            return Ok(Vec::new());
        }

        // 逐组把组内每个词（head + 同义词）各 embed 一次；组数/词数通常个位数，
        // embed 调用开销可忽略——真正的暴力扫描（cosine × 全部文档向量）在下面才发生。
        let mut group_vecs: Vec<Vec<Vec<f32>>> = Vec::with_capacity(valid_groups.len());
        for group in &valid_groups {
            let mut vecs = Vec::new();
            for term in group.all() {
                let term = term.trim();
                if term.is_empty() {
                    continue;
                }
                vecs.push(embedder.embed(term).map_err(to_search_err)?);
            }
            group_vecs.push(vecs);
        }

        let Some(query_dim) = group_vecs
            .iter()
            .flat_map(|vectors| vectors.iter())
            .map(Vec::len)
            .next()
        else {
            return Ok(Vec::new());
        };
        let groups = self.load_groups(embedder.model_id(), query_dim)?;
        let groups = groups.as_ref();
        let floor = (self.floor_provider)();
        let match_mode = expanded.match_mode;

        let scored: Vec<(f32, String)> = groups
            .iter()
            .filter_map(|doc_group| {
                let path = doc_group.paths.first()?;
                if images_only && !Self::is_image_path(path) {
                    return None;
                }
                // 组内 OR：该条件对本文档的分数 = 组内各词 cosine 的最大值。
                let per_condition_scores = group_vecs.iter().map(|term_vecs| {
                    term_vecs
                        .iter()
                        .map(|qv| cosine(qv, &doc_group.vector))
                        .fold(f32::MIN, f32::max)
                });
                // 组间按 match_mode 汇总：All 取最小值（一票否决）、Any 取最大值。
                let combined = match match_mode {
                    MatchMode::All => per_condition_scores.fold(f32::MAX, f32::min),
                    MatchMode::Any => per_condition_scores.fold(f32::MIN, f32::max),
                };
                Some((combined, path.clone()))
            })
            .collect();

        let scored = filter_rank_topk(scored, floor, TOP_K);
        Ok(scored
            .into_iter()
            .map(|(score, path)| vector_hit_to_result(&path, score))
            .collect())
    }

    /// 取当前身份分组：缓存签名（generation + model + dim + 行数）未变则复用，否则重载。
    /// 归组 = 按 `content_hash` 聚合（相同身份合一组、代表向量取首见，副本 path 随组保留）；
    /// `content_hash=None` 的候选各自独立成组（不去重）。
    fn load_groups(
        &self,
        embed_model: &str,
        query_dim: usize,
    ) -> Result<Arc<Vec<IdentityGroup>>, SearchError> {
        let signature = self.current_signature(embed_model, query_dim)?;
        // 快路径：签名命中缓存 → 按 Arc 克隆（O(1)，不 memcpy 向量）。
        if let Ok(guard) = self.cache.lock() {
            if let Some(cache) = guard.as_ref() {
                if cache.signature == signature {
                    return Ok(Arc::clone(&cache.groups));
                }
            }
        }
        // 慢路径：重载全部候选 + 归组。
        let idx = DocumentIndex::open(&self.db_path).map_err(to_search_err)?;
        let candidates = idx
            .candidate_vectors_for_model_dim(embed_model, query_dim)
            .map_err(to_search_err)?;
        let groups = Arc::new(group_by_identity(candidates));
        if let Ok(mut guard) = self.cache.lock() {
            *guard = Some(VectorCache {
                signature,
                groups: Arc::clone(&groups),
            });
        }
        Ok(groups)
    }

    /// 当前缓存签名只读 schema_meta + COUNT，不加载 BLOB。
    fn current_signature(
        &self,
        embed_model: &str,
        query_dim: usize,
    ) -> Result<CacheSignature, SearchError> {
        let idx = DocumentIndex::open(&self.db_path).map_err(to_search_err)?;
        let generation = idx.vector_generation().map_err(to_search_err)?;
        let count = idx
            .vector_count_for_model_dim(embed_model, query_dim)
            .map_err(to_search_err)?;
        Ok((generation, embed_model.to_owned(), query_dim, count))
    }
}

/// 把候选向量按 `content_hash` 归组：同身份合一组（代表向量取首见、副本 path 追加），
/// `None` 身份各自独立成组。组内顺序稳定（按候选出现顺序）。
fn group_by_identity(candidates: Vec<scout_indexer::CandidateVector>) -> Vec<IdentityGroup> {
    use std::collections::HashMap;
    let mut groups: Vec<IdentityGroup> = Vec::new();
    // content_hash → groups 下标（仅对 Some 身份去重）。
    let mut index: HashMap<String, usize> = HashMap::new();
    for c in candidates {
        match c.content_hash {
            Some(h) => {
                if let Some(&i) = index.get(&h) {
                    groups[i].paths.push(c.path);
                } else {
                    index.insert(h, groups.len());
                    groups.push(IdentityGroup {
                        vector: c.vector,
                        paths: vec![c.path],
                    });
                }
            }
            None => groups.push(IdentityGroup {
                vector: c.vector,
                paths: vec![c.path],
            }),
        }
    }
    groups
}

impl SearchBackend for SemanticIndexBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::SemanticIndex
    }

    /// 嵌入器缺失或探测不可用（feature 关 / 模型缺失 / 加载失败）→ 不可用
    /// （query 无法嵌入），路由期即退出 fan-out、整链优雅降级 FTS-only。
    /// `is_ready()` 每查询 live 探测——模型文件事后就位 / 后台暖机完成后语义臂自动回归。
    fn is_available(&self) -> bool {
        self.embedder.as_ref().is_some_and(|e| e.is_ready())
    }

    fn search<'a>(
        &'a self,
        intent: &'a SearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        Box::pin(async move {
            let results = self.search_results(intent)?;
            Ok(backend_stream_from_results(results, cancel))
        })
    }

    /// 2026-07-20：复合条件（≥2 有效词组）时按 `match_mode` 做逐条件语义 AND/OR
    /// （见 [`Self::search_results_expanded`]）；单条件/无条件时与旧行为一致，直接走
    /// base intent 整句 embed（同义词/gazetteer 由 FTS 臂覆盖，语义召回靠向量近邻
    /// 天然吸收同义/近义，此时拆分无额外收益）。
    fn search_expanded<'a>(
        &'a self,
        expanded: &'a ExpandedSearchIntent,
        cancel: CancellationToken,
    ) -> BackendSearchFuture<'a> {
        Box::pin(async move {
            let results = self.search_results_expanded(expanded)?;
            Ok(backend_stream_from_results(results, cancel))
        })
    }
}

/// 按相似度下限过滤 + 降序排序 + 截断 topK（纯函数，可单测）。
/// 全部候选低于 `floor` → 返回空（语义臂空，整链优雅降级 FTS-only）。
fn filter_rank_topk(mut scored: Vec<(f32, String)>, floor: f32, k: usize) -> Vec<(f32, String)> {
    scored.retain(|(s, _)| *s >= floor);
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.truncate(k);
    scored
}

/// 候选向量命中 → `SearchResult`。`path` 规范化（与三系统后端 / LocalIndexBackend 一致），
/// `score` = cosine（升至 f64），`match_type=Semantic`、`source=SemanticIndex`。
fn vector_hit_to_result(path: &str, score: f32) -> SearchResult {
    let path = canonical(Path::new(path));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    SearchResult {
        id: result_id(&path),
        name,
        path,
        source: BackendKind::SemanticIndex,
        match_type: MatchType::Semantic,
        score: Some(f64::from(score)),
        metadata: SearchResultMetadata::default(),
    }
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[allow(clippy::needless_pass_by_value)] // 用于 `.map_err(to_search_err)`，owned 更顺手
fn to_search_err(e: scout_indexer::IndexError) -> SearchError {
    SearchError::Io {
        detail: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use scout_search_backend::{FileSearch, SchemaVersion};

    /// 确定性「坐标轴」嵌入器：含「猫」→ x 轴，含「狗」→ y 轴。
    /// 让 cosine 排序可预测，无需真实模型。
    #[derive(Debug)]
    struct AxisEmbedder;
    impl TextEmbedder for AxisEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>, scout_indexer::IndexError> {
            Ok(vec![
                if text.contains('猫') { 1.0 } else { 0.0 },
                if text.contains('狗') { 1.0 } else { 0.0 },
            ])
        }
        fn model_id(&self) -> &'static str {
            "axis"
        }
    }

    /// 构造 FileSearch intent（关键词单元素）。
    fn file_search(keyword: &str) -> SearchIntent {
        SearchIntent::FileSearch(FileSearch {
            schema_version: SchemaVersion::V1,
            language: None,
            keywords: Some(vec![keyword.to_owned()]),
            extensions: None,
            file_type: None,
            location: None,
            modified_time: None,
            created_time: None,
            accessed_time: None,
            size: None,
            exclude_extensions: None,
            exclude_file_type: None,
            sort: None,
            limit: None,
        })
    }

    #[test]
    fn semantic_query_ranks_by_cosine() {
        let dir = tempfile::tempdir().unwrap();
        // 写两份真实 txt，让 index_dirs 建文档行（upsert_vector 需先有文档行）。
        std::fs::write(dir.path().join("cat.txt"), "关于猫的笔记").unwrap();
        std::fs::write(dir.path().join("dog.txt"), "关于狗的笔记").unwrap();
        let db = dir.path().join("index.db");
        let idx = DocumentIndex::open(&db).unwrap();
        idx.index_dirs(&[dir.path().to_path_buf()]).unwrap();

        // 用索引存的同款 path 字符串（绝对路径）挂载手工向量：cat→[1,0]、dog→[0,1]。
        let cat = dir.path().join("cat.txt").to_string_lossy().into_owned();
        let dog = dir.path().join("dog.txt").to_string_lossy().into_owned();
        assert!(idx.upsert_vector(&cat, &[1.0, 0.2], "axis", "h1").unwrap()); // cosine ≈ 0.98
        assert!(idx.upsert_vector(&dog, &[1.0, 1.0], "axis", "h2").unwrap()); // cosine ≈ 0.71

        let backend = SemanticIndexBackend::new(
            &db,
            Some(Arc::new(AxisEmbedder)),
            std::sync::Arc::new(|| 0.30_f32),
        );
        // 查询「我家的猫」→ embed=[1,0]，应让 cat 排第一。
        let intent = file_search("我家的猫");
        let results = backend.search_results(&intent).unwrap();

        assert_eq!(results.len(), 2, "两条候选都应返回");
        // 路径可能被 canonical 规范化（temp dir 软链等），断言 file_name 更稳。
        assert_eq!(results[0].name, "cat.txt", "猫查询应让 cat 排第一");
        assert_eq!(results[1].name, "dog.txt");
        assert_eq!(results[0].source, BackendKind::SemanticIndex);
        assert_eq!(results[0].match_type, MatchType::Semantic);
        // cat 与查询同轴 → cosine ≈ 1；dog 正交 → ≈ 0。
        assert!(results[0].score.unwrap() > results[1].score.unwrap());
    }

    /// 2026-07-20：复合条件（2 词组：「猫」「狗」）+ `MatchMode::All`——只有**同时**
    /// 沾猫又沾狗的文档才应通过（一票否决语义）；纯猫 / 纯狗文档在另一个条件上的 cosine
    /// 接近 0、被 floor 挡在外面。`MatchMode::Any` 下三份文档应全部通过（任一条件即可）。
    #[test]
    fn expanded_all_mode_requires_every_group_any_mode_requires_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cat.txt"), "关于猫的笔记").unwrap();
        std::fs::write(dir.path().join("dog.txt"), "关于狗的笔记").unwrap();
        std::fs::write(dir.path().join("both.txt"), "猫和狗都有的笔记").unwrap();
        let db = dir.path().join("index.db");
        let idx = DocumentIndex::open(&db).unwrap();
        idx.index_dirs(&[dir.path().to_path_buf()]).unwrap();

        let cat = dir.path().join("cat.txt").to_string_lossy().into_owned();
        let dog = dir.path().join("dog.txt").to_string_lossy().into_owned();
        let both = dir.path().join("both.txt").to_string_lossy().into_owned();
        // AxisEmbedder：猫→x轴[1,0]，狗→y轴[0,1]。cat 纯 x、dog 纯 y、both 两轴都占。
        assert!(idx.upsert_vector(&cat, &[1.0, 0.0], "axis", "h1").unwrap());
        assert!(idx.upsert_vector(&dog, &[0.0, 1.0], "axis", "h2").unwrap());
        assert!(idx.upsert_vector(&both, &[1.0, 1.0], "axis", "h3").unwrap());

        let backend = SemanticIndexBackend::new(
            &db,
            Some(Arc::new(AxisEmbedder)),
            std::sync::Arc::new(|| 0.30_f32),
        );
        let groups = vec![KeywordGroup::singleton("猫"), KeywordGroup::singleton("狗")];

        let expanded_all = ExpandedSearchIntent {
            base: file_search("猫 狗"),
            keyword_groups: groups.clone(),
            match_mode: MatchMode::All,
        };
        let all_results = backend.search_results_expanded(&expanded_all).unwrap();
        let all_names: Vec<&str> = all_results.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            all_names,
            vec!["both.txt"],
            "All 模式只有同时沾猫又沾狗的文档应通过一票否决：{all_names:?}"
        );

        let expanded_any = ExpandedSearchIntent {
            base: file_search("猫 狗"),
            keyword_groups: groups,
            match_mode: MatchMode::Any,
        };
        let any_results = backend.search_results_expanded(&expanded_any).unwrap();
        assert_eq!(
            any_results.len(),
            3,
            "Any 模式任一条件命中即可，三份文档都应通过：{any_results:?}"
        );
    }

    /// 单个有效词组（或全 singleton 未扩词也只剩 1 组）时应退化为 `search_results` 的整句
    /// embed 行为，`match_mode` 不产生任何影响（省一次多余的逐组 embed/cosine）。
    #[test]
    fn expanded_single_group_falls_back_to_plain_search_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cat.txt"), "关于猫的笔记").unwrap();
        let db = dir.path().join("index.db");
        let idx = DocumentIndex::open(&db).unwrap();
        idx.index_dirs(&[dir.path().to_path_buf()]).unwrap();
        let cat = dir.path().join("cat.txt").to_string_lossy().into_owned();
        assert!(idx.upsert_vector(&cat, &[1.0, 0.2], "axis", "h1").unwrap());

        let backend = SemanticIndexBackend::new(
            &db,
            Some(Arc::new(AxisEmbedder)),
            std::sync::Arc::new(|| 0.30_f32),
        );
        let expanded = ExpandedSearchIntent {
            base: file_search("我家的猫"),
            keyword_groups: vec![KeywordGroup::singleton("我家的猫")],
            match_mode: MatchMode::All,
        };
        let results = backend.search_results_expanded(&expanded).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "cat.txt");
    }

    /// 构造 MediaSearch intent（走 serde internally-tagged 反序列化，免平铺全部字段）。
    fn media_search(media_type: &str, keyword: &str) -> SearchIntent {
        serde_json::from_value(serde_json::json!({
            "intent": "media_search",
            "schema_version": "1.0",
            "media_type": media_type,
            "keywords": [keyword],
        }))
        .expect("media_search intent 应能构造")
    }

    /// 测试用 stub OCR：任何图片都返回固定文本（免真实 OCR 引擎依赖）。
    #[derive(Debug)]
    struct StubOcr;
    impl scout_indexer::OcrEngine for StubOcr {
        fn recognize(&self, _image: &Path) -> Result<String, scout_indexer::IndexError> {
            Ok("关于猫的看板截图".to_owned())
        }
        fn name(&self) -> &'static str {
            "stub"
        }
    }

    /// BETA-40 O-09 根因回归：图片类 MediaSearch（「xx的截图」问法）应走语义臂且只返回
    /// 图片候选；音频类 MediaSearch 仍然跳过。
    #[test]
    fn media_image_intent_served_and_filtered_to_images() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cat.txt"), "关于猫的笔记").unwrap();
        std::fs::write(dir.path().join("board.png"), b"\x89PNG fake").unwrap();
        let db = dir.path().join("index.db");
        let idx = DocumentIndex::open(&db).unwrap();
        idx.index_dirs(&[dir.path().to_path_buf()]).unwrap();
        // 图片轮（stub OCR）让 board.png 建文档行。
        idx.index_image_dirs_excluding_with_progress(
            &[dir.path().to_path_buf()],
            &StubOcr,
            &scout_indexer::GlobSet::empty(),
            &scout_indexer::NoopProgress,
        )
        .unwrap();

        let cat = dir.path().join("cat.txt").to_string_lossy().into_owned();
        let png = dir.path().join("board.png").to_string_lossy().into_owned();
        assert!(idx.upsert_vector(&cat, &[1.0, 0.0], "axis", "h1").unwrap());
        assert!(idx.upsert_vector(&png, &[1.0, 0.1], "axis", "h2").unwrap());

        let backend = SemanticIndexBackend::new(
            &db,
            Some(Arc::new(AxisEmbedder)),
            std::sync::Arc::new(|| 0.30_f32),
        );

        // 图片类：只返回 png（txt 被类型过滤），且确实走了语义臂。
        let results = backend
            .search_results(&media_search("image", "我家的猫"))
            .unwrap();
        assert_eq!(results.len(), 1, "图片 intent 只应返回图片候选");
        assert_eq!(results[0].name, "board.png");
        assert_eq!(results[0].match_type, MatchType::Semantic);

        // screenshot 同 image 语义。
        let results = backend
            .search_results(&media_search("screenshot", "我家的猫"))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "board.png");

        // 音频类：语义臂不接（维持原行为）。
        let results = backend
            .search_results(&media_search("audio", "我家的猫"))
            .unwrap();
        assert!(results.is_empty(), "音频 intent 不应走语义臂");

        // FileSearch 不受过滤影响：两条都在。
        let results = backend.search_results(&file_search("我家的猫")).unwrap();
        assert_eq!(results.len(), 2, "FileSearch 应不做类型过滤");
    }

    #[test]
    fn query_spec_and_image_path_helpers() {
        // FileSearch → 不限图片；MediaSearch(image) → 限图片；无 keyword → None。
        let (_, imgs) = SemanticIndexBackend::query_spec(&file_search("猫")).unwrap();
        assert!(!imgs);
        let (text, imgs) = SemanticIndexBackend::query_spec(&media_search("image", "猫")).unwrap();
        assert_eq!(text, "猫");
        assert!(imgs);
        assert!(SemanticIndexBackend::query_spec(&media_search("video", "猫")).is_none());

        assert!(SemanticIndexBackend::is_image_path("/a/b/photo.PNG"));
        assert!(SemanticIndexBackend::is_image_path(r"C:\x\shot.jpg"));
        assert!(!SemanticIndexBackend::is_image_path("/a/b/note.txt"));
        assert!(!SemanticIndexBackend::is_image_path("/a/b/noext"));
    }

    #[test]
    fn filter_rank_topk_filters_sorts_truncates() {
        let scored = vec![
            (0.10_f32, "low.txt".to_owned()),
            (0.90, "hi.txt".to_owned()),
            (0.50, "mid.txt".to_owned()),
            (0.29, "below.txt".to_owned()),
        ];
        let out = filter_rank_topk(scored, 0.30, 10);
        let names: Vec<&str> = out.iter().map(|(_, p)| p.as_str()).collect();
        assert_eq!(names, vec!["hi.txt", "mid.txt"], "仅 ≥floor 存活、降序");
    }

    #[test]
    fn filter_rank_topk_truncates_to_k() {
        let scored = vec![
            (0.9_f32, "a".to_owned()),
            (0.8, "b".to_owned()),
            (0.7, "c".to_owned()),
        ];
        let out = filter_rank_topk(scored, 0.30, 2);
        assert_eq!(out.len(), 2, "截断到 K");
        assert_eq!(out[0].1, "a");
        assert_eq!(out[1].1, "b");
    }

    #[test]
    fn filter_rank_topk_all_below_floor_is_empty() {
        let scored = vec![(0.10_f32, "a".to_owned()), (0.20, "b".to_owned())];
        assert!(
            filter_rank_topk(scored, 0.30, 10).is_empty(),
            "全低于 floor → 空"
        );
    }

    /// 端到端：低相关（正交，cosine 0）候选被下限挡掉，不进结果。
    #[test]
    fn semantic_floor_filters_low_relevance() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cat.txt"), "关于猫的笔记").unwrap();
        std::fs::write(dir.path().join("dog.txt"), "关于狗的笔记").unwrap();
        let db = dir.path().join("index.db");
        let idx = DocumentIndex::open(&db).unwrap();
        idx.index_dirs(&[dir.path().to_path_buf()]).unwrap();
        let cat = dir.path().join("cat.txt").to_string_lossy().into_owned();
        let dog = dir.path().join("dog.txt").to_string_lossy().into_owned();
        assert!(idx.upsert_vector(&cat, &[1.0, 0.0], "axis", "h1").unwrap());
        assert!(idx.upsert_vector(&dog, &[0.0, 1.0], "axis", "h2").unwrap());

        let backend = SemanticIndexBackend::new(
            &db,
            Some(Arc::new(AxisEmbedder)),
            std::sync::Arc::new(|| 0.30_f32),
        );
        let results = backend.search_results(&file_search("我家的猫")).unwrap();

        assert_eq!(results.len(), 1, "正交低相关 dog 被下限挡掉");
        assert_eq!(results[0].name, "cat.txt");
    }

    #[test]
    fn no_embedder_is_unavailable_and_empty() {
        // 无嵌入器路径在 search_results 开头即返回，不触碰 DB；故无需建库/放模型。
        let db = std::path::PathBuf::from("/nonexistent/index.db");
        let backend = SemanticIndexBackend::new(&db, None, std::sync::Arc::new(|| 0.30_f32));
        assert!(!backend.is_available(), "无嵌入器 → 不可用");
        let intent = file_search("猫");
        assert!(
            backend.search_results(&intent).unwrap().is_empty(),
            "无嵌入器 → 空结果"
        );
    }

    /// 探测不可用的嵌入器：模拟 feature 关 / 模型缺失 / 加载失败的桌面句柄
    /// （BETA-33 cycle 9：`is_ready()`=false 时语义臂路由期即退出 fan-out）。
    #[derive(Debug)]
    struct NotReadyEmbedder;
    impl TextEmbedder for NotReadyEmbedder {
        fn embed(&self, _text: &str) -> Result<Vec<f32>, scout_indexer::IndexError> {
            Err(scout_indexer::IndexError::Io {
                path: String::new(),
                detail: "embedding 模型不可用".to_owned(),
            })
        }
        fn model_id(&self) -> &'static str {
            "not-ready"
        }
        fn is_ready(&self) -> bool {
            false
        }
    }

    /// BETA-33 cycle 9：嵌入器存在但探测不可用（`is_ready()`=false）→ backend 不可用，
    /// 路由期即被 `available_search_tools_supporting` 剔除、不进 fan-out——不再出现
    /// 「必败语义臂每查询报 embedding 模型不可用」。默认实现（未覆写 is_ready）仍恒可用。
    #[test]
    fn not_ready_embedder_is_unavailable() {
        let db = std::path::PathBuf::from("/nonexistent/index.db");
        let backend = SemanticIndexBackend::new(
            &db,
            Some(Arc::new(NotReadyEmbedder)),
            std::sync::Arc::new(|| 0.30_f32),
        );
        assert!(!backend.is_available(), "is_ready()=false → 不可用");

        // 对照：默认 is_ready()=true 的嵌入器（AxisEmbedder 未覆写）→ 可用，零行为变化。
        let backend = SemanticIndexBackend::new(
            &db,
            Some(Arc::new(AxisEmbedder)),
            std::sync::Arc::new(|| 0.30_f32),
        );
        assert!(backend.is_available(), "默认 is_ready()=true → 可用");
    }

    /// BETA-38 cycle 3：`group_by_identity` 纯函数——同 content_hash 合一组（首个代表 + 副本
    /// 随组）、None 身份各自独立、顺序稳定。
    #[test]
    fn group_by_identity_merges_same_hash_keeps_none_separate() {
        use scout_indexer::CandidateVector;
        let cands = vec![
            CandidateVector {
                path: "/a.txt".into(),
                vector: vec![1.0, 0.0],
                content_hash: Some("h1".into()),
            },
            CandidateVector {
                path: "/a_copy.txt".into(),
                vector: vec![1.0, 0.0],
                content_hash: Some("h1".into()), // 同身份 → 并入首组
            },
            CandidateVector {
                path: "/b.txt".into(),
                vector: vec![0.0, 1.0],
                content_hash: Some("h2".into()),
            },
            CandidateVector {
                path: "/legacy.txt".into(),
                vector: vec![0.5, 0.5],
                content_hash: None, // 老库无身份 → 独立成组
            },
        ];
        let groups = group_by_identity(cands);
        assert_eq!(groups.len(), 3, "h1 合一组 + h2 + None 独立 = 3 组");
        assert_eq!(
            groups[0].paths,
            vec!["/a.txt", "/a_copy.txt"],
            "首个为代表、副本随组"
        );
        assert_eq!(groups[1].paths, vec!["/b.txt"]);
        assert_eq!(
            groups[2].paths,
            vec!["/legacy.txt"],
            "None 身份不与他人合并"
        );
    }

    /// BETA-38 cycle 3：两份相同内容的文档在语义结果里只出一条（代表），不被副本刷屏。
    #[test]
    fn semantic_dedups_identical_content_copies_in_results() {
        let dir = tempfile::tempdir().unwrap();
        // 两份完全相同内容 → 同 content_hash（index_dirs 回填）。
        let same = "关于猫的详细笔记内容用于副本去重测试足够长度";
        std::fs::write(dir.path().join("cat_orig.txt"), same).unwrap();
        std::fs::write(dir.path().join("cat_copy.txt"), same).unwrap();
        let db = dir.path().join("index.db");
        let idx = DocumentIndex::open(&db).unwrap();
        idx.index_dirs(&[dir.path().to_path_buf()]).unwrap();
        // 两副本挂同一向量（模拟 cycle 2 复用后的状态）。
        let orig = dir
            .path()
            .join("cat_orig.txt")
            .to_string_lossy()
            .into_owned();
        let copy = dir
            .path()
            .join("cat_copy.txt")
            .to_string_lossy()
            .into_owned();
        assert!(idx.upsert_vector(&orig, &[1.0, 0.0], "axis", "h1").unwrap());
        assert!(idx.upsert_vector(&copy, &[1.0, 0.0], "axis", "h1").unwrap());

        let backend = SemanticIndexBackend::new(
            &db,
            Some(Arc::new(AxisEmbedder)),
            std::sync::Arc::new(|| 0.30_f32),
        );
        let results = backend.search_results(&file_search("我家的猫")).unwrap();
        assert_eq!(results.len(), 1, "两份相同内容副本合并为一条结果");
    }

    /// BETA-38 cycle 3：缓存签名未变时连续查询走驻留缓存、结果一致（不改变正确性）。
    #[test]
    fn semantic_cache_hit_keeps_results_consistent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cat.txt"),
            "关于猫的笔记内容足够长度用于测试",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("dog.txt"),
            "关于狗的笔记内容足够长度用于测试",
        )
        .unwrap();
        let db = dir.path().join("index.db");
        let idx = DocumentIndex::open(&db).unwrap();
        idx.index_dirs(&[dir.path().to_path_buf()]).unwrap();
        let cat = dir.path().join("cat.txt").to_string_lossy().into_owned();
        let dog = dir.path().join("dog.txt").to_string_lossy().into_owned();
        assert!(idx.upsert_vector(&cat, &[1.0, 0.2], "axis", "h1").unwrap());
        assert!(idx.upsert_vector(&dog, &[1.0, 1.0], "axis", "h2").unwrap());
        drop(idx); // 关连接，落盘

        let backend = SemanticIndexBackend::new(
            &db,
            Some(Arc::new(AxisEmbedder)),
            std::sync::Arc::new(|| 0.30_f32),
        );
        // 首查询填充缓存；二查询命中缓存。两次结果（顺序 + 命中）应一致。
        let r1 = backend.search_results(&file_search("我家的猫")).unwrap();
        let r2 = backend.search_results(&file_search("我家的猫")).unwrap();
        assert_eq!(r1.len(), 2);
        let n1: Vec<&str> = r1.iter().map(|r| r.name.as_str()).collect();
        let n2: Vec<&str> = r2.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(n1, n2, "缓存命中查询与首查询结果一致");
        assert_eq!(n1[0], "cat.txt", "猫查询代表命中不受缓存/去重影响");
    }

    #[test]
    fn semantic_cache_reloads_on_same_count_vector_update_and_filters_model_dim() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cat.txt"), "关于猫的笔记内容足够长度").unwrap();
        std::fs::write(dir.path().join("dog.txt"), "关于狗的笔记内容足够长度").unwrap();
        let db = dir.path().join("index.db");
        let idx = DocumentIndex::open(&db).unwrap();
        idx.index_dirs(&[dir.path().to_path_buf()]).unwrap();
        let cat = dir.path().join("cat.txt").to_string_lossy().into_owned();
        let dog = dir.path().join("dog.txt").to_string_lossy().into_owned();
        idx.upsert_vector(&cat, &[1.0, 0.0], "axis", "h1").unwrap();
        // 不同模型即使维度相同也不能进入 axis 查询候选。
        idx.upsert_vector(&dog, &[1.0, 0.0], "old-model", "h2")
            .unwrap();
        let generation_before = idx.vector_generation().unwrap();
        drop(idx);

        let backend = SemanticIndexBackend::new(
            &db,
            Some(Arc::new(AxisEmbedder)),
            std::sync::Arc::new(|| 0.0_f32),
        );
        let first = backend.search_results(&file_search("猫")).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name, "cat.txt");

        // 行数不变、可发生于 WAL：同一 doc 原地更新必须递增 generation 并使驻留缓存失效。
        let idx = DocumentIndex::open(&db).unwrap();
        idx.upsert_vector(&cat, &[0.0, 1.0], "axis", "h3").unwrap();
        assert!(idx.vector_generation().unwrap() > generation_before);
        drop(idx);

        let second = backend.search_results(&file_search("猫")).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].name, "cat.txt");
        assert_eq!(second[0].score, Some(0.0), "必须读到更新后的向量");
    }

    #[test]
    fn floor_provider_controls_filtering() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cat.txt"), "关于猫的笔记").unwrap();
        std::fs::write(dir.path().join("dog.txt"), "关于狗的笔记").unwrap();
        let db = dir.path().join("index.db");
        let idx = DocumentIndex::open(&db).unwrap();
        idx.index_dirs(&[dir.path().to_path_buf()]).unwrap();
        let cat = dir.path().join("cat.txt").to_string_lossy().into_owned();
        let dog = dir.path().join("dog.txt").to_string_lossy().into_owned();
        // cat[1,0.2]·查询「猫」[1,0]≈0.98；dog[1,1]≈0.71。
        assert!(idx.upsert_vector(&cat, &[1.0, 0.2], "axis", "h1").unwrap());
        assert!(idx.upsert_vector(&dog, &[1.0, 1.0], "axis", "h2").unwrap());

        let strict = SemanticIndexBackend::new(
            &db,
            Some(Arc::new(AxisEmbedder)),
            std::sync::Arc::new(|| 0.95_f32),
        );
        let r = strict.search_results(&file_search("我家的猫")).unwrap();
        assert_eq!(r.len(), 1, "高下限只留 cat");
        assert_eq!(r[0].name, "cat.txt");

        let loose = SemanticIndexBackend::new(
            &db,
            Some(Arc::new(AxisEmbedder)),
            std::sync::Arc::new(|| 0.0_f32),
        );
        assert_eq!(
            loose
                .search_results(&file_search("我家的猫"))
                .unwrap()
                .len(),
            2
        );
    }
}
