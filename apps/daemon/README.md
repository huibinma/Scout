# scoutd — Scout 团队归档 MCP daemon

一句话定位：**把团队共享归档（设计稿 / 标书 / 财务底稿）的 hybrid 语义+FTS 检索能力下沉为 headless 服务，team 成员在自己机器上的 Claude Code / Codex / 任意 MCP 客户端通过 MCP streamable-HTTP 连上、不必每人本地灌一份索引。**

- ROADMAP 卡片：[`ROADMAP.md` BETA-32](../../ROADMAP.md)

---

## 1. 概览

### 场景

- **管理员一台机器**长跑 `scoutd`（macOS launchd / Linux systemd / Windows service），索引一份**单一固定目录**（例如 `/Volumes/Shared/departed-colleague-docs`）。
- 在职同事在自己机器上**任意 MCP 客户端**（Claude Code / Codex / Cline / 自研 Agent）通过 `Authorization: Bearer <token>` + streamable-HTTP 连上、跑 `tools/list` + `tools/call search` 拿命中。
- 团队不必每人本地灌一份大归档索引、节省磁盘 + 同步成本。

### 架构

复用 Scout 桌面 app 现成 hybrid 检索栈作 library——`packages/{intent-parser,harness,result-normalizer,ranker,indexer,model-runtime,search-backends/*}` 在 daemon 端零代码改动复用。新增两层：

- `packages/scout-server`（**lib crate**）：bearer auth + `ServerCtx`（共享 indexer / embedder / 索引状态）+ `Tool` trait + `SearchTool` + `ListRootsTool` + axum Router 工厂 + rmcp 1.8 streamable-HTTP transport 适配。
- `apps/daemon`（**binary `scoutd`**）：CLI / preflight 六检 / 首次全量索引 / lifecycle（graceful shutdown 信号）/ tracing 日志。

---

## 2. 安装

### 2.1 macOS — launchd

将下载的 binary 放 `/usr/local/bin/scoutd`，配置 `~/Library/LaunchAgents/com.scout.daemon.plist`：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTD/PropertyLists-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.scout.daemon</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/scoutd</string>
        <string>--config</string>
        <string>/etc/scoutd.toml</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>600</integer>
    <key>StandardOutPath</key>
    <string>/var/log/scoutd.out.log</string>
    <key>StandardErrorPath</key>
    <string>/var/log/scoutd.err.log</string>
</dict>
</plist>
```

加载：

```bash
launchctl load ~/Library/LaunchAgents/com.scout.daemon.plist
launchctl start com.scout.daemon
```

> **`ThrottleInterval = 600s`**：launchd 默认 10 秒重启窗口对大目录首次全量索引（可能数分钟）太短，会触发 restart loop；上调到 10 分钟。详见下方「故障排查 #3」。

### 2.2 Linux — systemd

`/etc/systemd/system/scoutd.service`：

```ini
[Unit]
Description=Scout team archive MCP daemon
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/scoutd --config /etc/scoutd.toml
Restart=always
RestartSec=10
TimeoutStartSec=900
User=scoutd
Group=scoutd
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```

启用：

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now scoutd
sudo journalctl -u scoutd -f
```

> **`TimeoutStartSec=900`**：systemd 默认 90 秒启动超时对大目录首次全量索引太短。详见下方「故障排查 #3」。

### 2.3 Intel Mac（x86_64）— 自行编译

Intel Mac binary 不在 CI release 范围（GitHub `macos-13` runner 池长期紧张、daemon-v0.1.0 cycle 中排队 2h+ 未启动阻塞 release）。Intel Mac 团队成员请本机编译：

```bash
rustup target add x86_64-apple-darwin
git clone https://github.com/huibinma/Scout.git
cd Scout
cargo build --release --locked --target x86_64-apple-darwin --bin scoutd
# 产物：target/x86_64-apple-darwin/release/scoutd
sudo cp target/x86_64-apple-darwin/release/scoutd /usr/local/bin/
```

之后按 §2.1 launchd 流程配置即可。Apple Silicon Mac 用户直接下 `scoutd-aarch64-apple-darwin` 即可，无需自编译。

### 2.4 Windows — NSSM service

下载 [NSSM](https://nssm.cc/) 后：

```cmd
nssm install Scoutd "C:\Program Files\Scout\scoutd.exe" ^
    --config "C:\ProgramData\Scout\scoutd.toml"
nssm set Scoutd AppStdout "C:\ProgramData\Scout\daemon.out.log"
nssm set Scoutd AppStderr "C:\ProgramData\Scout\daemon.err.log"
nssm set Scoutd Start SERVICE_AUTO_START
nssm start Scoutd
```

> Windows 服务管理控制台默认服务启动超时 30 秒；通过 `HKLM\SYSTEM\CurrentControlSet\Control\ServicesPipeTimeout` 调到 600000（10 分钟）或更高。详见下方「故障排查 #3」。

### 2.5 Windows — 本机编译带真实 embedder 的 binary（llama-cpp feature）

默认构建是 stub embedder（FTS-only，见故障排查 #4）。要在 Windows 本机编出带语义召回的 `scoutd`（开发调试 / 跑 `enterprise_scenarios` 评测），用仓库脚本：

```cmd
scripts\build-scoutd-llama.bat            :: dev profile
scripts\build-scoutd-llama.bat --release  :: 额外参数透传 cargo
```

前置条件（均可用环境变量覆盖，详见脚本头注释）：VS 2022 Build Tools（MSVC + 自带 cmake/ninja 组件）+ 一份解压的 LLVM Windows release（bindgen 需要 libclang，缺省找 `<repo>\.tmp\LLVM-*\bin`，或设 `LIBCLANG_PATH`）。脚本会把 `LOCALAPPDATA` 重定向到 `<repo>\.tmp` 以复用 llama-cpp-sys 的 llcb 构建缓存——热重编约 2 分钟（冷启 ~10 分钟）。

---

## 3. 配置

BETA-36 起两种启动形态**二选一**（互斥）：

- **legacy 单根**：`--root` + `--token`——自动合成 `default` 归档集合 + 全权 admin token，与 BETA-32 时代行为一致、零迁移；
- **collection 模式**：`--config <TOML>`——多归档集合（案件 / 离职员工 / 审计项目）+ 多 token per-collection 授权 + audit 配置。

### 3.1 CLI flag

| flag | 类型 | 默认 | 说明 |
|---|---|---|---|
| `--root` | path | — | 索引根目录（legacy 单根模式；与 `--config` 互斥） |
| `--bind` | socket addr | `0.0.0.0:8765` | HTTP 监听地址 |
| `--token` | string | — | Bearer token（legacy 单根模式，可走 env；与 `--config` 互斥） |
| `--data-dir` | path | （必填） | 索引 DB 目录（legacy：`index.db`；collection 模式另有 `collections/<id>/index.db`；audit 落 `audit.jsonl`） |
| `--model-path` | path | （必填，可走 env） | embedder GGUF 文件 |
| `--config` | path | — | collection 模式 TOML（见 §3.3；与 `--root`/`--token` 互斥） |
| `--semantic-weight` | float | 桌面 `DEFAULT_SEMANTIC_WEIGHT` | hybrid RRF 融合中语义臂权重（BETA-40 企业评测 A/B 用；一般不需要动） |
| `--disable-image-semantics` | bool | `false`（即默认**开**图片语义） | 关闭「OCR 图片文本入语义索引」。daemon 与桌面默认相反：企业场景图片证据（凭证/截图/照片）是检索刚需，且 2 字 CJK 词 FTS 结构性不可达、语义臂是图片内容唯一兜底；BETA-39 双层质量门槛沿用。关闭后启动期清除全部图片向量 |
| `--log-format` | `text` \| `json` | `text` | 日志输出格式 |
| `--log-level` | `trace`/`debug`/`info`/`warn`/`error` | `info` | 日志级别 |
| `--allow-rebuild-schema` | bool | `false` | 检测到 `schema_meta` 不一致或残留 rebuild 文件时允许重建 |

### 3.2 环境变量

| 变量 | 等价 flag | 说明 |
|---|---|---|
| `SCOUTD_TOKEN` | `--token` | Bearer token（推荐通过 launchd / systemd secrets 注入，避免出现在 `ps` / 日志） |
| `SCOUTD_MODEL_PATH` | `--model-path` | embedder GGUF 路径 |

### 3.3 collection 模式 TOML（BETA-36）

`/etc/scoutd.toml`（**含 token 明文——`chmod 600`、仅 daemon 运行账户可读**）：

```toml
[[collections]]
id = "case-2026-blueharbor"          # 唯一 slug（[a-z0-9-]、不以 - 开头）；同时是索引目录路径段
display_name = "蓝湾贸易合同纠纷案"
subject_kind = "case"                 # case | employee | audit_project | other
roots = ["/archive/cases/blueharbor"] # 支持多 root 归组
read_only = true                      # 只读态（冷冻归档）：指名 reindex 返 409
audit_tags = ["lawfirm", "litigation"]
# allow_full_read 缺省 false（BETA-43）：read_document 只能片段模式（命中片段 +
# 有限上下文，不吐全文）；显式 true 才放开全文读取。
allow_full_read = false

[[collections]]
id = "offboarding-lishili"
subject_kind = "employee"
roots = ["/archive/offboarding/lishili"]

[[tokens]]
token = "<≥32 字符>"
subject = "zhang.san"                 # audit 留痕主体（谁在检索）
collections = ["case-2026-blueharbor"] # 授权集合；["*"] = 全权
# admin 缺省 false：不能调 /admin/*

[[tokens]]
token = "<≥32 字符>"
subject = "ops"
collections = ["*"]
admin = true                          # 可调 /admin/reindex 与 /admin/audit

[audit]
log_query = true                      # false 时 audit 只记 query 长度（不记明文）
```

```bash
scoutd --config /etc/scoutd.toml --data-dir /var/lib/scoutd \
          --model-path /usr/local/share/scoutd/embeddinggemma-300M-qat-q8_0.gguf
```

**索引隔离**：每个 collection 一份独立 `index.db`（`data_dir/collections/<id>/`；legacy `default` 沿用 `data_dir/index.db`）——检索永远只打开授权集合的库文件，信息墙靠物理隔离而非查询层过滤。

### 3.4 audit 留痕（BETA-36 / BETA-43）

- 每次 `search` / `list_collections` / `read_document` / reindex / 越权拒绝，追加一条 JSONL 到 `<data_dir>/audit.jsonl`：`ts / subject / action / collections / query / results / path / read_mode / denied_reason`。
- **默认记 query 明文**（审计取证的核心诉求："谁在什么时候搜了什么"）；`[audit] log_query = false` 降级只记长度。
- **两套规则各守其职**：ops tracing log 永不记 query 内容（BETA-32 隐私硬规则不变）；`audit.jsonl` 是专用取证留痕，属于与被检索数据同级的敏感资产，须与 data_dir 同权限管控。
- 原始导出：`GET /admin/audit?tail=N`（admin token；缺省 100、上限 1000）。
- **合规报告导出（BETA-43）**：`GET /admin/audit/report?format=md|csv&subject=&collection=&from=&to=`（admin token）——jsonl 直接产出人读 Markdown（统计摘要 + 明细表）或 CSV，不必自行 parse。`from`/`to` 接受 RFC 3339 或 `YYYY-MM-DD`（`to` 的日期短格式含当日）。示例：`curl -H "Authorization: Bearer <admin token>" "http://host:8765/admin/audit/report?format=csv&subject=zhang.san&from=2026-07-01" > report.csv`。
- 越权行为（非 admin 调 `/admin/*` → **403**；MCP 请求未授权集合 / 禁全文集合请求全文 → tool error `access denied`）都会留 denied 记录。

---

## 4. Claude Code / Codex 接入

### 4.1 Claude Code（`mcpServers` JSON）

在客户端机器的 `~/.claude/settings.json`（Cline 等吃同款 JSON 的客户端配置位置同理）：

```json
{
  "mcpServers": {
    "scout-archive": {
      "type": "http",
      "url": "http://192.168.1.50:8765/mcp",
      "headers": {
        "Authorization": "Bearer <token>"
      }
    }
  }
}
```

### 4.2 Codex（`codex mcp add` 命令）

Codex **不吃上面的 `mcpServers` JSON**，只认 `codex mcp add` 命令（写进它自己的 TOML 配置）。令牌走环境变量、不明文落配置：

```bash
setx SCOUT_MCP_TOKEN "<token>"
codex mcp add scout-local --url http://192.168.1.50:8765/mcp --bearer-token-env-var SCOUT_MCP_TOKEN
```

> ⚠ **Codex 桌面版是 MSIX 应用**：`setx` 设置环境变量后，需**注销并重新登录 Windows**（或重启机器）才会生效——重启 Codex app 或 explorer 都不够。否则会连上但一直返回 `401`。

### 4.3 通用 / curl 探活

手动测 `/mcp` 时 rmcp 要求 `Accept` 头**同时**声明 `application/json` 和 `text/event-stream`，缺一即报错：

```bash
curl -X POST http://192.168.1.50:8765/mcp \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

客户端启动后会自动调 `tools/list` 发现三个工具：

- `search(query, limit?, collections?)` — hybrid 检索；缺省搜当前 token 授权的全部集合，`collections` 可限定；命中带 `collection` 字段与出处定位（`snippet` 命中片段；扫描件另带 `pages` 命中页号，BETA-43）；请求未授权集合返 tool error `access denied`。
- `read_document(path, collection, query?, full?)` — 读取命中文档内容（BETA-43，内容取自索引 db、不触磁盘原文件）：缺省片段模式必带 `query`、仅返回命中片段 + 有限上下文窗口 + 命中页摘录；`full=true` 需该集合 `allow_full_read=true`，否则 tool error `access denied` 并留 denied 记录。
- `list_collections()` — 返回当前 token 授权的归档集合（id / 显示名 / 归档主体 / 只读态 / allow_full_read / roots / doc_count / indexed_at）；未授权集合的存在性不泄漏。

---

## 5. 故障排查

### #1 `/health` 不响应 / 连接拒绝

- 检查端口是否真在监听：`lsof -iTCP:8765 -sTCP:LISTEN`（macOS / Linux）/ `netstat -ano | findstr 8765`（Windows）
- daemon 启动期间「首次全量索引」阶段 `/health` 尚未挂上，请等待索引完成（journal / launchd log 会输出 `首次全量索引完成`）。
- 防火墙：内网部署若用 `--bind 0.0.0.0:8765` 检查 host firewall（macOS Network Settings / Linux `ufw` / Windows Defender Firewall）。

### #2 `401 Unauthorized`

- 客户端 `Authorization` header 必须是 **`Bearer <token>`**（带 `Bearer ` 前缀 + 一个空格），不是裸 token。
- token 比较走常数时间（`subtle::ConstantTimeEq`），任何不匹配（含大小写 / 末尾空白）都返 401。
- 服务端日志会打印 401 但不打印任何 token 内容。

### #3 索引慢 / launchd / systemd 反复重启

- 首次全量索引耗时与归档大小成正比，**10 万文档级别可能数分钟到十几分钟**。
- launchd 默认 `ThrottleInterval = 10s`、systemd 默认 `TimeoutStartSec = 90s`，都比首次索引短，会进入 restart loop。
- 解决：launchd `ThrottleInterval` 调 600，systemd `TimeoutStartSec` 调 900（见 §2.1 / §2.2）。
- daemon 启动日志会打印 `首次全量索引开始；大目录可能耗时数分钟、期间 /health 不响应` 警告。

### #4 embedder stub fallback（语义召回降级）

- 默认 binary 构建**不含**真模型 backend，`embed()` 返 Err、daemon 退化为 **FTS-only** 模式。启动日志会打印 `embedder 不支持 embed()（默认 stub backend）；语义召回已禁用、daemon 退化为 FTS-only` 警告。
- 生产部署若要语义召回，须用 `--features semantic-recall`（macOS Metal）或同款 llama-cpp 系列 feature（Linux CPU/CUDA、Windows）编译 binary。
- FTS-only 模式仍可用：关键词 / 文件名 / 元数据匹配全部正常，只是「跨语言」「按意思命中」能力下降。

### #5 schema mismatch / 残留 rebuild 文件

- 升级 daemon 版本时若 schema 变化，启动会 fail-fast 报 `schema_meta` 不一致或检测到上次重建未清的 leftover 文件。
- 显式确认后加 `--allow-rebuild-schema` 重启、daemon 会清理残留 + 重建 schema。**重建会触发全量重索引、耗时与初次部署相同**。
- 在升级前先停 daemon、备份 `data_dir` 是稳健做法。

---

## 6. 限制 / 已知问题

- **默认 FTS-only**：本 cycle 范围内 daemon binary 默认 stub backend、不带模型。生产要语义召回须自带 GGUF 模型 + `semantic-recall` feature 编译，或等 follow-up cycle 默认开启。
- **首次全量索引阻塞 `/health`**：spec §5.1 字面允许、当前 cycle 已 warn 提示；follow-up cycle 计划改为 background spawn。
- **glibc 2.35 floor**（Linux x86_64）：CI 在 Ubuntu 24.04 上构建、依赖 glibc 2.35+。RHEL 7 / Ubuntu 20.04 等老发行版需自行从源码编译。
- **单租户、单 root**：本 cycle 范围 = 一台 daemon 索引一个根目录、所有访问者共用同一 token。多租户 / 多 root / per-user ACL 留 V 阶段（V10-16）。
- **reindex = 增量**（2026-07-03 真实化；2026-07-04 BETA-40 收尾扩为四段）：`POST /admin/reindex[?collection=<id>]` 跑真增量索引——music + document + **图片 OCR 轮**（JPG/PNG 经 Windows.Media.Ocr / Tesseract 入库；OCR 引擎每次 reindex 现场重探测，装好依赖无需重启）+ **语义向量 pass**（embedder 可用时写 `document_vectors`），完成后刷新该集合的 doc_count / indexed_at。schema 变更级的全量重建仍走重启 + `--allow-rebuild-schema`。
- **hybrid 检索**（2026-07-04 BETA-40 收尾）：embedder probe 通过时 per-collection 候选链 = FTS 臂 + 语义臂（`SemanticIndexBackend`，相似度下限 0.30），走桌面同款加权 RRF 融合；stub 构建自动回退 FTS-only。
- **提取失败留痕**（2026-07-04）：整份文件提取失败（不支持的 PDF 编码 / OCR 依赖缺失 / 畸形文件）落 `index_failures(path, reason, failed_time)` 表，成功重扫或磁盘删除后自动清除；取证复核可直接查该表。启动期会探测 OCR / pdftoppm 依赖并 warn 指明后果。

---

## 7. 相关文档

- [docs/playbooks/](../../docs/playbooks/README.md) — **三场景部署 playbook**（律所卷宗 / 审计取证 / 离职归档：collection 划分 + 权限矩阵 + 示例 query + LLM 工作流，BETA-40）
- [PROJECT.md](../../PROJECT.md) — 项目目标与原则
- [CONVENTIONS.md](../../CONVENTIONS.md) — 三工具协作 / 编码规范 / 收工流程
- [ROADMAP.md](../../ROADMAP.md) — 全量 task 状态（BETA-32 卡片）
- [docs/third-party-licenses.md](../../docs/third-party-licenses.md) — 第三方依赖登记
