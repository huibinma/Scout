# Third-Party Licenses

> 一旦引入任何第三方组件（crate / npm package / 模型 / 二进制），必须同步在此登记。
>
> 规则见 [CONVENTIONS.md §9](../CONVENTIONS.md) 与 [Scout知识产权保护计划书.md §7](./Scout知识产权保护计划书.md)。

## 登记格式

每个组件一行表格：

| 组件 | 版本 | 用途 | 来源 | License | 是否修改 | 是否随产品分发 | 备注 |
|---|---|---|---|---|---|---|---|
| serde | 1.0.228 | Rust 类型序列化 / 反序列化 | crates.io | MIT OR Apache-2.0 | 否 | 是 | SearchIntent / SearchResult JSON |
| serde_json | 1.0.150 | JSON fixture 与 schema 测试 | crates.io | MIT OR Apache-2.0 | 否 | 是 | 测试与运行时 JSON 处理 |
| chrono | 0.4.44 | 日期 / 时间类型与 metadata 时间戳 | crates.io | MIT OR Apache-2.0 | 否 | 是 | `default-features = false`；windows-search + intent-parser 额外启用 `clock`（相对时间 / 「X月份」运行期解析为绝对日期） |
| num-traits | 0.2.19 | chrono 间接依赖 | crates.io | MIT OR Apache-2.0 | 否 | 是 | 间接依赖 |
| iana-time-zone | 0.1.65 | chrono `clock` 的本地时区获取 | crates.io | MIT OR Apache-2.0 | 否 | 是 | 间接依赖（chrono clock 引入） |
| iana-time-zone-haiku | 0.1.2 | iana-time-zone 在 Haiku 平台实现 | crates.io | MIT OR Apache-2.0 | 否 | 是 | 间接依赖（不影响 macOS/Windows） |
| llama-cpp-4 | 0.3.2 | llama.cpp 绑定（Rust） | crates.io | MIT | 否 | 是 | MVP-14 模型推理；BETA-25 起静态链接进二进制；BETA-15B-9 0.3.0 → 0.3.2 升级（语义等价闸 cos ≥ 0.9999） |
| candle-core | 0.10.2 | 纯 Rust 张量库（模型推理） | crates.io | MIT OR Apache-2.0 | 否 | 是 | GGUF 支持 |
| candle-transformers | 0.10.2 | 模型架构实现（Llama） | crates.io | MIT OR Apache-2.0 | 否 | 是 | GGUF 支持 |
| candle-nn | 0.10.2 | 神经网络层实现 | crates.io | MIT OR Apache-2.0 | 否 | 是 | GGUF 支持 |
| thiserror | 2.0 | 强类型错误处理 | crates.io | MIT OR Apache-2.0 | 否 | 是 | 库错误定义 |
| anyhow | 1.0 | 动态错误处理 | crates.io | MIT OR Apache-2.0 | 否 | 是 | 应用级错误 |
| jsonschema | 0.33.0 | 运行时 JSON Schema 校验 | crates.io | MIT | 否 | 是 | MVP-02 SchemaValidator |
| tracing | 0.1 | 结构化日志 | crates.io | MIT | 否 | 是 | 诊断与追踪 |
| dirs | 5.0.1 | Windows Known Folder 解析（跨平台） | crates.io | MIT OR Apache-2.0 | 否 | 是 | MVP-13 WindowsLocationResolver；取代直接依赖 windows crate（其 `SHGetKnownFolderPath` 为 `unsafe`，与 workspace `unsafe_code = "forbid"` 冲突），unsafe 收敛进依赖内 |
| dirs-sys | 0.4.1 | dirs 平台后端 | crates.io | MIT OR Apache-2.0 | 否 | 是 | dirs 间接依赖 |
| tauri | 2.0 | 跨平台桌面框架（Rust 端） | crates.io | MIT OR Apache-2.0 | 否 | 是 | MVP-18 桌面应用框架 |
| tauri-plugin-global-shortcut | 2.0.0 | 全局快捷键支持 | crates.io | MIT OR Apache-2.0 | 否 | 是 | MVP-20 全局快捷键 |
| tauri-plugin-dialog | 2 | 系统文件/文件夹选择对话框 | crates.io | MIT OR Apache-2.0 | 否 | 是 | BETA-27 设置页索引目录选择器（2026-07-03 补登记，引入时漏记） |
| tauri-plugin-single-instance | 2 | 单实例锁（第二实例退出并聚焦既有窗口） | crates.io | MIT OR Apache-2.0 | 否 | 是 | BETA-33 cycle 9 防两实例并发写 index.db / settings.json |
| react | 18.3.1 | 前端 UI 库 | npm | MIT | 否 | 是 | MVP-18 桌面应用前端 |
| react-router-dom | 7.0.0 | 前端路由管理 | npm | MIT | 否 | 是 | MVP-22 设置/隐私页 |
| @phosphor-icons/react | 2.1.10 | Desktop 统一界面图标系统 | npm | MIT | 否 | 是 | 2026-07-24 Desktop UI 重构；替代跨平台样式不一致的 emoji 与传统菜单图标 |
| @tauri-apps/plugin-global-shortcut | 2.0.0 | 全局快捷键前端绑定 | npm | MIT OR Apache-2.0 | 否 | 是 | MVP-20 全局快捷键 |
| @tauri-apps/plugin-dialog | 2 | 文件/文件夹选择对话框前端绑定 | npm | MIT OR Apache-2.0 | 否 | 是 | BETA-27 设置页索引目录选择器（2026-07-03 补登记，引入时漏记） |
| vite | 5.3.1 | 前端构建工具 | npm | MIT | 否 | 是 | MVP-18 桌面应用构建 |
| futures-core | 0.3.32 | `BackendStream` / `ResultStream` 异步流 trait | crates.io | MIT OR Apache-2.0 | 否 | 是 | MVP-07A async/streaming |
| futures-channel | 0.3.32 | Harness 流事件 channel | crates.io | MIT OR Apache-2.0 | 否 | 是 | MVP-07A async/streaming |
| futures-util | 0.3.32 | Stream 扩展工具与测试消费 | crates.io | MIT OR Apache-2.0 | 否 | 是 | `default-features = false` |
| futures-executor | 0.3.32 | 测试用 `block_on`（替代各 crate 手写 `noop_waker` 轮询副本） | crates.io | MIT OR Apache-2.0 | 否 | 否 | 仅 `[dev-dependencies]`，不进生产分发 |
| tokio | 1.52.3 | CLI 当前线程 runtime；后续 backend async runtime 基础 | crates.io | MIT | 否 | 是 | 仅启用 `rt`，避免 lockfile 新增依赖；BETA-31 桌面 backend 加 `fs / io-util / rt-multi-thread` features 供模型 GUI 下载 stream 用 |
| reqwest | 0.12 | HTTP 客户端（stream + rustls-tls 后端） | crates.io | MIT OR Apache-2.0 | 否 | 是 | BETA-31 模型 GUI 一键下载；`default-features = false` + `rustls-tls` 关 default-tls、避免 openssl 平台依赖；与 BETA-26 spike-retrieval / training 侧 HTTP 解耦 |
| sha2 | 0.10 | 计算 fixture 源文件 sha256（BETA-08 数据集元信息锚定） | crates.io | MIT OR Apache-2.0 | 否 | 否 | 仅 `build_lora_dataset` binary 训练数据生成时使用，不进生产分发 |
| lofty | 0.24.0 | 音频标签提取（artist/title/album/duration/format/bitrate） | crates.io | MIT OR Apache-2.0 | 否 | 是 | BETA-01 音乐 metadata 索引 |
| lofty_attr | 0.12.0 | lofty 派生宏（proc-macro） | crates.io | MIT OR Apache-2.0 | 否 | 是 | lofty 间接依赖 |
| ogg_pager | 0.7.2 | OGG 容器分页解析 | crates.io | MIT OR Apache-2.0 | 否 | 是 | lofty 间接依赖 |
| rusqlite | 0.32.1 | SQLite 绑定（Rust），`bundled` 内嵌 SQLite/FTS5 | crates.io | MIT | 否 | 是 | BETA-01 索引存储；pin 0.32（0.40/libsqlite3-sys 0.38 用未稳定 `cfg_select`，stable 编不过） |
| libsqlite3-sys | 0.30.1 | rusqlite 的 SQLite C 绑定（`bundled` 编译 SQLite 源码，启用 FTS5） | crates.io | MIT | 否 | 是 | 内嵌的 SQLite 引擎为 Public Domain |
| hashlink | 0.9.1 | rusqlite 的 LRU 缓存 | crates.io | MIT OR Apache-2.0 | 否 | 是 | rusqlite 间接依赖 |
| fallible-iterator | 0.3.0 | rusqlite 可错迭代器 | crates.io | MIT OR Apache-2.0 | 否 | 是 | rusqlite 间接依赖 |
| fallible-streaming-iterator | 0.1.9 | rusqlite 流式可错迭代器 | crates.io | MIT OR Apache-2.0 | 否 | 是 | rusqlite 间接依赖 |
| walkdir | 2.5.0 | 递归目录遍历（索引扫描） | crates.io | Unlicense OR MIT | 否 | 是 | BETA-01 音乐目录扫描 |
| data-encoding | 2.11.0 | lofty 的 base64/hex 编码 | crates.io | MIT | 否 | 是 | lofty 间接依赖 |
| tempfile | 3.27.0 | 临时目录/文件（RAII 自动清理） | crates.io | MIT OR Apache-2.0 | 否 | 是 | BETA-35 起为 indexer 生产依赖（PDF 页渲染临时 PNG；BETA-37 附件临时文件）；此前仅 dev-dependency |
| calamine | 0.35.0 | 电子表格读取（xlsx/xls/ods，含旧版二进制 xls） | crates.io | MIT | 否 | 是 | BETA-02 文档内容索引 |
| pdf-extract | 0.10.0 | PDF 文本抽取 | crates.io | MIT | 否 | 是 | BETA-02 文档内容索引 |
| lopdf | 0.38.0 | PDF 解析（pdf-extract 依赖） | crates.io | MIT | 否 | 是 | 间接依赖 |
| quick-xml | 0.40.1 | OOXML/HTML XML 解析（docx/pptx/html 正文提取） | crates.io | MIT | 否 | 是 | BETA-02；calamine 另引 0.39.x |
| zip | 2.4.2 | OOXML 容器（docx/pptx）ZIP 读取 | crates.io | MIT | 否 | 是 | BETA-02；`default-features=false, features=["deflate"]` |
| pulldown-cmark | 0.13.4 | Markdown 解析（剥语法取纯文本） | crates.io | MIT | 否 | 是 | BETA-02 文档内容索引 |
| encoding_rs | 0.8.35 | 字符编码（calamine/quick-xml/mail-parser 依赖） | crates.io | (Apache-2.0 OR MIT) AND BSD-3-Clause | 否 | 是 | 间接依赖；BSD-3-Clause 仅覆盖部分编码数据表；mail-parser 经 `full_encoding` feature 用它解 GBK/GB18030 等历史 charset |
| mail-parser | 0.11.4 | eml MIME 解析（headers / RFC 2047 encoded-word / multipart / 传输编码 / charset） | crates.io | Apache-2.0 OR MIT | 否 | 是 | BETA-37 邮件格式提取；纯 Rust |
| hashify | 0.2.9 | 编译期完美哈希（mail-parser 的 proc-macro） | crates.io | Apache-2.0 OR MIT | 否 | 否 | mail-parser 编译期依赖，不进分发产物 |
| codepage | 0.1.2 | Windows 代码页映射（calamine 依赖） | crates.io | Apache-2.0 OR MIT | 否 | 是 | 间接依赖 |
| flate2 | 1.1.9 | DEFLATE 解压（zip 依赖） | crates.io | MIT OR Apache-2.0 | 否 | 是 | 间接依赖 |
| rayon | 1.12.0 | 并行音频标签提取（BETA-01A 全盘索引砍提取耗时） | crates.io | MIT OR Apache-2.0 | 否 | 是 | index_paths 并行 lofty 提取 |
| rayon-core | 1.13.0 | rayon 线程池核心 | crates.io | MIT OR Apache-2.0 | 否 | 是 | rayon 间接依赖 |
| crossbeam-deque | 0.8.6 | rayon work-stealing 队列 | crates.io | MIT OR Apache-2.0 | 否 | 是 | rayon 间接依赖 |
| crossbeam-epoch | 0.9.18 | crossbeam 内存回收 | crates.io | MIT OR Apache-2.0 | 否 | 是 | rayon 间接依赖 |
| crossbeam-utils | 0.8.21 | crossbeam 并发原语 | crates.io | MIT OR Apache-2.0 | 否 | 是 | rayon 间接依赖 |
| either | 1.16.0 | rayon 的 Either 迭代器 | crates.io | MIT OR Apache-2.0 | 否 | 是 | rayon 间接依赖 |
| rmcp | 1.8.0 | MCP server SDK（Rust 官方 SDK） | crates.io | Apache-2.0 | 否 | 是 | BETA-32 MCP daemon；features `server` + `transport-streamable-http-server` |
| axum | 0.8.9 | HTTP server 框架 | crates.io | MIT | 否 | 是 | BETA-32 MCP transport + admin REST |
| tower | 0.5.3 | 中间件 Service trait | crates.io | MIT | 否 | 是 | BETA-32 axum 中间件栈 |
| tower-http | 0.7.0 | HTTP 中间件（trace + limit） | crates.io | MIT | 否 | 是 | BETA-32 admin REST 限流/追踪 |
| secrecy | 0.10.3 | Bearer token 凭据封装 | crates.io | MIT OR Apache-2.0 | 否 | 是 | BETA-32 T5 auth；防 token 误日志 |
| subtle | 2.6.1 | 常量时间比较 | crates.io | BSD-3-Clause | 否 | 是 | BETA-32 T5 鉴权 timing-safe equals |
| getrandom | 0.2.17 | OS CSPRNG（随机 bearer token） | crates.io | MIT OR Apache-2.0 | 否 | 是 | BETA-53 桌面内嵌本机 MCP 服务 token 生成；本就为 rustls/reqwest 传递依赖，BETA-53 起桌面直依赖 |
| parking_lot | 0.12 | 快速锁原语 | crates.io | MIT OR Apache-2.0 | 否 | 是 | BETA-32 server 共享状态锁 |
| async-trait | 0.1 | trait async fn 桥接 | crates.io | MIT OR Apache-2.0 | 否 | 是 | BETA-32 server 内部 trait（注：rmcp 1.8 `ServerHandler` 用 RPIT，不走 async-trait） |
| httptest | 0.16.4 | HTTP mock server（dev-only） | crates.io | MIT OR Apache-2.0 | 否 | 否 | BETA-32 集成测试，仅 `[dev-dependencies]`，不进生产分发 |
| clap | 4.6.1 | CLI 解析（derive + env） | crates.io | MIT OR Apache-2.0 | 否 | 是 | BETA-32 T9 daemon CLI |
| toml | 0.8.2 | TOML 配置文件解析 | crates.io | MIT OR Apache-2.0 | 否 | 是 | BETA-32 T10 登记；BETA-36 起 scout-server 直接依赖（daemon `--config` collections/tokens/audit 配置） |
| tracing-subscriber | 0.3.23 | tracing 日志后端（env-filter + json） | crates.io | MIT | 否 | 是 | BETA-32 T9 daemon 日志 |
| tracing-appender | 0.2 | tracing 文件 sink（daily 滚动 + non-blocking writer） | crates.io | MIT | 否 | 是 | BETA-31-v3 cycle 2 桌面 app scout.log 持久化 |

## 预期组件清单（计划中，尚未引入）

| 组件 | 用途 | 预期 License |
|---|---|---|
| Qwen2.5-1.5B-Instruct | 基座模型 | Apache 2.0（以模型卡为准） |
| objc2 crate | macOS API（Rust） | MIT |
| Everything `es.exe` CLI（非 SDK） | Windows 可选加速——**运行期外部进程**，用户自装、不随产品分发 | voidtools License（MIT 风格宽松许可，**2026-07-04 已核查**，见下方说明） |
| Tesseract | 跨平台 OCR 兜底 | Apache-2.0 |
| Apple Vision framework | macOS OCR | macOS 系统 API |
| Windows.Media.Ocr | Windows OCR | Windows 系统 API |

引入时确认实际版本与 license，并迁移到上方正式表格。

> **BETA-03 OCR 说明**：图片 OCR 以**运行期外部进程**调用，**无新 cargo 依赖**——
> Windows 经 `powershell` 调 **Windows.Media.Ocr**（Windows 系统自带，无需分发）；跨平台兜底
> shell-out **Tesseract**（Apache-2.0，用户**可选**自行安装，未装则图片索引优雅跳过）。
> 二者均不进 `Cargo.lock`，故仍列于本「预期/外部」区。

> **Everything 再分发条款核查（2026-07-04，BETA-00 开源发布审查项）**：
> `packages/search-backends/everything` 仅在运行期 spawn 用户自装的 `es.exe`（onboarding 引导用户到
> voidtools 官网自行下载），**未使用 Everything SDK、仓库与安装包均不含任何 voidtools 二进制**
> （git 全库无 exe/dll/lib 入库）——即不构成再分发，voidtools 条款对本项目分发物不产生约束。
> 另查 [voidtools License.txt](https://www.voidtools.com/License.txt) 本身为 MIT 风格宽松许可
> （免费使用含商业、允许再分发、仅要求保留版权声明），即便未来改为捆绑分发亦无阻碍。
> **结论：核查通过，开源分发无风险；命名上继续以描述性方式提及 Everything，不暗示 voidtools 背书。**
