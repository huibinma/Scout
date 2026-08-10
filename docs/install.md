# Scout 安装指南

> Scout 是公开的 MIT 开源项目。Windows 正在申请 SignPath Foundation 免费开源代码签名；申请获批并完成流水线切换后的新 Release 将带 Authenticode 签名，旧 Release 仍可能未签名。macOS 暂未加入 Apple Developer Program，仍未签名、未公证。

## Windows

### 下载安装

1. 打开公开的 [Releases](https://github.com/huibinma/Scout/releases) 页面，下载最新的 `Scout_x.y.z_x64-setup.exe`（NSIS 安装包）。
2. 右键安装包 →「属性」→「数字签名」，或在 PowerShell 中执行：
   ```powershell
   Get-AuthenticodeSignature .\Scout_x.y.z_x64-setup.exe | Format-List Status,SignerCertificate
   ```
   - SignPath 流水线切换后的 Release：预期 `Status` 为 `Valid`，证书发布者显示 SignPath Foundation。
   - 旧的未签名 Release：SmartScreen 仍可能显示「Windows 已保护你的电脑」，可点「更多信息 → 仍要运行」，并务必先核对 SHA256。
3. 按向导完成安装。

Scout 的签名范围、团队角色、隐私边界和发布流程见 [Code signing policy](../CODE_SIGNING.md)。Free code signing provided by [SignPath.io](https://about.signpath.io/), certificate by [SignPath Foundation](https://signpath.org/)。

### 校验下载（建议）

```powershell
Get-FileHash .\Scout_x.y.z_x64-setup.exe -Algorithm SHA256
```

与 Release 页面对应资产显示的 SHA256 摘要比对一致即可。

### 升级 / 卸载

- **升级**：直接运行新版安装包覆盖安装，索引、模型、设置全部保留（安装器带升级守卫）。
- **卸载**：控制面板卸载即可，卸载器会自动清除索引、模型、日志、审计与搜索历史（`settings.json` 保留）；也可先在应用内「隐私 → 卸载清理」手动执行。

## macOS

到 [Releases](https://github.com/huibinma/Scout/releases) 下载最新的 `Scout_x.y.z_aarch64.dmg`（仅 Apple Silicon / arm64，CI 在 macos-14 runner 上以 `aarch64-apple-darwin` 为 target 构建；Intel Mac 用户请从源码构建）。

未签名、未公证的 app（仅带 Tauri 默认的 ad-hoc 签名）首次打开会被 **Gatekeeper** 拦截，具体提示文案因系统版本而异：

- 「无法打开，因为它来自身份不明的开发者」/「未能验证不包含恶意软件」——较常见于 macOS 14 及更早。
- **「"Scout" 已损坏，无法打开。你应该将它移到废纸篓」**——macOS 13 (Ventura) 之后、尤其是较新系统（如 macOS 15/26）上更常见，右键「打开」这条旁路经常**不生效**。这条提示不代表文件真的损坏或被篡改，只是 Gatekeeper 对「无可信 Developer ID 签名链 + 下载隔离属性」组合的更严格拦截——请优先用下面的命令行方式放行，并可配合 SHA256 校验下载完整性以确认文件本身完好。

任选其一放行：

- **命令行（推荐，所有版本通用，尤其是遇到「已损坏」提示时）**：
  ```bash
  xattr -dr com.apple.quarantine /Applications/Scout.app
  ```
  若还没安装、只想直接对 DMG 去隔离属性：`xattr -dr com.apple.quarantine ~/Downloads/Scout_x.y.z_aarch64.dmg`。
- **macOS 14 及更早**：在访达中**右键（Control-点按）app → 打开** → 弹窗中再点「打开」。
- **macOS 15 (Sequoia) 及更新**：直接双击会被拒且右键不再提供旁路——先双击一次，然后到 **系统设置 → 隐私与安全性**，在页面底部找到 Scout 条目点「**仍要打开**」（若仍报「已损坏」而非「未知开发者」，此路径可能不出现，请改用命令行）。

以上任一操作只需做一次。

## 从源码构建

前置：Rust stable（版本见 [rust-toolchain.toml](../rust-toolchain.toml)）、Node 20+、[Tauri 2 前置依赖](https://tauri.app/start/prerequisites/)（Windows 另需 cmake，用于 llama.cpp）。

```bash
git clone https://github.com/huibinma/Scout.git
cd Scout/apps/desktop
npm install
npm run tauri build -- --features model-fallback,semantic-recall
```

产物在 `apps/desktop/src-tauri/target/release/bundle/`。源码构建的 app 不带下载隔离属性，无 Gatekeeper/SmartScreen 提示。

## 模型文件（可选）

安装后首次运行，「快速入门」提供两个本地模型的**一键下载**（来源 huggingface.co，这是应用唯一的联网行为，见 [PRIVACY.md](../PRIVACY.md)）：

- **embedding 模型** —— 启用「按意思找 / 跨语言」语义召回；缺失则降级纯关键词（FTS）搜索。
- **生成模型** —— 复杂查询的 AI 解析 fallback；缺失则降级规则解析。

两者都不装也能正常使用。离线环境可手动放置 GGUF 文件到数据目录 `models/`（Windows：`%APPDATA%\Scout\models\`）。

## 包管理器渠道（规划中）

winget / Scoop / Homebrew 渠道在评估推进中（[渠道评估](reviews/beta-10-distribution-channels-2026-07-04.md)），上线后本文更新一键安装命令。
