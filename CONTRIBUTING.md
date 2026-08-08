# 为 Scout 做贡献

感谢你的兴趣！Scout 是本地优先的跨平台语义文件搜索（macOS + Windows），MIT 许可开源。

## 许可条款

除非你明确声明，你有意提交给本项目的任何贡献均按 **MIT 许可**授权，无附加条款。提交 PR 即视为同意此条款。

## 开始之前

- 先读 [README.md](./README.md)（仓库结构）与 [PROJECT.md](./PROJECT.md)（定位与**范围红线**）。
- 特别注意 PROJECT.md「不做什么」：内容摘要/比对/起草等**分析层能力不自建**（走 MCP daemon + 外部 LLM 组合）；提这类 feature 会被引导到 MCP 工作流而非产品内实现。
- 大的改动请先开 issue 讨论再动手，避免白做。

## 开发环境

- **Rust**：stable，版本以 [rust-toolchain.toml](./rust-toolchain.toml) 为准；workspace 全局 `unsafe_code = "forbid"`。
- **前端**：Node 18+ / npm；Tauri 2 前置依赖见 [Tauri 官方文档](https://tauri.app/start/prerequisites/)。
- **Windows 上手**：见 [docs/windows-setup.md](./docs/windows-setup.md)；带 llama 的 daemon 一律用 `scripts/build-scoutd-llama.bat` 构建。
- **一次性设置**：clone 后运行 `git config core.hooksPath scripts/hooks`（启用文档体积等 pre-commit 闸门）。

## 提交前的验证闸门

PR 合入要求全绿（CI 亦会检查）：

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
# 前端（apps/desktop）：
npm run build            # tsc + vite build
```

- **动了 `packages/intent-parser` / evals**：必须跑 evals 套件并保证 v0.5 / v0.9 **逐 case 零回归**（详 [packages/evals README](./packages/evals/README.md)）；标注（coverage）变更需在 PR 里说明依据。
- **引入/移除依赖**：当场更新 [docs/third-party-licenses.md](./docs/third-party-licenses.md) 台账（版本 / License / 是否随产品分发）。
- **平台隔离**：通用层不得 import 平台 API——平台代码只进 `platform/*` 与对应 `search-backends/*`。

## 代码与提交约定

- 代码标识符用**英文**；注释以**中文为主**（英文亦可，与所在文件保持一致）。
- commit message 中文或英文均可，简洁说明主题；不要加 AI 工具自夸签名。
- 仓库内文档互引一律**相对路径**。

## 隐私红线（必守）

- 默认不联网、无遥测（见 [PRIVACY.md](./PRIVACY.md)）——任何新增网络请求必须是用户显式触发，并在 PR 里说明。
- 测试语料一律合成/虚构；**不得提交真实个人数据**（真实路径、真实文档、密钥）。日志/审计等敏感面改动需同步 PRIVACY.md。

## 报告问题

- Bug / feature 请用 issue 模板。
- **安全漏洞**请勿开公开 issue，走 GitHub Security Advisories（仓库 Security → Report a vulnerability）私下报告。
