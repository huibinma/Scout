# Scoop bucket 种子文件

`huibinma/scoop-scout` 独立 bucket 仓库的源文件（渠道评估）。

**建库步骤**（v0.9.14 Release 出资产后执行）：

1. 取资产 SHA256：`gh api repos/huibinma/Scout/releases/tags/v0.9.14 -q '.assets[0].digest'`（或下载后 `Get-FileHash`）。
2. 把 [scout.json](./scout.json) 中 `HASH_PLACEHOLDER` 换成该 sha256（不带 `sha256:` 前缀）。
3. `gh repo create huibinma/scoop-scout --public`，仓库结构：`bucket/scout.json` + `README.md`（用 [bucket-README.md](./bucket-README.md)）。
4. 验证：`scoop bucket add scout <repo-url> && scoop install scout`（真机装机可与 cycle 9 合并做）。

后续发版：manifest 带 `checkver`/`autoupdate`，可手动或用 scoop 的 excavator 模式更新版本与 hash；本目录种子与 bucket 仓库同步维护。
