//! BETA-38 cycle 4：语义向量检索十万级规模化基准工具。
//!
//! 提供：确定性合成语料生成器（含**已知副本组**，复用 BETA-41 `dup_group` 靶设计）、
//! 延迟统计（p50/p95/p99）、固定查询嵌入器。供 `bench_semantic` bin 对比
//! 「进程级缓存」vs「暴力全量重载」的 p95 延迟 + 常驻内存，并断言身份去重正确
//! （同副本组在结果里合并为一条代表，不被副本刷屏）。
//!
//! 生成器**不落大文件入仓**：向量按 seed 确定性生成、直接批量种入临时 index.db
//! （[`DocumentIndex::seed_synthetic_vectors`]），可复现、跑完即弃。

// 基准用途：RNG→f32 转换与向量字节统计的整型窄化均精度无关；沿用 perf.rs bin 同款务实 allow。
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use std::time::Duration;

use anyhow::{ensure, Result};
use scout_indexer::embed::TextEmbedder;
use scout_indexer::{DocumentIndex, IndexError};
use scout_search_backend::{FileSearch, SchemaVersion, SearchIntent};

/// 合成模型 id：种入向量与查询嵌入器共用，保证 `embed_model` 一致（候选加载不被模型过滤）。
pub const SYNTH_MODEL: &str = "synth-bench";

/// 合成语料规格。`total` 总文档数中，`dup_groups * dup_copies` 篇为已知副本
/// （分 `dup_groups` 组、每组 `dup_copies` 份同身份），其余为各自独立身份的唯一文档。
#[derive(Debug, Clone)]
pub struct CorpusSpec {
    /// 总文档数（含副本）。
    pub total: usize,
    /// 向量维度（生产 `EmbeddingGemma` = 768/1024；基准可调）。
    pub dim: usize,
    /// 已知副本组数。
    pub dup_groups: usize,
    /// 每组副本数（含原件；≥2 才构成副本）。
    pub dup_copies: usize,
    /// PRNG 种子（可复现）。
    pub seed: u64,
}

/// 生成结果元数据（供基准报告 + 去重正确性断言）。
#[derive(Debug, Clone)]
pub struct GeneratedCorpus {
    /// 实际种入文档行数（= `spec.total`）。
    pub total_docs: usize,
    /// 去重后身份数（= `dup_groups` + 唯一文档数）——缓存驻留的向量条数。
    pub identities: usize,
    /// 查询向量：等于首个副本组的向量 → cosine≈1.0 命中该组（去重断言靶）。
    pub query_vector: Vec<f32>,
    /// 首个副本组的 `content_hash`（期望命中的代表身份）。
    pub query_target_hash: String,
    /// 首个副本组的全部副本 path（正确性断言：结果只应出其一）。
    pub target_group_paths: Vec<String>,
    /// 去重后常驻向量字节数（`identities * dim * 4`）——缓存内存占用度量。
    pub vector_bytes: usize,
}

/// splitmix64：零依赖、可复现 PRNG（避免引入 `rand`）。
#[derive(Debug)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// [0,1) 均匀 f32（取高 24 位）。
    fn next_unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// 一条 `dim` 维、L2 归一化的随机向量（模拟归一化 embedding 输出）。
    fn unit_vector(&mut self, dim: usize) -> Vec<f32> {
        let mut v: Vec<f32> = (0..dim).map(|_| self.next_unit() * 2.0 - 1.0).collect();
        l2_normalize(&mut v);
        v
    }
}

fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// 按规格确定性生成合成语料并种入 `idx`。返回元数据（含查询靶 + 去重断言数据）。
///
/// 布局：先 `dup_groups` 组副本（组内同 `content_hash` + 同向量），后唯一文档
/// （各自独立 `content_hash`）。首个副本组的向量兼作查询向量（保证命中该组代表）。
pub fn generate_and_seed(idx: &DocumentIndex, spec: &CorpusSpec) -> Result<GeneratedCorpus> {
    ensure!(spec.dim > 0, "dim 须为正");
    ensure!(spec.dup_copies >= 1, "dup_copies 须 ≥1");
    let dup_docs = spec
        .dup_groups
        .checked_mul(spec.dup_copies)
        .ok_or_else(|| anyhow::anyhow!("dup_groups * dup_copies 溢出"))?;
    ensure!(
        spec.total >= dup_docs,
        "total ({}) 须 ≥ dup_groups*dup_copies ({dup_docs})",
        spec.total
    );
    ensure!(spec.dup_groups >= 1, "至少一组副本作查询靶");

    let unique_docs = spec.total - dup_docs;
    let mut rng = SplitMix64::new(spec.seed);
    // (path, content_hash, vector)
    let mut docs: Vec<(String, Option<String>, Vec<f32>)> = Vec::with_capacity(spec.total);

    let mut query_vector = Vec::new();
    let mut query_target_hash = String::new();
    let mut target_group_paths = Vec::new();

    for g in 0..spec.dup_groups {
        let vector = rng.unit_vector(spec.dim);
        let hash = format!("dupg-{g:08x}");
        for c in 0..spec.dup_copies {
            let path = format!("/synthetic/dup/{g}/copy_{c}.txt");
            if g == 0 {
                target_group_paths.push(path.clone());
            }
            docs.push((path, Some(hash.clone()), vector.clone()));
        }
        if g == 0 {
            query_vector = vector;
            query_target_hash = hash;
        }
    }

    for u in 0..unique_docs {
        let vector = rng.unit_vector(spec.dim);
        let hash = format!("uniq-{u:08x}");
        let path = format!("/synthetic/uniq/doc_{u}.txt");
        docs.push((path, Some(hash), vector));
    }

    idx.seed_synthetic_vectors(&docs, SYNTH_MODEL)?;

    let identities = spec.dup_groups + unique_docs;
    Ok(GeneratedCorpus {
        total_docs: spec.total,
        identities,
        query_vector,
        query_target_hash,
        target_group_paths,
        vector_bytes: identities * spec.dim * 4,
    })
}

/// 基准查询 intent：`FileSearch` 单关键词（嵌入器忽略文本、返回固定靶向量，故词本身无关）。
#[must_use]
pub fn bench_intent() -> SearchIntent {
    SearchIntent::FileSearch(FileSearch {
        schema_version: SchemaVersion::V1,
        language: None,
        keywords: Some(vec!["bench".to_owned()]),
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

/// 固定向量嵌入器：对任意查询文本返回同一预置向量（基准里把查询固定到已知靶向量，
/// 隔离出「候选加载 + cosine」这一被测路径，不引真实模型）。
#[derive(Debug, Clone)]
pub struct FixedEmbedder {
    vector: Vec<f32>,
}

impl FixedEmbedder {
    #[must_use]
    pub fn new(vector: Vec<f32>) -> Self {
        Self { vector }
    }
}

impl TextEmbedder for FixedEmbedder {
    fn embed(&self, _text: &str) -> Result<Vec<f32>, IndexError> {
        Ok(self.vector.clone())
    }

    fn model_id(&self) -> &str {
        SYNTH_MODEL
    }
}

/// 延迟分布统计（纳秒精度 `Duration`）。
#[derive(Debug, Clone)]
pub struct LatencyStats {
    pub count: usize,
    pub min: Duration,
    pub max: Duration,
    pub mean: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
}

impl LatencyStats {
    /// 从样本计算分位（nearest-rank）。空样本 → 全 0。
    #[must_use]
    pub fn from_samples(mut samples: Vec<Duration>) -> Self {
        if samples.is_empty() {
            return Self {
                count: 0,
                min: Duration::ZERO,
                max: Duration::ZERO,
                mean: Duration::ZERO,
                p50: Duration::ZERO,
                p95: Duration::ZERO,
                p99: Duration::ZERO,
            };
        }
        samples.sort_unstable();
        let n = samples.len();
        let sum: Duration = samples.iter().sum();
        let mean = sum / n as u32;
        Self {
            count: n,
            min: samples[0],
            max: samples[n - 1],
            mean,
            p50: percentile(&samples, 50),
            p95: percentile(&samples, 95),
            p99: percentile(&samples, 99),
        }
    }
}

/// nearest-rank 分位（`samples` 须已升序）。`p` ∈ [0,100]。
fn percentile(samples: &[Duration], p: u32) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    let n = samples.len();
    // rank = ceil(p/100 * n)，1-based；clamp 到 [1, n]。
    let rank = ((u64::from(p) * n as u64).div_ceil(100)).max(1) as usize;
    samples[rank.min(n) - 1]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn percentile_nearest_rank_boundaries() {
        let s: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        assert_eq!(percentile(&s, 50), Duration::from_millis(50));
        assert_eq!(percentile(&s, 95), Duration::from_millis(95));
        assert_eq!(percentile(&s, 99), Duration::from_millis(99));
        assert_eq!(percentile(&s, 100), Duration::from_millis(100));
    }

    #[test]
    fn latency_stats_empty_is_zero() {
        let st = LatencyStats::from_samples(vec![]);
        assert_eq!(st.count, 0);
        assert_eq!(st.p95, Duration::ZERO);
    }

    #[test]
    fn generator_is_deterministic_for_seed() {
        let idx1 = DocumentIndex::open_in_memory().unwrap();
        let idx2 = DocumentIndex::open_in_memory().unwrap();
        let spec = CorpusSpec {
            total: 40,
            dim: 16,
            dup_groups: 3,
            dup_copies: 4,
            seed: 42,
        };
        let a = generate_and_seed(&idx1, &spec).unwrap();
        let b = generate_and_seed(&idx2, &spec).unwrap();
        assert_eq!(a.query_vector, b.query_vector, "同 seed → 同查询向量");
        assert_eq!(a.identities, b.identities);
        // 身份数 = 3 组 + (40 - 12) 唯一 = 31。
        assert_eq!(a.identities, 31);
        assert_eq!(a.total_docs, 40);
        assert_eq!(a.target_group_paths.len(), 4, "首组 4 副本");
    }

    #[test]
    fn generator_rejects_oversized_dup_set() {
        let idx = DocumentIndex::open_in_memory().unwrap();
        let spec = CorpusSpec {
            total: 5,
            dim: 8,
            dup_groups: 3,
            dup_copies: 4, // 12 > 5
            seed: 1,
        };
        assert!(generate_and_seed(&idx, &spec).is_err());
    }
}
