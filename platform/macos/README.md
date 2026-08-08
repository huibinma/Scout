# platform/macos

macOS 特定的平台适配代码与分发资产。

**状态**：PROTO-04A location resolver v0.1 已完成。

## 计划职责

- **签名与公证**：Developer ID Application 证书、Hardened Runtime、entitlements、Notarization 脚本、Stapler
- **分发**：DMG 制作脚本、（可选）Mac App Store 适配（App Sandbox / Privacy Manifest）
- **系统集成**：LaunchAgent plist、菜单栏图标资源
- **Spotlight FFI**：如需要 `NSMetadataQuery` 而非仅用 `mdfind`，绑定写在这里
- **Vision framework**：OCR 调用 binding（Beta 阶段）
- **权限引导**：Full Disk Access 引导 UI 与文案
- **位置解析**：`MacOsLocationResolver` 将 SearchIntent location hint 解析为绝对路径。

## 已实现

- `下载/桌面/文稿/图片/影片/音乐` → home 目录下对应系统目录。
- `截屏/截图/screenshots` → 优先读取 `com.apple.screencapture` 的 `location` 偏好；失败 fallback 到 `~/Desktop` 与 `~/Pictures/Screenshots`。
- 路径使用 `PathBuf` 与 home dir 环境 API 生成，不拼接平台分隔符字符串。

## 关键约束

- 默认不申请 `com.apple.security.files.all`
- entitlements 最小化
- 详见 [docs/Scout项目注意事项与风险清单.md §9.2 §3.4](../../docs/Scout项目注意事项与风险清单.md)
