//! BETA-38 cycle 4：规模化基准的正确性护栏（小规模、CI 常跑）。
//!
//! 只断言**正确性**（生成器确定性 + 身份去重 + baseline≡cached 结果一致），
//! **不断言计时**（延迟受机器负载影响、易抖动，留 `bench_semantic` bin 手动出报告）。
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use scout_evals::scaling::{bench_intent, generate_and_seed, CorpusSpec, FixedEmbedder};
use scout_indexer::DocumentIndex;
use scout_semantic_index::SemanticIndexBackend;

fn small_spec() -> CorpusSpec {
    CorpusSpec {
        total: 2_000,
        dim: 128,
        dup_groups: 5,
        dup_copies: 4,
        seed: 7,
    }
}

#[test]
fn dedup_collapses_target_group_to_single_representative() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("index.db");
    let corpus = {
        let idx = DocumentIndex::open(&db).unwrap();
        generate_and_seed(&idx, &small_spec()).unwrap()
    };
    // 5 组 ×4 = 20 副本 + 1980 唯一 = 1985 身份。
    assert_eq!(corpus.identities, 1_985);
    assert_eq!(corpus.total_docs, 2_000);

    let embedder = Arc::new(FixedEmbedder::new(corpus.query_vector.clone()));
    let backend = SemanticIndexBackend::new(&db, Some(embedder), Arc::new(|| 0.0_f32));
    let results = backend.search_results(&bench_intent()).unwrap();

    // 靶组 4 份副本的 file_name。
    let target_names: Vec<String> = corpus
        .target_group_paths
        .iter()
        .map(|p| {
            std::path::Path::new(p)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    let hits = results
        .iter()
        .filter(|r| target_names.contains(&r.name))
        .count();
    assert_eq!(hits, 1, "靶组 4 副本应合并为一条代表，不刷屏");
    // 查询向量 == 靶组向量 → cosine≈1 → 代表排首位。
    assert!(
        target_names.contains(&results[0].name),
        "靶组代表应排首位（cosine≈1）"
    );
}

#[test]
fn cached_and_baseline_paths_agree() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("index.db");
    let corpus = {
        let idx = DocumentIndex::open(&db).unwrap();
        generate_and_seed(&idx, &small_spec()).unwrap()
    };
    let embedder = Arc::new(FixedEmbedder::new(corpus.query_vector.clone()));
    let intent = bench_intent();

    // baseline：新后端 → 冷缓存 → 全量重载。
    let baseline = SemanticIndexBackend::new(&db, Some(embedder.clone()), Arc::new(|| 0.0_f32));
    let r_base = baseline.search_results(&intent).unwrap();

    // cached：复用后端，暖机后再查。
    let cached = SemanticIndexBackend::new(&db, Some(embedder), Arc::new(|| 0.0_f32));
    let _ = cached.search_results(&intent).unwrap();
    let r_cached = cached.search_results(&intent).unwrap();

    let names_base: Vec<&str> = r_base.iter().map(|r| r.name.as_str()).collect();
    let names_cached: Vec<&str> = r_cached.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names_base, names_cached,
        "暴力重载与进程级缓存两条路径结果应逐条一致"
    );
    assert!(!r_base.is_empty(), "靶向量命中，结果非空");
}
