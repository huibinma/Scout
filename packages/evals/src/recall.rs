//! BETA-15A 同义词召回评测：离线确定性匹配模拟 + 指标 + 门槛。
//!
//! 匹配忠实于 BETA-15D 双查询语义：组内 OR、组间 AND、大小写不敏感子串，
//! 命中域 = 文件名 + `content_terms`。不跑 Spotlight / mdfind / 模型。

use scout_search_backend::KeywordGroup;
use serde::Deserialize;

/// 合成 corpus 中的一个文件。
#[derive(Debug, Clone, Deserialize)]
pub struct CorpusFile {
    pub id: String,
    pub filename: String,
    #[serde(default)]
    pub content_terms: Vec<String>,
}

/// 文件 F 是否命中 keyword 组：组内 OR、组间 AND、大小写不敏感子串。
/// 空 groups（keyword 缺失且 gazetteer 未命中）→ 不命中任何文件。
#[must_use]
pub fn matches(groups: &[KeywordGroup], file: &CorpusFile) -> bool {
    if groups.is_empty() {
        return false;
    }
    let haystacks: Vec<String> = std::iter::once(file.filename.to_lowercase())
        .chain(file.content_terms.iter().map(|t| t.to_lowercase()))
        .collect();
    groups.iter().all(|g| {
        g.all().iter().any(|term| {
            let needle = term.to_lowercase();
            haystacks.iter().any(|h| h.contains(&needle))
        })
    })
}

/// 一个召回 case（手工标注，序列化自 cases.json）。
#[derive(Debug, Clone, Deserialize)]
pub struct RecallCase {
    pub id: String,
    pub query: String,
    pub language: String,
    pub bucket: String,
    pub expected_hits: Vec<String>,
}

/// 单 case 的命中核算。
#[derive(Debug, Clone)]
pub struct CaseOutcome {
    pub case_id: String,
    pub bucket: String,
    pub language: String,
    pub expected: usize,
    pub recalled: usize,
    pub false_positives: usize,
    pub non_expected: usize,
    pub missing: Vec<String>,
    pub extra: Vec<String>,
}

/// 门槛（对齐 ROADMAP BETA-15A 验收下限）。
pub const RECALL_GATE: f64 = 0.70;
pub const FP_GATE: f64 = 0.05;

/// 在给定 corpus 上核算一个 case：`actual_hits` 是已算好的命中 id 集合。
#[must_use]
pub fn outcome_for(
    case: &RecallCase,
    corpus: &[CorpusFile],
    actual_hits: &[String],
) -> CaseOutcome {
    let expected: std::collections::HashSet<&str> =
        case.expected_hits.iter().map(String::as_str).collect();
    let actual: std::collections::HashSet<&str> = actual_hits.iter().map(String::as_str).collect();
    let recalled = expected.iter().filter(|id| actual.contains(*id)).count();
    let false_positives = actual.iter().filter(|id| !expected.contains(*id)).count();
    let mut missing: Vec<String> = expected
        .iter()
        .filter(|id| !actual.contains(*id))
        .map(|s| (*s).to_owned())
        .collect();
    let mut extra: Vec<String> = actual
        .iter()
        .filter(|id| !expected.contains(*id))
        .map(|s| (*s).to_owned())
        .collect();
    missing.sort();
    extra.sort();
    CaseOutcome {
        case_id: case.id.clone(),
        bucket: case.bucket.clone(),
        language: case.language.clone(),
        expected: case.expected_hits.len(),
        recalled,
        false_positives,
        non_expected: corpus.len() - case.expected_hits.len(),
        missing,
        extra,
    }
}

/// 聚合报告。
#[derive(Debug, Clone)]
pub struct RecallReport {
    pub outcomes: Vec<CaseOutcome>,
    pub corpus_size: usize,
}

impl RecallReport {
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn recall_rate(&self) -> f64 {
        let exp: usize = self.outcomes.iter().map(|o| o.expected).sum();
        let rec: usize = self.outcomes.iter().map(|o| o.recalled).sum();
        if exp == 0 {
            1.0
        } else {
            rec as f64 / exp as f64
        }
    }

    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn false_positive_rate(&self) -> f64 {
        let denom: usize = self.outcomes.iter().map(|o| o.non_expected).sum();
        let fp: usize = self.outcomes.iter().map(|o| o.false_positives).sum();
        if denom == 0 {
            0.0
        } else {
            fp as f64 / denom as f64
        }
    }

    /// 按 key（bucket 或 language）算召回率。
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn recall_by<F: Fn(&CaseOutcome) -> &str>(&self, key: F) -> Vec<(String, f64)> {
        use std::collections::BTreeMap;
        let mut acc: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for o in &self.outcomes {
            let e = acc.entry(key(o).to_owned()).or_insert((0, 0));
            e.0 += o.expected;
            e.1 += o.recalled;
        }
        acc.into_iter()
            .map(|(k, (exp, rec))| {
                (
                    k,
                    if exp == 0 {
                        1.0
                    } else {
                        rec as f64 / exp as f64
                    },
                )
            })
            .collect()
    }

    #[must_use]
    pub fn passes_gate(&self) -> bool {
        self.recall_rate() >= RECALL_GATE && self.false_positive_rate() <= FP_GATE
    }
}

use scout_harness::SynonymExpander;
use std::collections::HashSet;
use std::path::Path;

/// 跑全管线：对每 case `parse → expand → matches` → 聚合报告。
#[must_use]
pub fn run_recall(
    expander: &dyn SynonymExpander,
    corpus: &[CorpusFile],
    cases: &[RecallCase],
) -> RecallReport {
    let outcomes = cases
        .iter()
        .map(|case| {
            let intent = scout_intent_parser::parse(&case.query);
            let ex = expander.expand(intent, &case.query);
            let groups = &ex.keyword_groups;
            let actual_hits: Vec<String> = corpus
                .iter()
                .filter(|f| matches(groups, f))
                .map(|f| f.id.clone())
                .collect();
            outcome_for(case, corpus, &actual_hits)
        })
        .collect();
    RecallReport {
        outcomes,
        corpus_size: corpus.len(),
    }
}

/// 从 JSON 文件加载 `corpus`。
pub fn load_corpus(path: &Path) -> anyhow::Result<Vec<CorpusFile>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读 corpus {} 失败: {e}", path.display()))?;
    Ok(serde_json::from_str(&raw)?)
}

/// 从 JSON 文件加载 `cases`。
pub fn load_cases(path: &Path) -> anyhow::Result<Vec<RecallCase>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读 cases {} 失败: {e}", path.display()))?;
    Ok(serde_json::from_str(&raw)?)
}

/// 校验引用完整性：`corpus`/`cases` id 无重复；`cases` 引用的 file id 必须存在于 `corpus`。
pub fn check_integrity(corpus: &[CorpusFile], cases: &[RecallCase]) -> anyhow::Result<()> {
    let mut seen_files: HashSet<&str> = HashSet::new();
    for f in corpus {
        if !seen_files.insert(f.id.as_str()) {
            anyhow::bail!("corpus 重复 file id: {}", f.id);
        }
    }
    let mut seen_cases: HashSet<&str> = HashSet::new();
    for c in cases {
        if !seen_cases.insert(c.id.as_str()) {
            anyhow::bail!("cases 重复 case id: {}", c.id);
        }
        for hit in &c.expected_hits {
            if !seen_files.contains(hit.as_str()) {
                anyhow::bail!("case {} 引用了不存在的 file id: {}", c.id, hit);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn file(id: &str, name: &str) -> CorpusFile {
        CorpusFile {
            id: id.into(),
            filename: name.into(),
            content_terms: vec![],
        }
    }
    fn group(head: &str, syns: &[&str]) -> KeywordGroup {
        KeywordGroup {
            head: head.into(),
            synonyms: syns.iter().map(|s| (*s).into()).collect(),
        }
    }

    #[test]
    fn synonym_member_substring_hits() {
        // group {工作汇报, 述职} 命中名为 述职.ppt 的文件（同义词召回的核心）
        let groups = vec![group("工作汇报", &["述职", "年度总结"])];
        assert!(matches(&groups, &file("f1", "述职.ppt")));
    }

    #[test]
    fn no_member_substring_misses() {
        let groups = vec![group("工作汇报", &["述职"])];
        assert!(!matches(&groups, &file("f2", "项目计划.docx")));
    }

    #[test]
    fn case_insensitive_substring() {
        let groups = vec![group("resume", &["cv"])];
        assert!(matches(&groups, &file("f3", "My_CV.docx")));
        assert!(matches(&groups, &file("f4", "RESUME_final.pdf")));
    }

    #[test]
    fn cross_group_is_and() {
        // 两组：必须都命中。文件名只含其一 → 不命中。
        let groups = vec![group("工作汇报", &["述职"]), group("ppt", &[])];
        assert!(!matches(&groups, &file("f5", "述职.docx")));
        assert!(matches(&groups, &file("f6", "述职.ppt")));
    }

    #[test]
    fn empty_groups_never_match() {
        assert!(!matches(&[], &file("f7", "述职.ppt")));
    }

    #[test]
    fn content_terms_hit() {
        let groups = vec![group("合同", &["协议"])];
        let f = CorpusFile {
            id: "f8".into(),
            filename: "doc-001.pdf".into(),
            content_terms: vec!["这是一份协议".into()],
        };
        assert!(matches(&groups, &f));
    }

    fn case(id: &str, bucket: &str, lang: &str, expected: &[&str]) -> RecallCase {
        RecallCase {
            id: id.into(),
            query: "q".into(),
            language: lang.into(),
            bucket: bucket.into(),
            expected_hits: expected.iter().map(|s| (*s).into()).collect(),
        }
    }

    #[test]
    fn full_recall_zero_fp() {
        let corpus = vec![file("a", "述职.ppt"), file("b", "干扰.txt")];
        let c = case("c1", "office", "zh", &["a"]);
        let o = outcome_for(&c, &corpus, &["a".into()]);
        let report = RecallReport {
            outcomes: vec![o],
            corpus_size: 2,
        };
        assert!((report.recall_rate() - 1.0).abs() < 1e-9);
        assert!(report.false_positive_rate().abs() < 1e-9);
        assert!(report.passes_gate());
    }

    #[test]
    fn miss_lowers_recall() {
        let corpus = vec![file("a", "x"), file("b", "y")];
        let c = case("c1", "office", "zh", &["a", "b"]);
        let o = outcome_for(&c, &corpus, &["a".into()]); // 漏 b
        let report = RecallReport {
            outcomes: vec![o],
            corpus_size: 2,
        };
        assert!((report.recall_rate() - 0.5).abs() < 1e-9);
        assert!(!report.passes_gate()); // 0.5 < 0.70
    }

    #[test]
    fn extra_hit_raises_fp() {
        let corpus = vec![file("a", "x"), file("b", "y"), file("c", "z")];
        let c = case("c1", "office", "zh", &["a"]);
        let o = outcome_for(&c, &corpus, &["a".into(), "b".into()]); // b 是假阳
        let report = RecallReport {
            outcomes: vec![o],
            corpus_size: 3,
        };
        // non_expected = 3 - 1 = 2; fp = 1 → 0.5
        assert!((report.false_positive_rate() - 0.5).abs() < 1e-9);
        assert!(!report.passes_gate());
    }

    #[test]
    fn recall_by_bucket_splits() {
        let corpus = vec![file("a", "x"), file("b", "y")];
        let o1 = outcome_for(&case("c1", "office", "zh", &["a"]), &corpus, &["a".into()]);
        let o2 = outcome_for(&case("c2", "document", "zh", &["b"]), &corpus, &[]);
        let report = RecallReport {
            outcomes: vec![o1, o2],
            corpus_size: 2,
        };
        let by = report.recall_by(|o| o.bucket.as_str());
        assert_eq!(by, vec![("document".into(), 0.0), ("office".into(), 1.0)]);
    }

    #[test]
    fn integrity_passes_on_valid() {
        let corpus = vec![file("a", "述职.ppt"), file("b", "干扰.txt")];
        let cases = vec![case("c1", "office", "zh", &["a"])];
        assert!(check_integrity(&corpus, &cases).is_ok());
    }

    #[test]
    fn integrity_catches_dangling_ref() {
        let corpus = vec![file("a", "述职.ppt")];
        let cases = vec![case("c1", "office", "zh", &["missing-id"])];
        assert!(check_integrity(&corpus, &cases).is_err());
    }

    #[test]
    fn integrity_catches_dup_file_id() {
        let corpus = vec![file("a", "x"), file("a", "y")];
        let cases: Vec<RecallCase> = vec![];
        assert!(check_integrity(&corpus, &cases).is_err());
    }

    #[test]
    fn corpus_deserializes_content_terms_default() {
        let json = r#"[{"id":"a","filename":"x.pdf"}]"#;
        let corpus: Vec<CorpusFile> = serde_json::from_str(json).unwrap();
        assert_eq!(corpus[0].content_terms.len(), 0);
    }
}
