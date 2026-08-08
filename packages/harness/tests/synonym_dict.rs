//! 集成测试：仓内 shipped 同义词词典必须能通过 lint。
//!
//! 这把"词典本身的格式正确性"锁进 CI，防止 PR 时词典写错才被发现。

#![allow(clippy::expect_used)]

use scout_harness::YamlSynonymExpander;
use std::path::PathBuf;

#[test]
fn shipped_dicts_pass_lint() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root expected 2 levels above harness Cargo.toml")
        .to_path_buf();
    let zh = root.join("resources/synonyms/zh.yaml");
    let en = root.join("resources/synonyms/en.yaml");
    assert!(zh.exists(), "zh.yaml not found at {}", zh.display());
    assert!(en.exists(), "en.yaml not found at {}", en.display());

    YamlSynonymExpander::from_paths(&zh, &en).expect("仓内词典必须 lint pass");
}
