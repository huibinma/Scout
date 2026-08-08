//! BETA-37 eml 提取集成测试：BETA-41 企业 fixture 的 6 封 eml 全数走真实提取管线。
//!
//! 纯 Rust 无装机依赖（mail-parser 解析 + txt 附件递交现有提取器），**常跑不 --ignored**。
//! 断言口径：subject → title、from → author、正文/headers 头块进 body 可检索、
//! 带附件的两封（e00035 / e00074 / e00093）附件正文并入 body。

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use scout_indexer::{extract_document, DocumentIndex, DocumentQuery};

/// BETA-41 fixture 文件根（`packages/evals/fixtures/enterprise-recall/files/`）。
fn fixture_files(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../evals/fixtures/enterprise-recall/files")
        .join(rel)
}

/// 一条 eml fixture 用例：(相对路径, 期望 title, 期望 author 含, body 关键词, 附件段关键词——空表示无附件)。
type EmlCase = (
    &'static str,
    &'static str,
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
);

const EML_FIXTURES: &[EmlCase] = &[
    (
        "audit/e00035-approval-chain.eml",
        "猎户座采购项目——办公一体机与耗材采购审批",
        "procurement@example.com",
        &["晨星办公用品", "三个工作日完成会签"],
        &[
            "[附件 e00038-procurement-contract.txt]",
            "两年质保",
            "合同价款按报价单执行",
        ],
    ),
    (
        "audit/e00036-payment-hold.eml",
        "关于猎户座项目第二笔付款的疑点",
        "finance@example.com",
        &["收款账户与合同约定账户不一致", "暂缓付款"],
        &[],
    ),
    (
        "audit/e00050-tipline.eml",
        "关于行政采购的情况反映",
        "tipline@example.com",
        &["过从甚密", "请审计部门核实"],
        &[],
    ),
    (
        "audit/e00037-vendor-quotation.eml",
        "Quotation for Project Orion office equipment",
        "sales@example.com",
        &["quotation", "consumables package"],
        &[],
    ),
    (
        "offboarding/e00074-handover.eml",
        "离职交接安排",
        "lishili@example.com",
        &["灯塔项目文档", "鲲鹏结算值班"],
        &[
            "[附件 e00076-db-account-list.txt]",
            "口令一律走密码保管系统移交",
        ],
    ),
    (
        "offboarding/e00093-open-bugs.eml",
        "离职前遗留问题说明",
        "lishili@example.com",
        &["三个未结缺陷", "限流规则热更新"],
        &[
            "[附件 e00094-open-bugs-detail.txt]",
            "建复合索引",
            "配置下发与规则加载竞态",
        ],
    ),
];

#[test]
fn beta41_eml_fixtures_extract_headers_body_attachments() {
    for (rel, title, author_contains, body_keywords, attachment_keywords) in EML_FIXTURES {
        let doc = extract_document(&fixture_files(rel), 0)
            .unwrap_or_else(|e| panic!("{rel} 提取应成功: {e}"));
        assert_eq!(doc.entry.doc_type, "eml", "{rel}");
        assert_eq!(
            doc.entry.title.as_deref(),
            Some(*title),
            "{rel} subject 应映射到 title"
        );
        let author = doc.entry.author.as_deref().unwrap_or_default();
        assert!(
            author.contains(author_contains),
            "{rel} from 应映射到 author，期望含 {author_contains}，实得 {author}"
        );
        // headers 头块进 body（可 FTS）。
        assert!(doc.body.contains("From: "), "{rel} body 应含 From 头块行");
        assert!(
            doc.body.contains("Date: 2026-07-02T10:00:00+08:00"),
            "{rel} body 应含解析后的 Date 行"
        );
        for kw in *body_keywords {
            assert!(doc.body.contains(kw), "{rel} 正文应含关键词「{kw}」");
        }
        for kw in *attachment_keywords {
            assert!(doc.body.contains(kw), "{rel} 附件段应含「{kw}」");
        }
        // 邮件无页概念：passages / failed_pages 恒空（不进 BETA-35 双表）。
        assert!(doc.passages.is_empty(), "{rel}");
        assert!(doc.failed_pages.is_empty(), "{rel}");
    }
}

#[test]
fn eml_indexed_and_hit_via_fts() {
    // 端到端：eml 目录进增量索引（DOC_EXTS 白名单生效）→ FTS 关键词命中。
    let dir = tempfile::tempdir().unwrap();
    for rel in [
        "offboarding/e00074-handover.eml",
        "audit/e00036-payment-hold.eml",
    ] {
        let src = fixture_files(rel);
        std::fs::copy(&src, dir.path().join(src.file_name().unwrap())).unwrap();
    }
    let index = DocumentIndex::open_in_memory().unwrap();
    let stats = index.index_dirs(&[dir.path().to_path_buf()]).unwrap();
    assert_eq!(stats.added, 2, "两封 eml 应进索引");
    assert_eq!(stats.failed, 0);

    // 附件正文关键词可命中（附件文本并入邮件 body）。
    let hits = index
        .query(&DocumentQuery {
            text: Some("密码保管系统".to_string()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1, "附件正文关键词应命中 e00074");
    assert!(hits[0].entry.file_name.contains("e00074"));
    assert_eq!(hits[0].entry.doc_type, "eml");
    assert_eq!(hits[0].entry.title.as_deref(), Some("离职交接安排"));

    // subject（title 列）可命中。
    let hits = index
        .query(&DocumentQuery {
            text: Some("第二笔付款的疑点".to_string()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1, "subject 关键词应命中 e00036");
    assert!(hits[0].entry.file_name.contains("e00036"));
}
