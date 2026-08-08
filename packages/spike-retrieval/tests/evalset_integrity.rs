//! BETA-26 评测集完整性守护：cases.json 结构合法、doc_id 全在语料内、grade 合规、bucket 合法。
//! 数据文件（corpus.jsonl / cases.json）是 gitignored 的真实个人数据，本测试在数据缺失时跳过而非失败，
//! 以免在干净检出（无语料）的机器上误红。

use spike_retrieval::{CorpusDoc, EvalCase};
use std::collections::HashSet;
use std::path::Path;

const CORPUS: &str = "fixtures/corpus.jsonl";
const CASES: &str = "fixtures/evalset/cases.json";

fn load_corpus_ids() -> Option<HashSet<String>> {
    let txt = std::fs::read_to_string(CORPUS).ok()?;
    Some(
        txt.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str::<CorpusDoc>(l)
                    .expect("corpus 行解析失败")
                    .id
            })
            .collect(),
    )
}

fn load_cases() -> Option<Vec<EvalCase>> {
    let txt = std::fs::read_to_string(CASES).ok()?;
    Some(serde_json::from_str(&txt).expect("cases.json 解析失败"))
}

#[test]
fn evalset_is_well_formed() {
    if !Path::new(CORPUS).exists() || !Path::new(CASES).exists() {
        eprintln!("跳过：语料或评测集缺失（gitignored，需先跑 build-corpus + 起草 cases.json）");
        return;
    }
    let corpus = load_corpus_ids().expect("corpus 读取失败");
    let cases = load_cases().expect("cases 读取失败");

    assert!(cases.len() >= 50, "评测集至少 50 条，实际 {}", cases.len());

    let valid_buckets = [
        "synonym",
        "concept",
        "crosslang",
        "ocr",
        "content-not-name",
        "exact-name",
    ];
    let mut ids = HashSet::new();
    for c in &cases {
        assert!(ids.insert(c.id.clone()), "case id 重复: {}", c.id);
        assert!(
            valid_buckets.contains(&c.bucket.as_str()),
            "非法 bucket: {} (case {})",
            c.bucket,
            c.id
        );
        assert!(!c.query.trim().is_empty(), "case {} query 为空", c.id);
        assert!(!c.relevant.is_empty(), "case {} 无相关文件", c.id);
        let mut seen_docs = HashSet::new();
        for r in &c.relevant {
            assert!(
                corpus.contains(&r.doc_id),
                "case {} 引用了语料外的 doc_id {}",
                c.id,
                r.doc_id
            );
            assert!(
                (1..=3).contains(&r.grade),
                "case {} grade 越界: {}",
                c.id,
                r.grade
            );
            assert!(
                seen_docs.insert(r.doc_id.clone()),
                "case {} 内 doc_id {} 重复",
                c.id,
                r.doc_id
            );
        }
    }
    eprintln!(
        "✅ 评测集完整：{} 条 case，全部 doc_id 命中语料（语料 {} 篇）",
        cases.len(),
        corpus.len()
    );
}
