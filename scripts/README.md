# scripts

跨平台的开发/构建/打包脚本。

## 已有脚本

| 脚本 | 平台 | 用途 |
|---|---|---|
| `build-scoutd-llama.bat` | Windows (cmd) | 编译带 llama-cpp feature（真实 embedder）的 `scoutd`：自动装配 VS Build Tools vcvars + 自带 cmake/ninja + `LIBCLANG_PATH`（缺省 `<repo>\.tmp\LLVM-*\bin`）+ llcb 缓存重定向（`LOCALAPPDATA→<repo>\.tmp`，热重编 ~2min）。前置条件与可覆盖环境变量见脚本头注释。额外参数透传 cargo（如 `--release`）。评测（`enterprise_scenarios`）与本机 daemon 语义验证前跑它。 |
| `ci.sh` | POSIX | CI 辅助 |
| `gen-enterprise-file-fixtures.ps1` | Windows (PowerShell) | 生成企业场景文件 fixture |
| `generate_enterprise_real_format_materials.py` | 跨平台 (Python) | 生成企业三场景真实格式材料（DOCX/PPTX/XLSX/PDF/JPG/PNG/扫描 PDF/EML） |
| `hooks/` | — | git hooks |

## 计划内容

- 打包：macOS DMG / Windows MSIX
- 签名：macOS codesign + notarytool / Windows signtool
- 评测跑批：批量跑 evals 并生成报告
- 模型下载与量化

约定脚本既能在 macOS 上跑（zsh / bash），也能在 Windows 上跑（PowerShell / cmd）。如平台特有，文件名加后缀或用平台原生扩展名，并在本文件登记用途。含中文的 `.ps1` 必须带 UTF-8 BOM（PS 5.1 无 BOM 按 GBK 读）；`.bat` 注释用 ASCII 避免代码页问题。
