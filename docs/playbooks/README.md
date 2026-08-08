# Scout 企业冷归档检索 playbook（BETA-40）

> 三份场景手册的共用底座：组件、部署拓扑、配置语法入口、MCP 客户端接入、audit、验证清单。
> 场景差异（collection 划分 / 权限矩阵 / 示例 query / LLM 工作流）见各场景篇：
>
> 1. [律所案件卷宗检索](./lawfirm-case-archive.md)
> 2. [企业内部审计取证检索](./internal-audit-forensics.md)
> 3. [离职员工材料归档检索](./offboarding-archive.md)
>
> 定位边界（[PROJECT.md](../../PROJECT.md)）：Scout 是**数据不出门的检索底座**——按意思找、跨语言模糊召回、留痕。
> 摘要 / 比对 / 起草等分析能力**不内置**，由内网 LLM 客户端（Claude Code / Codex / Cline 等）经 MCP 组合完成。

## 1. 共同画像

三场景共享四个特征，这也是 OS 原生语义搜索（锁新硬件、管不到归档服务器）覆盖不到的缝隙：

1. **敏感数据不出门**——卷宗 / 凭证 / 离职材料不允许上传任何云端；
2. **冷归档**——材料基本不再变化，坐在文件服务器上；
3. **检索者不熟悉语料组织方式**——接手律师 / 审计员 / 继任者不知道原作者怎么起文件名、放哪个目录；
4. **需留痕**——谁在什么时候搜了什么，要能回答。

## 2. 组件与前置

| 组件 | 说明 |
|---|---|
| `scoutd` | 归档检索 daemon（[apps/daemon/README](../../apps/daemon/README.md)：安装 / launchd / systemd / NSSM 服务化全套） |
| embedding 模型（GGUF） | 语义召回用；FTS-only 降级模式可先跑通链路（见 daemon README 故障排查 #4） |
| poppler（pdftoppm）+ OCR 引擎 | **扫描件场景必装**（判决书 / 发票 / 保密协议扫描版走"页渲染 → OCR"管线）：Windows `winget install oschwartz10612.Poppler` + Windows.Media.Ocr 自带；macOS/Linux `brew/apt install poppler`（+ tesseract） |
| 内网 LLM 客户端 | Claude Code / Codex / Cline 等任一支持 MCP streamable-HTTP 的客户端 |

> **图片语义默认开**（2026-07-04 起）：daemon 会把 JPG/PNG 的 OCR 文字纳入语义索引（凭证 /
> 截图 / 现场照片按意思可查，`鲲鹏` 这类 2 字中文词也能命中图片内容），OCR 乱码由双层质量
> 门槛拦截；如需关闭加 `--disable-image-semantics`（重启后自动清除已嵌图片向量）。

## 3. 部署拓扑（三场景同构）

```text
┌────────────────────────────── 内网 ──────────────────────────────┐
│                                                                   │
│  归档文件服务器                          检索者工作机               │
│  ┌─────────────────────────┐            ┌─────────────────────┐  │
│  │ /archive/...（只读挂载） │            │ LLM 客户端           │  │
│  │ scoutd :8765          │◄──MCP────►│ (Claude Code 等)     │  │
│  │  ├ collections/<id>/db   │  bearer    │  search /            │  │
│  │  ├ audit.jsonl           │  token     │  list_collections    │  │
│  │  └ 模型 GGUF             │            └─────────────────────┘  │
│  └─────────────────────────┘                                      │
└───────────────────────────────────────────────────────────────────┘
```

- daemon 跑在**归档数据所在的机器**上（数据不动，索引就地建）；
- 每个归档主体（案件 / 审计项目 / 离职员工）一个 **collection**，独立 index.db 物理隔离；
- 客户端只拿到自己 token 授权范围内的集合——`list_collections` 都看不到别人的。

## 4. 配置

collection / token / audit 的 TOML 完整语法与校验规则见 [daemon README §3.3](../../apps/daemon/README.md)；各场景篇给出**该场景的划分建议与权限矩阵示例**。要点：

- token 明文在 TOML 里 → `chmod 600`、仅 daemon 运行账户可读；
- 每人一枚 token、`subject` 填实名/工号（audit 留痕主体）；**不要共用 token**，否则留痕退化为"有人查过"；
- 归档定稿后把 collection 置 `read_only = true`（冷冻：误触 reindex 直接 409）；
- 材料有新增时 `POST /admin/reindex?collection=<id>`（admin token）增量刷新，无需重启。

## 5. MCP 客户端接入（Claude Code 示例）

检索者工作机 `~/.claude/settings.json`：

```json
{
  "mcpServers": {
    "scout-archive": {
      "type": "http",
      "url": "http://<归档服务器>:8765/mcp",
      "headers": { "Authorization": "Bearer <本人 token>" }
    }
  }
}
```

客户端自动发现三个工具：

- `search(query, limit?, collections?)`——自然语言检索（中文 / 英文 / 跨语言别名），命中返回 `path / name / collection / size / mtime / score` + 出处定位（`snippet` 命中片段；扫描件另带 `pages` 命中页号，BETA-43）；
- `read_document(path, collection, query?, full?)`——读取命中文档内容（BETA-43，取自索引、不触磁盘原文件）：缺省片段模式只返回 `query` 命中片段 + 有限上下文 + 命中页摘录；`full=true` 仅在该集合 `allow_full_read = true` 时可用，否则拒绝并留痕；
- `list_collections()`——本人可检索的集合与索引新鲜度。

**读原文的两条路**：① 集合策略允许时用 `read_document`（推荐——读取行为进 audit、且检索者工作机**不需要**挂载归档路径）；② 客户端自己的文件读取能力（如 Claude Code 的 Read）打开 `path`——需要 SMB/NFS 只读挂载或在归档服务器本机跑客户端，且**绕过 daemon 留痕**，信息墙要求严的部署应封掉挂载、只走 ①。

## 6. audit 留痕（三场景共用）

- 每次 search / list_collections / read_document / reindex / 越权拒绝 → `<data_dir>/audit.jsonl` 一行（`ts / subject / action / collections / query / results / path / read_mode / denied_reason`）；
- 默认记 **query 明文**（"谁在什么时候搜了什么"）；隐私要求更严的部署配 `[audit] log_query = false` 降级记长度；
- 原始导出：`GET /admin/audit?tail=N`（admin token）；文件本身与归档数据同级管控；
- **合规报告导出（BETA-43）**：`GET /admin/audit/report?format=md|csv&subject=&collection=&from=&to=`（admin token）——直接产出人读 Markdown / CSV 报告（统计摘要 + 明细），合规人员不必 parse jsonl；`from`/`to` 接受 RFC 3339 或 `YYYY-MM-DD`；
- ops 日志（tracing）**永不**记 query 内容——两套规则各守其职。

## 7. 真机走通验证清单（验收留证据用）

在目标内网环境按序执行并留存输出（截图 / 终端记录）：

1. `scoutd --config …` 启动，日志出现「collection 首次全量索引完成」×N；
2. `curl http://<host>:8765/health` → `{"status":"ok"}`；
3. 受限 token 调 `list_collections` → 只见授权集合；
4. 场景篇的 10 条示例 query 逐条经 LLM 客户端检索，记录命中率（预期 ≥8/10 top-5 命中）；
5. 受限 token 显式请求未授权集合 → `access denied`；
6. admin token `GET /admin/audit?tail=20` → 上述操作全部留痕、subject 正确；
7. 往某读写集合的 root 加一份新文件 → `POST /admin/reindex?collection=<id>` → 新文件可命中；
8. （BETA-43）禁全文集合上 `read_document(full=true)` → `access denied` 且 audit 有 denied 记录；片段模式返回命中片段不吐全文；
9. （BETA-43）`GET /admin/audit/report?format=md&subject=<检索者>` → 人读报告含上述操作明细。

> 验证证据回填：完成后在 ROADMAP BETA-40 对应 task 卡下记录验收证据（ROADMAP BETA-40 验收第二条）。
