# Windows 开发 / 测试环境准备

> 目的：在一台干净的 Windows 机器上从 GitHub 同步 Scout 代码后，最快路径跑起来——开发、测试、跑评测、（按需）跑桌面 app 与本地模型。
> 适用：Windows 10 / 11，x86-64。配合 Claude Code 使用。
> 维护：随工具链/前置变化更新；与 [PROJECT.md](../PROJECT.md) 技术决策、各 package README 对齐。

---

## 0. 一句话路径

- **只想跑 BETA-15A 召回评测 / parser / evals / harness / 后端单测** → 装 **Rust stable** 一项即可，`cargo test -p <crate>` 直接跑（最快，下面 §3）。
- **想跑桌面 app（Tauri）** → 额外装 **Node 18+ / MSVC C++ Build Tools / WebView2**（§4）。
- **想跑本地模型 fallback（GGUF 推理）** → 额外装 **CMake** + 手动拷贝模型文件（GGUF 被 gitignore，clone 不含，§5）。

按需要分层装，不必一次到位。

---

## 1. 同步代码

### 1.0 认证（私有仓库，clone / push 均需登录）

`huibinma/Scout` 是私有仓库，clone / pull / push 前需先用有权限的账号完成 GitHub 认证：

```powershell
winget install --id GitHub.cli
gh auth login        # 选 GitHub.com → HTTPS → Login with a web browser
```

### 1.1 clone / pull

```powershell
git clone https://github.com/huibinma/Scout.git
cd Scout
# 或已 clone 过：
git pull origin main
```

确认拿到最新：

```powershell
git log --oneline -1
# 与 GitHub 上 main 最新 commit 一致即可
```

**行尾**：仓库用 `.gitattributes` 统一 LF。Windows 上 git 默认 `core.autocrlf=true` 可能改行尾——若 `git status` 显示大量"伪改动"，设 `git config core.autocrlf false` 后重新 checkout。

---

## 2. 会话开始（用 Claude Code 时）

Scout 是 Claude Code / Codex / Gemini 三工具轮换协作的项目。**新会话开始必读四份共享文档**（[CLAUDE.md](../CLAUDE.md) 入口已写明）：

1. [PROJECT.md](../PROJECT.md) — 目标 / 架构
2. [STATUS.md](../STATUS.md) — 当前进度 / 当前 task / 下一步 / 会话日志（**单一信源**）
3. [ROADMAP.md](../ROADMAP.md) — 全程任务地图 / 出场标准
4. [CONVENTIONS.md](../CONVENTIONS.md) — 协作规则 / **收工流程** / 编码规范

收工时按 CONVENTIONS §3 更新 STATUS / ROADMAP，一次中文 commit，署名 `Claude Code`。

---

## 3. 最小环境：Rust（覆盖 BETA-15A / parser / evals / 后端单测）

### 装 Rust

1. 装 [rustup](https://rustup.rs/)（会随之拉起 MSVC 链接器需求，见下）。
2. 仓库 `rust-toolchain.toml` pin 了 `channel = stable` + `rustfmt` + `clippy`，rustup 会自动按它装对应组件，无需手动指定版本（workspace `rust-version = 1.80`，stable 满足）。
3. rustup 在 Windows 默认用 **MSVC toolchain**，需要 **Visual Studio C++ Build Tools**（见 §4.1，链接器 `link.exe` 必需）。若暂时不想装 VS，可改用 GNU toolchain：`rustup default stable-x86_64-pc-windows-gnu`（但桌面 Tauri 仍建议 MSVC）。

### 跑 BETA-15A 同义词召回评测

```powershell
# 报告（按桶/按语言分桶 + 门槛退出码）
cargo run -p scout-evals --bin synonym_recall
# 仅看未达标 case
cargo run -p scout-evals --bin synonym_recall -- --only-failures
# JSON 报告
cargo run -p scout-evals --bin synonym_recall -- --json
```

期望：总召回 88.2% / 假阳 0.0%（与 macOS 一致——纯离线 Rust，无平台差异），门槛通过退出码 0。

### 跑各 crate 单测 / 评测（不碰桌面与模型）

```powershell
cargo test -p scout-evals          # 含 recall 单测 + 集成门槛测试
cargo test -p scout-intent-parser  # parser
cargo test -p scout-harness        # harness（含同义词 expander）
cargo test -p scout-native-index    # 内置原生索引（MFT 枚举 + USN Journal，替代原 Everything 集成）
cargo test -p scout-search-backend # common
cargo run  -p scout-evals --bin evals -- --fixtures v0.5   # parser-only 评测（期望 472/26/2）
```

> **为什么用 `-p` 而不是 `--workspace`**：workspace 含 `apps/desktop/src-tauri`（需 Tauri 前置）与 `packages/model-runtime`（`llama-cpp` 特性需 CMake）。`cargo test --workspace` 会尝试编译它们——若还没装 §4/§5 的前置就会失败。**只做后端/评测/parser 开发时用 `-p` 精确指定 crate**，跑得最快、前置最少。

### ci.sh（bash 脚本）

`scripts/ci.sh`（fmt + clippy + build + test + synonym_recall）是 **bash 脚本**，Windows 原生 cmd/PowerShell 跑不了。两种方式：

- **Git Bash**（装 Git for Windows 自带）：`bash scripts/ci.sh`
- **直接敲命令**（推荐做局部开发时）：
  ```powershell
  cargo fmt --all -- --check
  cargo clippy -p <crate> --all-targets -- -D warnings
  cargo test -p <crate>
  ```
  注意 `scripts/ci.sh` 跑的是 `--workspace`，整套需要 §4/§5 前置齐全；局部开发用 `-p` 即可。

---

## 4. 桌面 app（Tauri 2）前置

仅当要跑/构建 `apps/desktop` 时需要。

### 4.1 MSVC C++ Build Tools（Rust 链接 + Tauri 都需要）

装 [Visual Studio 2022 Build Tools](https://visualstudio.microsoft.com/downloads/)，勾选 **「使用 C++ 的桌面开发」**（含 MSVC v143 + Windows 11 SDK）。这是 Rust MSVC toolchain 链接器与 Tauri 编译的硬前置。

### 4.2 WebView2 Runtime

Windows 11 通常已内置；Windows 10 若缺，装 [WebView2 Evergreen Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)。Tauri 的 WebView 依赖它。

### 4.3 Node.js

装 **Node 18+**（前端 vite + Tauri CLI）。仓库 `apps/desktop/package.json` 用 `@tauri-apps/cli ^2`、`vite ^5`、`react 18`。

```powershell
cd apps\desktop
npm install
# 开发模式（热重载）
npm run tauri dev
# 构建安装包
npm run tauri build
```

> 桌面 app 的「后端状态指示」「模型 fallback」等功能在无 §5 模型时会降级显示，不影响搜索主路径（系统搜索后端）。

---

## 5. 本地模型（GGUF 推理 fallback）—— 文件被 gitignore，需手动获取

### 现状

`.gitignore` 排除了 `*.gguf` / `*.safetensors`。**clean clone 不含任何模型文件**。纯 parser / 后端 / 评测开发**不需要**模型；只有跑「模型 fallback」（规则解析不足时调小模型补字段）才需要。

### 获取 GGUF（生产同款 = Qwen3-0.6B）

桌面 app 的一键下载（设置 → 常规）会自动拉取生产使用的同一个模型：`unsloth/Qwen3-0.6B-GGUF` 的 `Qwen3-0.6B-Q4_K_M.gguf`（~400 MB），存为 `<scout_data_dir>/models/qwen3-0.6b-q4_k_m.gguf`（详见 [apps/desktop/src-tauri/src/model_download.rs](../apps/desktop/src-tauri/src/model_download.rs)）。

不想走 GUI 的话，直接用浏览器 / `curl` 从 HuggingFace 拉同一个文件，放到 evals / `fallback_probe` 默认查找路径 `models/qwen3-0.6b-q4_k_m.gguf`，或自定义后用 `SCOUT_MODEL_PATH` 指向：

```
https://huggingface.co/unsloth/Qwen3-0.6B-GGUF/resolve/main/Qwen3-0.6B-Q4_K_M.gguf
```

### 编译模型 fallback 特性（Windows 真机实测前置 — BETA-09(a) 2026-06-01 解锁）

> `packages/model-runtime` 的 `llama-cpp` feature 在 Windows 上编译，除 CMake 外还有几个 macOS 不暴露的隐藏前置（macOS 的 clang/Metal 工具链自带）。以下为 BETA-09(a) 真机一次性趟通的完整清单，缺一不可：

| 前置 | 装法 | 为什么需要 |
|---|---|---|
| **MSVC C++ Build Tools** | §4.1 | 链接器 + C++ 编译（vcvars64）|
| **CMake** | `winget install Kitware.CMake` | llama.cpp 构建 |
| **LLVM / libclang** | `winget install LLVM.LLVM`，设 `LIBCLANG_PATH=C:\Program Files\LLVM\bin` | `llama-cpp-sys-4` 经 `bindgen` 生成 FFI 绑定，需 `libclang.dll`（不装报 `Unable to find libclang`）|
| **Ninja 生成器**（VS 自带）| 设 `CMAKE_GENERATOR=Ninja` | 默认 VS 生成器下 `cmake` crate 把 `-j8` 传给 MSBuild 会报 `MSB1001 未知开关`；改用 Ninja 即可。**务必在 vcvars64 开发者环境里编**（cl.exe 在 PATH）|
| **Vulkan SDK**（GPU 加速，可选但强烈推荐）| `winget install KhronosGroup.VulkanSDK`，设 `VULKAN_SDK=C:\VulkanSDK\<ver>` | 纯 CPU 推理在弱机器上慢到不实用（单次 fallback 几十秒）。Vulkan 走核显/独显快很多。Windows 用 `vulkan` 特性（非 macOS 的 `metal`）|

**编译命令**（在 VS 开发者环境的 cmd 里，避免引号问题建议写 bat）：

```bat
call "...\VC\Auxiliary\Build\vcvars64.bat"
set "CMAKE_GENERATOR=Ninja"
set "LIBCLANG_PATH=C:\Program Files\LLVM\bin"
set "VULKAN_SDK=C:\VulkanSDK\1.4.350.0"
set "PATH=C:\Program Files\CMake\bin;...VS...\CMake\Ninja;%VULKAN_SDK%\Bin;%PATH%"
cargo clean -p llama-cpp-sys-4 --release   :: 切 CPU<->Vulkan 时必做，避免 CMakeCache 生成器冲突
cargo build -p scout-evals --features model-fallback-vulkan --bin evals --release
```

> 切 GPU 后端时改 feature：`model-fallback`（纯 CPU）/ `model-fallback-vulkan`（Vulkan）/ `model-fallback-metal`（仅 macOS）。**注意 `cargo run` 时 feature 必须与编译一致，否则会按新 feature 重编。** 无法/不想装 CMake 时，model-runtime 有纯 Rust 的 `candle` 后端 fallback（见 [packages/model-runtime/README.md](../packages/model-runtime/README.md)）。

跑带模型的评测（CLI，在上述 bat 环境内）：

```bat
set "SCOUT_MODEL_PATH=...\models\qwen3-0.6b-q4_k_m.gguf"
cargo run -p scout-evals --features model-fallback-vulkan --bin evals --release -- --fixtures v0.5 --with-fallback --hybrid
```

> **BETA-09(a) 实测结论**：Intel Iris Xe Vulkan 跑完整 500 case 与 macOS/Metal **逐项 0pp 差异**（准确性跨平台完全一致）。但**延迟**：弱核显 p95 fallback ~22s（macOS Metal 1.6s），不达 3000ms 交互门槛——弱硬件上模型 fallback「准确但太慢」，产品侧应能力感知降级（默认纯 parser，检测到强 GPU 再启用模型）。

---

## 6. Windows 特定开发重点（这台机器才能推进的事）

STATUS / ROADMAP 里有几项一直**卡 Windows 真机**，正是这台机器解锁的价值：

1. **两个 Windows 后端执行层 ✅ 已在 Windows 11 真机实测（2026-05-31，MVP-11/12）**：
   - `packages/search-backends/windows-search/src/lib.rs`：`PlatformWindowsSearchExecutor` 经 `Search.CollatorDSO` OLE DB provider（固定 `PowerShell` + ADODB 脚本，SQL 经环境变量传入）执行；用 `System.ItemUrl` 还原真实路径（非本地化 `ItemPathDisplay`）；相对时间在执行器解析为绝对 ISO（provider 不支持 `DATEADD`/`GETDATE`）。真机集成测试 `tests/real_windows_search.rs`（`cargo test -p scout-search-backend-windows-search -- --ignored`）。
   - ~~`packages/search-backends/everything/src/lib.rs`：`EsCliExecutor` spawn `es.exe`~~——**2026-08-20 重构移除**：不再集成外部 Everything（需用户自装 `es.exe`），改用内置 `packages/search-backends/native-index`（`scout-native-index`）直接调用 Win32 API 读取 NTFS MFT / USN Journal，无需安装任何第三方软件、只需以管理员权限运行 Scout。真机集成测试 `packages/search-backends/native-index/tests/real_volume.rs`（`cargo test -p scout-native-index --test real_volume -- --ignored`）。
2. **MVP-26 跨平台一致性测试**：在 Windows 跑 v0.5 evals，与 macOS 对比，验证「双平台通过率差 < 5pp」（M→B 切换硬指标，至今从未实跑过）。
3. **BETA-09(a) 跨平台部署**：Windows 加载 GGUF（§5）验证推理路径与 macOS 一致（已闭合，详见 ROADMAP BETA-09(a) 归档）。
4. **MVP-24 Windows 索引位置引导**：当前 macOS stub，Windows 真检测待真机。

→ 下个会话开场，先看 STATUS「下一步」，从上述里选一条推。

### 6.1 图片 OCR（BETA-03）运行期前置

图片 OCR **无 cargo 依赖**，以外部进程运行，按平台需要可选前置：

- **Windows.Media.Ocr（首选，系统自带）**：需已装对应 **OCR 识别语言包**。检查：
  ```powershell
  [Windows.Media.Ocr.OcrEngine,Windows.Media.Ocr,ContentType=WindowsRuntime] | Out-Null
  [Windows.Media.Ocr.OcrEngine]::AvailableRecognizerLanguages | % DisplayName
  ```
  无中文识别器 → 设置 → 时间和语言 → 语言 → 添加「中文（简体）」语言包（含 OCR）。
- **Tesseract（跨平台兜底，可选）**：`winget install tesseract-ocr.tesseract` + 装 `chi_sim`/`eng`
  语言数据（PATH 上有 `tesseract` 即被 `default_ocr_engine` 选为兜底）。
- 两者皆无 → 图片索引**优雅跳过、不报错**（音乐/文档索引照常）。
- 真机集成测试：`cargo test -p scout-indexer --test real_ocr -- --ignored`（需已装 OCR 语言）。

---

## 7. 常见坑速查

| 现象 | 原因 / 解法 |
|---|---|
| `link.exe not found` / 链接失败 | 没装 MSVC C++ Build Tools（§4.1），或没重启终端让 PATH 生效 |
| `cargo build --workspace` 在 model-runtime 报 cmake 错 | 没装 CMake（§5），或只想跑后端/评测——改用 `cargo test -p <crate>` |
| `cargo test --workspace` 在 desktop 报 Tauri/WebView2 错 | 桌面前置未装（§4）——开发后端时用 `-p` 跳过 desktop |
| `bash: scripts/ci.sh` 找不到 | 用 Git Bash 跑，或直接敲 cargo 命令（§3） |
| git status 一堆伪改动（行尾） | `git config core.autocrlf false` 再重新 checkout（§1） |
| 模型 fallback 不生效 / 找不到模型 | GGUF 被 gitignore，需手动拷 + 设 `SCOUT_MODEL_PATH`（§5）；sha256 务必核对 |
| 内置原生索引后端 `BackendUnavailable` | 需以管理员权限运行 Scout（打开 NTFS 卷句柄的 Win32 硬性要求，见 §6.1） |

---

## 8. 验证清单（环境装好后自检）

```powershell
# 最小（Rust）
cargo test -p scout-evals                       # 召回单测 + 门槛集成测试全过
cargo run  -p scout-evals --bin synonym_recall  # 88.2% / 0.0% 门槛通过
cargo run  -p scout-evals --bin evals -- --fixtures v0.5   # 472/26/2

# 桌面（装了 §4）
cd apps\desktop && npm install && npm run tauri dev

# 模型（装了 §5）
$env:SCOUT_MODEL_PATH="C:\path\to\beta17-qwen3-0.6b-q4_k_m.gguf"
cargo run -p scout-evals --features model-fallback --bin evals -- --fixtures v0.5 --with-fallback --hybrid
```

跑通最小那三条，就说明代码同步 + Rust 环境 OK，可以开始 Windows 侧开发了。
