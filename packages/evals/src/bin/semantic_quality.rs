//! BETA-15B-6：语义召回质量评测 CLI。默认读 checked-in 合成语料 + 缓存向量，
//! 跑三臂 Recall@10/nDCG@10 分桶报告。`--json` 机读；`--write-baseline` 写 baseline.json。
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::expect_used,
    clippy::panic
)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use scout_evals::semantic_quality::data::{
    check_enterprise_integrity, check_integrity, check_vectors, load_cases, load_corpus,
    load_vectors,
};
use scout_evals::semantic_quality::report::{aggregate, score_case, BucketAgg};
use scout_evals::semantic_quality::{EVAL_SIMILARITY_FLOOR, TOP_K};
use scout_result_normalizer::{
    DEFAULT_COSINE_ROUTING_THRESHOLD, DEFAULT_RRF_K, DEFAULT_SEMANTIC_WEIGHT,
};

/// prefix 契约模式（BETA-15B-11 §4.2）：选 `none` 走裸文本、选 `standard` 走
/// `EmbeddingGemma` HF 模型卡的标准包装。默认 `none` 守 BETA-15B-10 及之前所有
/// cycle 向下兼容（不动现存 vectors.json/baseline.json/gate 守护对象）。
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PrefixMode {
    #[default]
    None,
    Standard,
}

/// fixture 集（BETA-41）：`personal` = 既有 `fixtures/semantic-recall/`（默认，
/// 与 BETA-15B 全部 cycle 向下兼容、不动 baseline/gate 守护对象）；
/// `enterprise` = `fixtures/enterprise-recall/`（三场景五桶，BETA-35/37/38 验收基线）。
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
enum FixtureSet {
    #[default]
    Personal,
    Enterprise,
}

impl FixtureSet {
    fn dir(self) -> &'static str {
        match self {
            Self::Personal => "fixtures/semantic-recall",
            Self::Enterprise => "fixtures/enterprise-recall",
        }
    }
}

#[derive(Parser)]
#[command(name = "semantic_quality", about = "BETA-15B-6 语义召回质量评测")]
struct Cli {
    /// 输出 JSON（分桶聚合）。
    #[arg(long)]
    json: bool,
    /// 把当前 hybrid 分桶结果写成 baseline.json（用户 bootstrap 用）。
    #[arg(long)]
    write_baseline: bool,
    /// 调模型重算 doc+query 向量、写 vectors.json（需 feature semantic-recall + 模型）。
    #[arg(long)]
    embed: bool,
    /// embedding 模型路径（仅 --embed）。
    #[arg(long, default_value = "models/qwen3-embedding-0.6b-q8_0.gguf")]
    model: String,
    /// 融合层语义臂权重（默认 = result-normalizer::DEFAULT_SEMANTIC_WEIGHT）。
    /// sweep 用：`--semantic-weight=3.0` 等。
    #[arg(long, default_value_t = DEFAULT_SEMANTIC_WEIGHT)]
    semantic_weight: f64,
    /// VEC top-1 cosine 绝对分数阈值路由：`vec[0].score >= threshold` 时跳过 FTS 臂。
    /// `1.01` ≈ 永不跳（cosine ∈ [0,1] 物理上限、与 spec §5 降级值同义）。
    /// 默认 = `DEFAULT_COSINE_ROUTING_THRESHOLD`（task 8 sweep 后 bake）。BETA-15B-3 A-5。
    #[arg(long, default_value_t = DEFAULT_COSINE_ROUTING_THRESHOLD)]
    cosine_threshold: f64,
    /// vectors 文件相对路径（相对 `fixtures/semantic-recall/`）。
    /// 默认 = `vectors.json`（与现 baseline.json / gate 守护对象一致）。
    /// sweep 多模型时用：`--vectors-file=vectors-bge-m3.json` 等。
    /// 同时影响 `--embed`（输出位置）和默认 sweep（输入位置）。BETA-15B-7。
    #[arg(long, default_value = "vectors.json")]
    vectors_file: String,
    /// prefix 契约模式：none = 裸 embed、standard = `EmbeddingGemma` HF 卡 prefix 包装。
    /// 默认 none 守 BETA-15B-10 及之前所有 cycle 向下兼容。BETA-15B-11 §4.2。
    #[arg(long, value_enum, default_value_t = PrefixMode::default())]
    prefix_mode: PrefixMode,
    /// fixture 集：personal = semantic-recall（默认）、enterprise = enterprise-recall（BETA-41）。
    /// 影响 corpus/cases/vectors/baseline 的读写目录与完整性检查桶集。
    #[arg(long, value_enum, default_value_t = FixtureSet::default())]
    fixture_set: FixtureSet,
}

fn fixt_in(set: FixtureSet, rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(set.dir())
        .join(rel)
}

fn print_table(aggs: &[BucketAgg]) {
    println!(
        "{:<18} {:>3} | {:>7} {:>7} | {:>7} {:>7} | {:>7} {:>7} | {:>7} {:>7}",
        "bucket", "n", "FTS_R", "FTS_N", "VEC_R", "VEC_N", "HYB_R", "HYB_N", "HYBR_R", "HYBR_N"
    );
    println!("{}", "-".repeat(98));
    for a in aggs {
        println!(
            "{:<18} {:>3} | {:>7.3} {:>7.3} | {:>7.3} {:>7.3} | {:>7.3} {:>7.3} | {:>7.3} {:>7.3}",
            a.bucket,
            a.n,
            a.fts_recall,
            a.fts_ndcg,
            a.vec_recall,
            a.vec_ndcg,
            a.hybrid_recall,
            a.hybrid_ndcg,
            a.hybrid_routed_recall,
            a.hybrid_routed_ndcg
        );
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let corpus = load_corpus(&fixt_in(cli.fixture_set, "corpus.json")).expect("读 corpus.json");
    let cases = load_cases(&fixt_in(cli.fixture_set, "cases.json")).expect("读 cases.json");
    match cli.fixture_set {
        FixtureSet::Personal => check_integrity(&corpus, &cases).expect("语料/评测集完整性"),
        FixtureSet::Enterprise => {
            check_enterprise_integrity(&corpus, &cases).expect("企业语料/评测集完整性");
        }
    }

    if cli.embed {
        #[cfg(feature = "semantic-recall")]
        {
            embed_and_write(
                &corpus,
                &cases,
                &cli.model,
                cli.fixture_set,
                &cli.vectors_file,
                cli.prefix_mode,
            );
            eprintln!("已写 {}", &cli.vectors_file);
            return ExitCode::SUCCESS;
        }
        #[cfg(not(feature = "semantic-recall"))]
        {
            eprintln!(
                "--embed 需 feature semantic-recall（且放好模型）。见 {}/README.md",
                cli.fixture_set.dir()
            );
            return ExitCode::from(2);
        }
    }

    let vectors = load_vectors(&fixt_in(cli.fixture_set, &cli.vectors_file))
        .unwrap_or_else(|_| panic!("读 {}（缺则先跑 --embed，见 README）", cli.vectors_file));
    check_vectors(&corpus, &cases, &vectors).expect("向量覆盖完整性");

    let scores: Vec<_> = cases
        .iter()
        .map(|c| {
            score_case(
                c,
                &corpus,
                &vectors,
                EVAL_SIMILARITY_FLOOR,
                cli.semantic_weight,
                DEFAULT_RRF_K,
                cli.cosine_threshold,
                TOP_K,
            )
        })
        .collect();
    let aggs = aggregate(&scores);

    if cli.write_baseline {
        let json = serde_json::to_string_pretty(&aggs).expect("序列化 baseline");
        std::fs::write(fixt_in(cli.fixture_set, "baseline.json"), json).expect("写 baseline.json");
        eprintln!("已写 baseline.json（{} 桶含 OVERALL）", aggs.len());
    }

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&aggs).expect("序列化"));
    } else {
        print_table(&aggs);
    }
    ExitCode::SUCCESS
}

/// embed 调用方角色：区分 query 侧与 doc 侧、用于 prefix-mode = standard 时
/// 套不同 `EmbeddingGemma` 模板（BETA-15B-11 §4.2）。内部 enum、不外露。
#[cfg(feature = "semantic-recall")]
#[derive(Clone, Copy, Debug)]
enum EmbedRole {
    Query,
    Doc,
}

#[cfg(feature = "semantic-recall")]
fn embed_and_write(
    corpus: &[scout_evals::semantic_quality::data::SemanticDoc],
    cases: &[scout_evals::semantic_quality::data::SemanticCase],
    model: &str,
    fixture_set: FixtureSet,
    vectors_file: &str,
    prefix_mode: PrefixMode,
) {
    use scout_evals::semantic_quality::data::VectorCache;
    use scout_model_runtime::{get_default_loader, ModelLoadParams};
    use std::collections::BTreeMap;
    use std::path::Path;

    let loader = get_default_loader();
    let rt = loader
        .load(
            Path::new(model),
            &ModelLoadParams {
                gpu_layers: 99,
                context_size: 2048,
            },
        )
        .expect("加载 embedding 模型");

    let embed = |text: &str, role: EmbedRole| -> Vec<f32> {
        let wrapped = match (prefix_mode, role) {
            (PrefixMode::None, _) => text.to_string(),
            (PrefixMode::Standard, EmbedRole::Query) => {
                format!("task: search result | query: {text}")
            }
            (PrefixMode::Standard, EmbedRole::Doc) => {
                format!("title: none | text: {text}")
            }
        };
        rt.embed(&wrapped).expect("embed 失败")
    };

    let mut doc_vectors = BTreeMap::new();
    let mut dim = 0usize;
    for d in corpus {
        let v = embed(&format!("{}\n{}", d.title, d.body), EmbedRole::Doc);
        dim = v.len();
        doc_vectors.insert(d.doc_id.clone(), v);
    }
    let mut query_vectors = BTreeMap::new();
    for c in cases {
        query_vectors.insert(c.id.clone(), embed(&c.query, EmbedRole::Query));
    }
    let vc = VectorCache {
        model_id: model.to_owned(),
        dim,
        doc_vectors,
        query_vectors,
    };
    let json = serde_json::to_string(&vc).expect("序列化向量");
    std::fs::write(fixt_in(fixture_set, vectors_file), json)
        .unwrap_or_else(|_| panic!("写 {vectors_file}"));
}

#[cfg(test)]
mod cli_tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn semantic_weight_flag_parses() {
        let cli = Cli::parse_from(["semantic_quality", "--semantic-weight", "4.0"]);
        assert!((cli.semantic_weight - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn semantic_weight_defaults_to_const() {
        use scout_result_normalizer::DEFAULT_SEMANTIC_WEIGHT;
        let cli = Cli::parse_from(["semantic_quality"]);
        assert!((cli.semantic_weight - DEFAULT_SEMANTIC_WEIGHT).abs() < f64::EPSILON);
    }

    #[test]
    fn cosine_threshold_flag_parses() {
        let cli = Cli::parse_from(["semantic_quality", "--cosine-threshold", "0.85"]);
        assert!((cli.cosine_threshold - 0.85).abs() < f64::EPSILON);
    }

    #[test]
    fn cosine_threshold_defaults_to_const() {
        use scout_result_normalizer::DEFAULT_COSINE_ROUTING_THRESHOLD;
        let cli = Cli::parse_from(["semantic_quality"]);
        assert!((cli.cosine_threshold - DEFAULT_COSINE_ROUTING_THRESHOLD).abs() < f64::EPSILON);
    }

    #[test]
    fn vectors_file_flag_parses() {
        let cli = Cli::parse_from(["semantic_quality", "--vectors-file", "vectors-bge-m3.json"]);
        assert_eq!(cli.vectors_file, "vectors-bge-m3.json");
    }

    #[test]
    fn vectors_file_defaults_to_vectors_json() {
        let cli = Cli::parse_from(["semantic_quality"]);
        assert_eq!(cli.vectors_file, "vectors.json");
    }

    #[test]
    fn prefix_mode_defaults_to_none() {
        let cli = Cli::parse_from(["semantic_quality"]);
        assert_eq!(cli.prefix_mode, super::PrefixMode::None);
    }

    #[test]
    fn prefix_mode_standard_parses() {
        let cli = Cli::parse_from(["semantic_quality", "--prefix-mode", "standard"]);
        assert_eq!(cli.prefix_mode, super::PrefixMode::Standard);
    }

    #[test]
    fn fixture_set_defaults_to_personal() {
        let cli = Cli::parse_from(["semantic_quality"]);
        assert_eq!(cli.fixture_set, super::FixtureSet::Personal);
        assert_eq!(cli.fixture_set.dir(), "fixtures/semantic-recall");
    }

    #[test]
    fn fixture_set_enterprise_parses() {
        let cli = Cli::parse_from(["semantic_quality", "--fixture-set", "enterprise"]);
        assert_eq!(cli.fixture_set, super::FixtureSet::Enterprise);
        assert_eq!(cli.fixture_set.dir(), "fixtures/enterprise-recall");
    }
}
