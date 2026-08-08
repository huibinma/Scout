//! 合成语料 / 评测集 / 向量缓存的类型、加载、完整性。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// 合成文档（含正文，语义召回对内容）。
///
/// 三个 `Option` 字段为 BETA-41 企业场景标签（`fixtures/enterprise-recall/`），
/// 个人场景 fixture（`fixtures/semantic-recall/`）缺省 `None`、序列化时跳过，
/// 现有 corpus.json / vectors.json / baseline.json 逐字节不受影响。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticDoc {
    pub doc_id: String,
    pub lang: String,
    pub title: String,
    pub body: String,
    /// 企业场景：`lawfirm | audit | offboarding`（BETA-41）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario: Option<String>,
    /// 材料形态：`scanned | email | attachment | plain`（BETA-41）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc_type: Option<String>,
    /// 近重复组 id：同组 = 同一材料的近似副本（BETA-41，服务 BETA-38 doc identity）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dup_group: Option<String>,
}

/// 分级相关文档。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelevantDoc {
    pub doc_id: String,
    pub grade: u8,
}

/// 分级相关性评测 case。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticCase {
    pub id: String,
    pub bucket: String,
    pub query: String,
    pub relevant: Vec<RelevantDoc>,
}

/// 缓存向量（合成文本 embedding，无 PII，可提交）。`BTreeMap` 保序确定性。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorCache {
    pub model_id: String,
    pub dim: usize,
    pub doc_vectors: BTreeMap<String, Vec<f32>>,
    pub query_vectors: BTreeMap<String, Vec<f32>>,
}

/// 合法桶（个人场景，`fixtures/semantic-recall/`）。
pub const BUCKETS: &[&str] = &[
    "synonym",
    "concept",
    "crosslang",
    "content-not-name",
    "exact-name",
];

/// 合法桶（企业场景，`fixtures/enterprise-recall/`，BETA-41）。
pub const ENTERPRISE_BUCKETS: &[&str] = &[
    "scanned-pdf",
    "email",
    "attachment",
    "crosslang-alias",
    "near-dup",
];

/// 企业场景合法 `scenario` 值（BETA-41 三场景）。
pub const ENTERPRISE_SCENARIOS: &[&str] = &["lawfirm", "audit", "offboarding"];

/// 企业场景合法 `doc_type` 值（BETA-41 材料形态）。
pub const ENTERPRISE_DOC_TYPES: &[&str] = &["scanned", "email", "attachment", "plain"];

pub fn load_corpus(path: &Path) -> anyhow::Result<Vec<SemanticDoc>> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

pub fn load_cases(path: &Path) -> anyhow::Result<Vec<SemanticCase>> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

pub fn load_vectors(path: &Path) -> anyhow::Result<VectorCache> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// 语料 + 评测集引用一致性（个人场景桶集）。
pub fn check_integrity(corpus: &[SemanticDoc], cases: &[SemanticCase]) -> anyhow::Result<()> {
    check_integrity_with_buckets(corpus, cases, BUCKETS)
}

/// 语料 + 评测集引用一致性，桶集参数化（BETA-41 起个人/企业两套桶共用）。
pub fn check_integrity_with_buckets(
    corpus: &[SemanticDoc],
    cases: &[SemanticCase],
    buckets: &[&str],
) -> anyhow::Result<()> {
    use std::collections::HashSet;
    let doc_ids: HashSet<&str> = corpus.iter().map(|d| d.doc_id.as_str()).collect();
    anyhow::ensure!(doc_ids.len() == corpus.len(), "corpus doc_id 重复");

    let mut case_ids = HashSet::new();
    for c in cases {
        anyhow::ensure!(case_ids.insert(c.id.as_str()), "case id 重复: {}", c.id);
        anyhow::ensure!(!c.query.trim().is_empty(), "case {} query 空", c.id);
        anyhow::ensure!(
            buckets.contains(&c.bucket.as_str()),
            "case {} 非法桶 {}",
            c.id,
            c.bucket
        );
        anyhow::ensure!(!c.relevant.is_empty(), "case {} relevant 空", c.id);
        let mut seen = HashSet::new();
        for r in &c.relevant {
            anyhow::ensure!(
                (1..=3).contains(&r.grade),
                "case {} grade 越界 {}",
                c.id,
                r.grade
            );
            anyhow::ensure!(
                doc_ids.contains(r.doc_id.as_str()),
                "case {} 引用未知 doc {}",
                c.id,
                r.doc_id
            );
            anyhow::ensure!(
                seen.insert(r.doc_id.as_str()),
                "case {} doc_id 重复 {}",
                c.id,
                r.doc_id
            );
        }
    }
    Ok(())
}

/// 企业场景 fixture 完整性（BETA-41）：基础一致性（enterprise 桶集）之上，
/// 加 scenario / `doc_type` 必填且合法、`dup_group` 组内 ≥ 2 篇、隐私启发式
/// （全文邮箱域名只允许 example.com / example.org——「合成集入仓做 CI 门控」红线的机器可查部分）。
pub fn check_enterprise_integrity(
    corpus: &[SemanticDoc],
    cases: &[SemanticCase],
) -> anyhow::Result<()> {
    use std::collections::HashMap;
    check_integrity_with_buckets(corpus, cases, ENTERPRISE_BUCKETS)?;

    let mut group_sizes: HashMap<&str, usize> = HashMap::new();
    for d in corpus {
        let scenario = d
            .scenario
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("doc {} 缺 scenario", d.doc_id))?;
        anyhow::ensure!(
            ENTERPRISE_SCENARIOS.contains(&scenario),
            "doc {} 非法 scenario {}",
            d.doc_id,
            scenario
        );
        let doc_type = d
            .doc_type
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("doc {} 缺 doc_type", d.doc_id))?;
        anyhow::ensure!(
            ENTERPRISE_DOC_TYPES.contains(&doc_type),
            "doc {} 非法 doc_type {}",
            d.doc_id,
            doc_type
        );
        if let Some(g) = d.dup_group.as_deref() {
            *group_sizes.entry(g).or_insert(0) += 1;
        }
    }
    for (g, n) in &group_sizes {
        anyhow::ensure!(*n >= 2, "dup_group {g} 只有 {n} 篇（近重复组应 ≥ 2）");
    }

    let full_text = corpus
        .iter()
        .flat_map(|d| [d.title.as_str(), d.body.as_str()])
        .chain(cases.iter().map(|c| c.query.as_str()));
    for text in full_text {
        for (i, _) in text.match_indices('@') {
            let domain: String = text[i + 1..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
                .collect();
            if !(domain.starts_with("example.com") || domain.starts_with("example.org")) {
                let mut s = i.saturating_sub(20);
                while !text.is_char_boundary(s) {
                    s -= 1;
                }
                let mut e = (i + 20).min(text.len());
                while !text.is_char_boundary(e) {
                    e += 1;
                }
                anyhow::bail!(
                    "隐私红线：出现非 example.com/org 邮箱域名 {domain}（上下文：{}）",
                    &text[s..e]
                );
            }
        }
    }
    Ok(())
}

/// 向量缓存覆盖全 doc + 全 case，维度一致。
pub fn check_vectors(
    corpus: &[SemanticDoc],
    cases: &[SemanticCase],
    vc: &VectorCache,
) -> anyhow::Result<()> {
    for d in corpus {
        let v = vc
            .doc_vectors
            .get(&d.doc_id)
            .ok_or_else(|| anyhow::anyhow!("缺 doc 向量: {}", d.doc_id))?;
        anyhow::ensure!(
            v.len() == vc.dim,
            "doc {} 维度 {} != {}",
            d.doc_id,
            v.len(),
            vc.dim
        );
    }
    for c in cases {
        let v = vc
            .query_vectors
            .get(&c.id)
            .ok_or_else(|| anyhow::anyhow!("缺 query 向量: {}", c.id))?;
        anyhow::ensure!(
            v.len() == vc.dim,
            "case {} 维度 {} != {}",
            c.id,
            v.len(),
            vc.dim
        );
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn doc(id: &str, lang: &str, body: &str) -> SemanticDoc {
        SemanticDoc {
            doc_id: id.into(),
            lang: lang.into(),
            title: id.into(),
            body: body.into(),
            scenario: None,
            doc_type: None,
            dup_group: None,
        }
    }

    fn ent_doc(id: &str, scenario: &str, doc_type: &str, dup: Option<&str>) -> SemanticDoc {
        SemanticDoc {
            doc_id: id.into(),
            lang: "zh".into(),
            title: id.into(),
            body: "合成正文".into(),
            scenario: Some(scenario.into()),
            doc_type: Some(doc_type.into()),
            dup_group: dup.map(Into::into),
        }
    }
    fn case(id: &str, bucket: &str, rel: &[(&str, u8)]) -> SemanticCase {
        SemanticCase {
            id: id.into(),
            bucket: bucket.into(),
            query: format!("q-{id}"),
            relevant: rel
                .iter()
                .map(|(d, g)| RelevantDoc {
                    doc_id: (*d).into(),
                    grade: *g,
                })
                .collect(),
        }
    }

    #[test]
    fn integrity_passes_on_valid_set() {
        let corpus = vec![doc("s1", "zh", "x"), doc("s2", "en", "y")];
        let cases = vec![case("c1", "crosslang", &[("s1", 3), ("s2", 1)])];
        check_integrity(&corpus, &cases).expect("合法集应通过");
    }

    #[test]
    fn integrity_rejects_unknown_doc_id() {
        let corpus = vec![doc("s1", "zh", "x")];
        let cases = vec![case("c1", "synonym", &[("s9", 3)])];
        assert!(check_integrity(&corpus, &cases).is_err());
    }

    #[test]
    fn integrity_rejects_bad_grade() {
        let corpus = vec![doc("s1", "zh", "x")];
        let cases = vec![case("c1", "synonym", &[("s1", 4)])];
        assert!(check_integrity(&corpus, &cases).is_err());
    }

    #[test]
    fn integrity_rejects_unknown_bucket() {
        let corpus = vec![doc("s1", "zh", "x")];
        let cases = vec![case("c1", "nonsense", &[("s1", 3)])];
        assert!(check_integrity(&corpus, &cases).is_err());
    }

    #[test]
    fn optional_fields_roundtrip_and_default_to_none() {
        // 老格式（无新字段）能反序列化 → None；None 序列化时跳过（守 semantic-recall byte-equal）。
        let legacy = r#"{"doc_id":"s1","lang":"zh","title":"t","body":"b"}"#;
        let d: SemanticDoc = serde_json::from_str(legacy).expect("老格式应可反序列化");
        assert!(d.scenario.is_none() && d.doc_type.is_none() && d.dup_group.is_none());
        let out = serde_json::to_string(&d).expect("序列化");
        assert!(!out.contains("scenario"), "None 字段不应出现在序列化输出");
    }

    #[test]
    fn enterprise_integrity_passes_on_valid_set() {
        let corpus = vec![
            ent_doc("e1", "lawfirm", "scanned", Some("g1")),
            ent_doc("e2", "lawfirm", "scanned", Some("g1")),
            ent_doc("e3", "audit", "email", None),
        ];
        let cases = vec![case("c1", "scanned-pdf", &[("e1", 3), ("e2", 2)])];
        check_enterprise_integrity(&corpus, &cases).expect("合法企业集应通过");
    }

    #[test]
    fn enterprise_integrity_rejects_missing_scenario() {
        let corpus = vec![doc("e1", "zh", "x")];
        let cases = vec![case("c1", "email", &[("e1", 3)])];
        assert!(check_enterprise_integrity(&corpus, &cases).is_err());
    }

    #[test]
    fn enterprise_integrity_rejects_singleton_dup_group() {
        let corpus = vec![
            ent_doc("e1", "audit", "plain", Some("g-lonely")),
            ent_doc("e2", "audit", "plain", None),
        ];
        let cases = vec![case("c1", "near-dup", &[("e1", 3)])];
        assert!(check_enterprise_integrity(&corpus, &cases).is_err());
    }

    #[test]
    fn enterprise_integrity_rejects_personal_bucket() {
        let corpus = vec![ent_doc("e1", "audit", "plain", None)];
        let cases = vec![case("c1", "synonym", &[("e1", 3)])];
        assert!(check_enterprise_integrity(&corpus, &cases).is_err());
    }

    #[test]
    fn enterprise_integrity_rejects_non_example_email_domain() {
        let mut bad = ent_doc("e1", "audit", "email", None);
        bad.body = "发件人 someone@realcorp.cn 的邮件，含中文上下文用于边界检查".into();
        let cases = vec![case("c1", "email", &[("e1", 3)])];
        assert!(check_enterprise_integrity(&[bad], &cases).is_err());
    }

    #[test]
    fn vectors_integrity_requires_full_coverage() {
        let corpus = vec![doc("s1", "zh", "x")];
        let cases = vec![case("c1", "synonym", &[("s1", 3)])];
        let mut vc = VectorCache {
            model_id: "m".into(),
            dim: 2,
            doc_vectors: BTreeMap::new(),
            query_vectors: BTreeMap::new(),
        };
        vc.doc_vectors.insert("s1".into(), vec![1.0, 0.0]);
        vc.query_vectors.insert("c1".into(), vec![1.0, 0.0]);
        check_vectors(&corpus, &cases, &vc).expect("全覆盖应通过");
        vc.doc_vectors.clear();
        assert!(check_vectors(&corpus, &cases, &vc).is_err());
    }
}
