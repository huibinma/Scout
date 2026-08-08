# platform/windows

Windows 平台适配层。

## 当前状态

MVP-13 已实现基于 Windows Known Folder API 的 `WindowsLocationResolver`：

- `Documents` / `Downloads` / `Desktop` / `Pictures` / `Music` / `Videos` / `Screenshots` hint 映射到真实 Windows Known Folder GUID。
- 通过 `SHGetKnownFolderPath` 获取用户系统上的真实路径。
- 特殊处理 `Screenshots`：解析 `Pictures` 路径后附加 `Screenshots` 子目录。
- 包含针对 macOS/Linux 开发环境的 Stub 实现，确保跨平台编译与基础逻辑测试。

## 待实测验证

当前实现在 Windows 11 上尚待最终物理验证：
- [ ] 验证 OneDrive 重定向下的路径解析是否符合预期（SHGetKnownFolderPath 理论上会自动处理）。
- [ ] 验证企业策略下 Known Folder 重定向。
- [ ] 验证多语言版 Windows 下的路径映射。

## 关键依赖

- `windows` crate: 使用 `Win32_UI_Shell`, `Win32_Foundation`, `Win32_System_Com` 特性。

