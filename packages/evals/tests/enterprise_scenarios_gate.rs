//! BETA-40 收尾：企业三场景评测的可重复回归门。
//!
//! 两层：
//!
//! 1. **fixture 完整性（常跑 CI）**——queries.tsv 可解析、期望路径真实存在于
//!    materials root、期望内容落在该 subject 授权 collection 的 root 之内
//!    （否则 case 结构性不可能通过）、三场景与越权负样本覆盖齐、**每个声明的
//!    collection 都被至少一条 case 演练**（无死 collection）、**每条越权负样本的
//!    墙目标非空洞**（真实存在 + 落在某未授权 collection 内，杜绝假绿越权断言）。
//! 2. **端到端（环境变量门控）**——`SCOUT_DAEMON_BIN` + `SCOUT_MODEL_PATH`
//!    都给时跑 `enterprise_scenarios` binary 全量（真模型 + 真 daemon），退 0 且
//!    报告含 OVERALL；CI 无 GGUF / 无 llama-cpp daemon 时自动跳过（与
//!    `daemon_mode_smoke` 同款语义）。

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    clippy::panic
)]

use std::path::PathBuf;
use std::process::Command;

use scout_evals::enterprise::{
    grants_for_subject, parse_queries_tsv, Expectation, COLLECTIONS, SCENARIOS,
};

fn materials_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-materials/enterprise-scenarios-raw")
}

fn load_repo_cases() -> Vec<scout_evals::enterprise::EnterpriseCase> {
    let path = materials_root().join("expected/queries.tsv");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("读 queries.tsv 失败（{}）：{e}", path.display()));
    parse_queries_tsv(&text).expect("仓库 queries.tsv 应可解析")
}

#[test]
fn queries_tsv_parses_and_covers_three_scenarios_with_denied_cases() {
    let cases = load_repo_cases();
    assert!(cases.len() >= 20, "case 数应 ≥20，实得 {}", cases.len());
    for scenario in SCENARIOS {
        assert!(
            cases.iter().any(|c| c.scenario == scenario),
            "缺 {scenario} 场景 case"
        );
    }
    let denied = cases
        .iter()
        .filter(|c| matches!(c.expectation, Expectation::AccessDenied { .. }))
        .count();
    assert!(
        denied >= 3,
        "越权负样本应 ≥3（每场景至少一条），实得 {denied}"
    );
}

#[test]
fn expected_paths_exist_on_disk() {
    let root = materials_root();
    for case in load_repo_cases() {
        let Expectation::Hits(paths) = &case.expectation else {
            continue;
        };
        for rel in paths {
            let full = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
            assert!(
                full.is_file(),
                "case {}: 期望路径不存在：{}",
                case.id,
                full.display()
            );
        }
    }
}

/// 期望内容必须落在该 subject 授权 collection 的 root 之内——否则物理信息墙
/// 会让该 case 永远查不到（fixture 编写期错误，宜 CI 直接炸）。
#[test]
fn expected_paths_fall_within_subject_granted_collections() {
    for case in load_repo_cases() {
        let Expectation::Hits(paths) = &case.expectation else {
            continue;
        };
        let granted = grants_for_subject(&case.subject);
        let granted_roots: Vec<&str> = COLLECTIONS
            .iter()
            .filter(|(id, ..)| granted.contains(id))
            .map(|(_, _, rel_root, _)| *rel_root)
            .collect();
        for rel in paths {
            assert!(
                granted_roots
                    .iter()
                    .any(|r| rel.starts_with(&format!("{r}/"))),
                "case {}: 期望路径 {rel} 不在 subject {} 的授权 collection roots {granted_roots:?} 内",
                case.id,
                case.subject
            );
        }
    }
}

/// 每个声明的 collection 都必须被至少一条 case 实际演练——正样本期望路径落在其
/// root 内，或某条越权 case 的墙目标（`denied_target`）指向它。否则是「死
/// collection」：声明了 ACL 边界却无任何 case 触达，回归时信息墙静默失守也测不出来。
#[test]
fn every_declared_collection_is_exercised_by_a_case() {
    let cases = load_repo_cases();
    for (id, _kind, rel_root, _display) in COLLECTIONS {
        let prefix = format!("{rel_root}/");
        let covered = cases.iter().any(|c| match &c.expectation {
            Expectation::Hits(paths) => paths.iter().any(|p| p.starts_with(prefix.as_str())),
            Expectation::AccessDenied { target } => target
                .as_deref()
                .is_some_and(|t| t.starts_with(prefix.as_str())),
        });
        assert!(
            covered,
            "collection {id}（root {rel_root}）无任何 case 演练——死 collection（正样本落在其内或越权墙目标指向它，二者至少居一）"
        );
    }
}

/// 每条越权负样本必须声明一个「非空洞」的墙目标：真实存在、落在某声明 collection
/// 内、且该 collection 不在该 subject 的授权集里。缺目标 = 墙背后可能根本没内容、
/// 检索本就零命中，「被拒」名不副实；目标落在自己授权集里 = fixture 自相矛盾
/// （既授权又期望拒绝）。这两类都让越权断言退化成假绿。
#[test]
fn denied_cases_declare_nonvacuous_unauthorized_targets() {
    let root = materials_root();
    for case in load_repo_cases() {
        let Expectation::AccessDenied { target } = &case.expectation else {
            continue;
        };
        let target = target.as_deref().unwrap_or_else(|| {
            panic!(
                "越权 case {}: 未声明墙目标（expected_paths 应为 ACCESS_DENIED:<相对路径>）",
                case.id
            )
        });
        let full = root.join(target.replace('/', std::path::MAIN_SEPARATOR_STR));
        assert!(
            full.is_file(),
            "越权 case {}: 墙目标不存在于磁盘：{}",
            case.id,
            full.display()
        );
        let owner = COLLECTIONS
            .iter()
            .find(|(_, _, rel_root, _)| target.starts_with(&format!("{rel_root}/")))
            .unwrap_or_else(|| {
                panic!(
                    "越权 case {}: 墙目标 {target} 不落在任何声明 collection 内",
                    case.id
                )
            });
        let (owner_id, ..) = owner;
        assert!(
            !grants_for_subject(&case.subject).contains(owner_id),
            "越权 case {}: 墙目标 {target} 落在 subject {} 已授权的 collection {owner_id} 内——不是真墙",
            case.id,
            case.subject
        );
    }
}

/// 端到端（真 daemon + 真模型）：环境变量都给时才跑，与 `daemon_mode_smoke`
/// 同款 skip 语义。带 `--require-all` 严格闸门——2026-07-04 baseline 22/22 全过
/// （权重 10 与 3 排名逐 case 一致；O-09 图片语义 daemon 默认开后顶位命中），
/// 任何 case 回退都应在此炸出来。
#[test]
fn enterprise_scenarios_end_to_end_when_env_provided() {
    let Ok(daemon_bin) = std::env::var("SCOUT_DAEMON_BIN") else {
        eprintln!("[skip] 未设 SCOUT_DAEMON_BIN，跳过 enterprise e2e");
        return;
    };
    let Ok(model_path) = std::env::var("SCOUT_MODEL_PATH") else {
        eprintln!("[skip] 未设 SCOUT_MODEL_PATH，跳过 enterprise e2e");
        return;
    };

    let out_json = tempfile::NamedTempFile::new().expect("temp json 应能建");
    let output = Command::new(env!("CARGO_BIN_EXE_enterprise_scenarios"))
        .args([
            "--daemon-binary",
            &daemon_bin,
            "--model-path",
            &model_path,
            "--json",
            out_json.path().to_str().unwrap(),
            "--require-all",
        ])
        .output()
        .expect("enterprise_scenarios 应能跑起来");
    assert!(
        output.status.success(),
        "enterprise_scenarios 应退 0，status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("OVERALL"), "报告应含 OVERALL 行：{stdout}");
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out_json.path()).unwrap())
            .expect("JSON 报告应合法");
    assert!(report["outcomes"].as_array().is_some_and(|a| a.len() >= 20));
}
