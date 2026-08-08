# scoop-scout

[Scout](https://github.com/huibinma/Scout)（Deep Local Search——本地语义文件搜索）的 Scoop bucket。

## 安装

```powershell
scoop bucket add scout https://github.com/huibinma/scoop-scout
scoop install scout
```

## 说明

- Scout 经 NSIS 安装器安装到 `%LOCALAPPDATA%\Scout`，升级/卸载由应用自身管理（非 Scoop 便携目录）。
- `scoop uninstall scout` 会运行应用卸载器并清除索引/模型/日志等本地数据（`settings.json` 保留）。
- 安装包未签名（开源免费分发），SmartScreen/Defender 提示属正常，详见[安装指南](https://github.com/huibinma/Scout/blob/main/docs/install.md)。

## License

manifest 与本仓库内容按 MIT 提供（与主仓库一致）。
