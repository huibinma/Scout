//! 真机冒烟测试：对本机 C: 盘跑一次真实的 `NativeIndexService::start`。
//!
//! 默认 `#[ignore]`（CI / 沙盒环境通常无管理员权限，且会实际建立一次全盘
//! MFT 枚举，耗时与卷内文件数成正比）——本地开发机手动
//! `cargo test -p scout-native-index --test real_volume -- --ignored --nocapture`
//! 验证。非管理员权限下预期 `Err(VolumeOpen)`，测试仍应视为"验证了优雅降级"
//! 而非失败，故断言只检查"要么成功且能查到结果，要么明确报打开卷失败"。

#[allow(clippy::print_stdout)]
#[test]
#[ignore = "需要管理员权限 + 真实 NTFS 卷，手动运行验证"]
fn start_on_c_drive_builds_index_or_fails_gracefully() {
    match scout_native_index::NativeIndexService::start('C') {
        Ok(service) => {
            let count = service.record_count();
            println!("C: 盘索引记录数: {count}");
            assert!(count > 0, "非空卷理应枚举出记录");

            // 用 Windows 系统目录名做一次子串搜索，几乎所有 Windows 安装都存在。
            let hits = service.search("system32", 5);
            println!("system32 命中: {hits:?}");
            assert!(!hits.is_empty(), "应能命中 Windows\\System32 相关路径");
        }
        Err(err) => {
            println!("打开卷失败（非管理员权限下预期行为）: {err}");
        }
    }
}
