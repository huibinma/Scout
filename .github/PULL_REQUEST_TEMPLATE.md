## 变更说明

<!-- 做了什么、为什么；关联 issue 用 Fixes #N -->

## 验证清单

- [ ] `cargo fmt --check` 通过
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` 通过
- [ ] `cargo test --workspace` 全绿
- [ ] 前端改动：`npm run build`（tsc + vite）通过（未动前端可勾选跳过）
- [ ] 动了 intent-parser / evals：v0.5 与 v0.9 evals **逐 case 零回归**（附对比结论）
- [ ] 引入/移除依赖：已更新 [docs/third-party-licenses.md](../docs/third-party-licenses.md)
- [ ] 新增网络请求 / 敏感数据面改动：已说明并同步 [PRIVACY.md](../PRIVACY.md)（无此类改动可勾选跳过）

## 许可确认

- [ ] 我同意本贡献按 **MIT 许可**授权（见 [CONTRIBUTING.md](../CONTRIBUTING.md)）
