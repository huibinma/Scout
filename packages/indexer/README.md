# scout-indexer

自建本地索引器。规划职责：音乐 metadata、Office/PDF 内容、OCR、向量检索、后台调度
（详 [计划书 §10](../../docs/local-personal-search-agent-project-plan.md)）。

**当前状态**：

| 子能力 | ROADMAP | 状态 |
|---|---|---|
| 音乐 metadata 索引 | BETA-01 | ✅ 已实现（`MusicIndex`） |
| Office/PDF 内容索引（FTS5） | BETA-02 | ✅ 已实现（`DocumentIndex`） |
| 图片 OCR 文本索引 | BETA-03 | ✅ 已实现（`OcrEngine` + `index_image_dirs`） |
| 向量检索 | BETA-11B / 1.0 | 未开始 |
| 后台低优先级调度 | BETA-07 | ✅ 已实现（后台启动索引 + prune） |

---

## BETA-01 音乐 metadata 索引

把音乐目录里的音频标签（artist / title / album / duration / format / bitrate）抽取进跨平台
SQLite + FTS5，对外提供查询 API。

> 范围：**只做索引层 + 查询 API**，不接 Agent / SearchBackend。与系统搜索结果融合留
> [BETA-04 Result Normalizer](../../ROADMAP.md) + BETA-05 Ranker；后台调度留 BETA-07。
> 设计见 spec。

### 公共 API

```rust
use scout_indexer::{MusicIndex, MusicQuery, default_music_roots};

// 打开（或创建）索引库，建表。
let idx = MusicIndex::open(&db_path)?;

// 增量索引：系统音乐目录 + 任意额外目录。跳过 mtime 未变的文件，
// 回收 roots 子树下磁盘已删的记录。
let mut roots = default_music_roots();          // dirs::audio_dir()
roots.push(extra_dir);
let stats = idx.index_dirs(&roots)?;            // IndexStats { scanned, added, updated, skipped, removed, failed }

// 查询：FTS5 文本（artist/title/album）+ 结构化过滤（AND 组合）。
let hits = idx.query(&MusicQuery {
    text: Some("周华健".into()),
    format: Some("FLAC".into()),
    limit: Some(20),
    ..Default::default()
})?;
```

`MusicIndex::open_in_memory()` 用于测试。

### SQLite schema

| 表 | 说明 |
|---|---|
| `music` | 主表：path（UNIQUE）/ file_name / artist / title / album / duration_secs / format / bitrate / modified_time / indexed_time |
| `music_fts` | FTS5 全文索引（artist / title / album，`trigram` tokenizer），`rowid` 与 `music.id` 手动对齐 |

FTS 用**独立** FTS5 表（非 `content=` external-content），删除直接 `DELETE FROM music_fts
WHERE rowid=?` 即可，避免 external-content 表的特殊 `'delete'` 命令；代价是多存一份
artist/title/album 文本（音乐 metadata 量级可忽略）。

### 增量语义

`index_dirs` 对每个 root 递归遍历（walkdir，不跟随 symlink），仅对**音乐扩展名白名单**
（`mp3 flac m4a aac ogg opus wav wma aiff aif ape`，大小写不敏感）的文件：

1. 取文件 mtime（unix 秒）；
2. 与索引中该 path 的 `modified_time` 比对：相等 → `skipped`；否则用 lofty 重读标签 upsert
   （新增 `added` / 已存在 `updated`）；标签读取失败计 `failed`，不中断整轮；
3. 遍历完后回收：索引中 path 落在任一 root 子树下、但本轮未见到的记录（= 磁盘已删）→
   `removed`。root 之外的记录不受影响。

`music.id` 跨更新保持稳定（UPDATE 而非 REPLACE），以维持 FTS rowid 对齐。

### 查询语义

- `text`：经 FTS5 `MATCH`。任意用户输入统一转义成单个合法短语前缀查询
  （包双引号 + 内部 `"` 转义为 `""` + 末尾 `*`），杜绝语法错误 / 注入。
- `artist` / `album`：主表 `LIKE '%…%'` 子串（ASCII 大小写不敏感）。
- `format`：精确 `= … COLLATE NOCASE`。
- 各字段 AND 组合；全空表示「全部」。`limit` 缺省 50，排序 `artist, title`。

### known limitation

- **FTS tokenizer = `trigram`**（BETA-04 起，原 `unicode61`）：支持任意 **≥3 字符**子串匹配
  （CJK + 英文，默认大小写不敏感），让「正文含某中文词」可命中。代价：**<3 字符查询无法命中**
  （trigram 固有，2 字符名 / 缩写走结构化 `artist` LIKE 过滤兜底）；模糊 / 语义召回属向量检索
  （BETA-11B）范畴。**BETA-42**：查询侧多词组合走 AND 拼接时，<3 字纯 CJK 词项会被上层
  （`scout-local-index-backend` 的 `fts_match_from_groups`）从 AND 条件里剔除，避免其
  结构性不可匹配拖垮整个组合查询——但该短词本身仍不参与匹配约束，此 known limitation 未变。
  **BETA-56（短查询 metadata LIKE 兜底）**：`DocumentIndex::query`/`MusicIndex::query` 对
  「无 `fts_match` 且 query 全词 <3 字符纯 alnum/CJK」的纯短查询改走 `LIKE '%词%'` 匹配
  **metadata 列**（documents: title/author/file_name；music: artist/title/album/file_name；
  **不扫 body**），让 2 字人名（「燎原」）/ 常用词 / 短编号命中元数据；判据 `db::short_metadata_like_terms`。
  长短混合查询仍走 FTS（正文内容仍受 <3 字限制，由语义臂兜底）。
- **`bundled` 编译需 C 编译器**：rusqlite `bundled` 从源码编译 SQLite，需 macOS clang /
  Windows MSVC（项目已因 llama.cpp 具备该前置，无新增系统依赖）。首次编译较慢。
- **单线程顺序索引**：未做并发；音乐库通常千级文件量级，足够。并发优化按需。
- **未接 Agent**：查询接口为 BETA-04 预留，当前不参与自然语言搜索链路。

### 测试

- 单元测试（in-memory，确定性，不依赖真实音频）：storage / query / 增量 / 删除 / FTS 转义，
  通过 stub 提取器隔离 lofty；lofty 提取走 WAV 往返（测试内纯 Rust 生成最小合法 WAV）。
- `tests/real_music.rs`（`#[ignore]`）：真机音乐目录端到端 smoke。
  `cargo test -p scout-indexer --test real_music -- --ignored --nocapture`。

### 全盘音频发现（BETA-01A）

超越固定 Music 目录、索引电脑内任意位置的音频（spike 实测：用户音频散落 OneDrive，
默认 Music 目录扫 0 条）。**发现 / 提取 / 存储三层拆分**：

```rust
use scout_indexer::{default_audio_discovery, MusicIndex};

// 发现层（可选加速）：枚举全盘音频路径（仅路径，不读内容）。
if let Some(disc) = default_audio_discovery() {
    if let Ok(paths) = disc.discover_audio() {
        idx.index_paths(&paths)?;          // 并行提取 + 占位符跳过
    }
}
```

- **`AudioDiscovery`**：Windows `NativeIndexAudioDiscovery`（内置 MFT 枚举 + USN Journal，
  `scout-native-index`）/ macOS `SpotlightDiscovery`（`mdfind public.audio`）。工具/权限不可用
  返 `DiscoveryError::Unavailable` → 调用方回退目录扫描（守「不强制依赖外部全盘索引工具」）。
- **`MusicIndex::index_paths(&[PathBuf])`**：顺序预检 → **rayon 并行** lofty 提取 → 顺序 upsert
  （rusqlite `Connection: !Sync`）。
- **占位符跳过**：Windows 查 `FILE_ATTRIBUTE_OFFLINE`/`RECALL_ON_DATA_ACCESS`（只读属性、无 unsafe）
  ——OneDrive "仅在线"文件不读标签、只存文件名（避失败 + 避触发下载，仍按名可搜）。
- **file_name 进 FTS**：标签覆盖稀疏（实测 ~21%），`music_fts` 含 file_name → 按文件名子串可搜；
  旧 3 列库打开自动迁移（从 music 主表重填）。

#### BETA-01A known limitation

- macOS iCloud dataless 占位符检测无安全 std API → best-effort 不跳过（留后续）。
- 全盘发现按 `ext:` 枚举，可能纳入系统/缓存音频（后续可加排除目录）。

**回收（BETA-07）**：`MusicIndex::prune_deleted()` 删除磁盘上已不存在的记录（用 `Path::exists()`
判定，占位符路径存在不误删）；`reindex` 发现分支调用它回收已删音乐（`index_dirs` 文档/回退分支
经 `run_incremental_index` 已自带回收）。**后台自动索引**：desktop 启动时后台跑一次 reindex（非阻塞 UI）
+ 并发守卫，详 BETA-07 spec。

---

## BETA-02 文档内容索引

把 Office / PDF / 纯文本文档的**正文 + 标题 + 作者**抽取进跨平台 SQLite + FTS5，提供全文查询
（含 `snippet()` 命中片段）。每文档粒度，mtime 增量，复用 BETA-01 的
[`IncrementalStore`] 扫描骨架。范围同 BETA-01：只做索引层 + 查询 API，不接 Agent（融合留
BETA-04）。设计见
spec。

### 格式覆盖

| 格式 | 库 | 提取 |
|---|---|---|
| docx | zip + quick-xml | `word/document.xml` 的 `<w:t>` + `docProps/core.xml` 的 title/author |
| pptx | zip + quick-xml | `ppt/slides/slideN.xml` 的 `<a:t>`；page_count = 幻灯片数 |
| xlsx / xls / ods | calamine | 全 sheet 全 cell 文本；page_count = 工作表数（含旧版二进制 xls） |
| pdf | pdf-extract | 全文文本（page_count 暂 None）；pdf-extract panic/Err（如中文 CID CMap `UniGB-UCS2-H` 不支持）→ 降级 rasterize + OCR 管线（BETA-40 收尾） |
| html / htm | quick-xml | Text 节点（跳过 `script`/`style`），`<title>` 作标题 |
| md / markdown | pulldown-cmark | 剥语法取纯文本 |
| txt | std | UTF-8 直读（非法字节 lossy） |
| eml | mail-parser | BETA-37：subject→title、from→author；From/To/Date/Subject 头块 + 正文进 body；附件解码→临时文件→递交本表提取器（深度限 1）、文本以「[附件 文件名]」段并入 body |

### 公共 API

```rust
use scout_indexer::{DocumentIndex, DocumentQuery, default_document_roots};

let idx = DocumentIndex::open(&db_path)?;
idx.index_dirs(&default_document_roots())?;          // 增量
let hits = idx.query(&DocumentQuery {
    text: Some("季度预算".into()),                   // FTS：title/author/body
    doc_type: Some("pdf".into()),
    ..Default::default()
})?;
// hits[i].entry 元信息；hits[i].snippet = FTS5 命中片段（仅文本查询）
```

`documents` 主表 + 独立 `documents_fts` FTS5 表（title/author/body/entity）；正文只进 FTS。
查询排序 `modified_time DESC`（近期优先）。老库（3 列）打开自动迁移加 `entity` 列、逐行保留
body（透明加列、无 schema bump）。

**文件级提取失败留痕**（BETA-40 收尾，2026-07-04）：整份文件提取失败（不支持的 PDF
编码 / OCR 依赖缺失 / 畸形文件）落 `index_failures(path, reason, failed_time)` 表——
不再只累计 `IndexStats.failed` 静默丢；成功重扫或磁盘删除后自动清除。查询 API：
`extraction_failures()` / `extraction_failure_count()`。与 `document_failed_pages`
（扫描 PDF **页级**失败）互补。音乐库暂不留痕（`IncrementalStore` 默认 no-op）。

**PII 类型概念词召回**（BETA-59 重构：独立 `entity` 列）：索引时对正文做轻量 PII 类型识别，
仅在命中中国大陆身份证号（18 位且 GB 11643 校验位正确）或手机号（`1[3-9]\d{9}`）时，把
“身份证 / 手机号”等**类型关键词**写进 `documents_fts.entity`（末列，非 body），用于
「查找包含身份证信息的文件」这类概念查询；不会把识别到的号码复制到任何字段。查询用裸
`documents_fts MATCH` 自动跨所有列，概念词照样命中 entity；而 `snippet()` 固定取 body 列
（index 2）、永不回显 entity 关键词——彻底隔离"可搜的类型标签"与"展示的出处片段"。存量索引
entity 列迁移后为空、待下次内容变更增量重抽时回填（老正文 body 已保、搜索不受影响）。

### known limitation

- **旧版二进制 doc / ppt（pre-2007）不支持**：纯 Rust 难，defer（旧版 xls 经 calamine 覆盖）。
- **Office 锁文件跳过**：扫描时按 basename 前缀 `~$` 跳过 Word/Excel/PowerPoint 打开文档期间
  生成的隐藏锁文件（如 `~$报告.docx`），不进提取管线——否则会当作真 docx/xlsx/pptx 解析、
  必然报 `invalid Zip archive: Could not find EOCD` 误计 failed（`scan.rs::is_office_lock_file`）。
- **加密 docx / pptx**：打开密码保护的文档是 OLE2/CFB 容器而非 zip，`zip::ZipArchive::new`
  必然失败；按 CFB 文件签名识别后给出「文档疑似已加密或为旧版二进制格式」的可读 detail，
  而非原始 zip 报错文本（仍计 failed，只是原因可读）；不做解密，正文读不到属预期。
- **每文档粒度**：存页/幻灯片总数，不返回精确命中页码（每页粒度留后续）。
- **pdf page_count 暂 None**：pdf-extract 不直接给页数；后续可加 lopdf 计页。
- **扫描件 PDF 依赖装机**：图片型 PDF 走 rasterize（pdftoppm）+ OCR 管线（BETA-35）；
  依赖缺失时整份计 failed 并留痕 `index_failures`。
- **body cap 1 MiB 字符**：超大文档正文截断。
- **OOXML 只抽文本叶子**（`<w:t>`/`<a:t>`），不解析样式/结构，兼容不同生产者。
- **邮件只支持 eml**（BETA-37）：msg（Outlook OLE）后置 BETA-37b、pst 明确不做；附件展开深度 1、
  单附件 32MB / 单邮件 32 个附件上限，超限只留文件名标记行；附件不单独成 documents 行。

### 文档身份与副本去重（BETA-38）

- **doc identity = 文件原始全字节 FNV-1a 指纹**（`documents.content_hash`，`embed::file_identity_hash`
  流式 8KB 缓冲）。与 `document_vectors.source_hash`（截断后**正文**指纹、驱动"正文没变跳过重嵌"）
  不同层：`content_hash` 是**文件身份**，同内容多副本（判决书存多盘 / 迁移盘 / 压缩包展开）得同值。
- **索引期去重**：`embed_pending` 遇同 `content_hash` 已有当前模型向量 → 复制向量而非重新 embed
  （相同字节→相同正文→相同向量，精确复制），返回 `(embedded, reused, failed)` 计数区分。
- **审计留痕不失真**：每 path 仍各自成 `documents` 行 + 向量（复用值），副本关系
  `SELECT path FROM documents WHERE content_hash=?` 可还原（取证可查一份材料的全部副本位置）。
- **迁移**：老库无 `content_hash` 列 → 打开自动 `ALTER TABLE ADD COLUMN`（列可空、老行 NULL、
  下次内容变更增量索引回填）；`content_hash` 索引在列就绪后建（不放 SCHEMA，避老库建索引撞缺列）。
- **known limitation**：FNV-1a 非密码学 hash，理论碰撞极低但非零（辅以文件 size 语义降误判，
  本卡未做全字节二次比对）；读取失败（权限 / 占位符 / 文件消失）降级 `None`、不阻断索引。
- **规模化基准（cycle 4）**：语义臂进程级缓存去掉每查询全量重载后，十万级 p95 从 ~900ms
  降到 ~170ms（缓存 vs 暴力重载 5×+，报告）。
  `DocumentIndex::seed_synthetic_vectors` 是**评测/基准专用**批量种入（合成十万语料、不走文件
  提取）——生产索引一律走 `index_dirs` + `embed_pending`，不调此方法。

### 测试

- 文本提取 helper（`collect_xml_text` / `first_element_text`）直接喂 XML 字节断言；
- docx / pptx 经测试内 `zip::ZipWriter` 构造最小样本端到端；md / html / txt 经临时文件；
- xlsx / pdf 经 `tests/real_documents.rs`（`#[ignore]`）真机覆盖。
  `cargo test -p scout-indexer --test real_documents -- --ignored --nocapture`。

---

## BETA-03 图片 OCR 内容索引

对图片做 OCR、把识别出的文字存进 BETA-02 的 `documents` FTS，让「找含某词的截图/图片」可命中。
图片当一种 doc_type（png/jpg…），OCR 文字当 body。设计见
spec。

### OCR 引擎策略（原生优先 + Tesseract 兜底）

在 workspace `unsafe_code = forbid` 约束下，原生 OCR API（WinRT / Vision = FFI）不能直接调，
沿用项目 **shell-out 拿结构化输出** 套路：

| 引擎 | 平台 | 机制 |
|---|---|---|
| `WindowsOcrEngine` | Windows | `powershell` 调内嵌 `.ps1` 经 **Windows.Media.Ocr WinRT**（`-EncodedCommand` 传脚本，图片路径走环境变量 `SCOUT_OCR_IMAGE` → 杜绝注入）。零安装、零 unsafe、中文佳 |
| `TesseractOcrEngine` | 跨平台 | shell-out `tesseract <img> stdout -l chi_sim+eng`（需用户装 + 语言数据） |
| macOS Vision | macOS | 留后续（`OcrEngine` trait 已抽象） |

`default_ocr_engine()` 优先级：Windows.Media.Ocr 可用 → 否则 PATH 有 tesseract → 都无返 `None`
（图片索引优雅跳过、不报错）。

> **`-EncodedCommand` 而非 `-File`**：PowerShell `-File`/stdin 会把整段脚本一次性编译，导致
> `[System.WindowsRuntimeSystemExtensions]` 等类型字面量在 `Add-Type` 之前解析而「找不到类型」；
> 脚本改为**顶层语句逐条执行** + `trap` 错误处理，经 base64(UTF-16LE) `-EncodedCommand` 传入。

### CJK 空格折叠

Windows.Media.Ocr 在 CJK 字符间插空格（`会 议 纪 要`），不折叠会破坏 trigram FTS 对「会议」的匹配。
`normalize_ocr_text` 折叠**相邻 CJK 表意字符间**的空白、保留拉丁词间空格（两引擎共用）。

### 公共 API

```rust
use scout_indexer::{DocumentIndex, default_image_roots, default_ocr_engine};

let idx = DocumentIndex::open(&db_path)?;
if let Some(ocr) = default_ocr_engine() {
    idx.index_image_dirs(&default_image_roots(), &*ocr)?;   // dirs::picture_dir()，含截图
}
// 查询经 DocumentQuery.doc_types 框定图片类型（见 local-index 的 MediaSearch(Image) 路由）。
```

图片轮复用 `run_incremental_index`（mtime 增量 skip + panic 兜底 + 回收）。**回收按本轮扩展名收窄**：
图片与文档可能同根目录，文档轮不回收图片、图片轮不回收文档。

### known limitation

- **逐文件 OCR**（v1）：每图 spawn 一次 PowerShell（~0.5-1s）。首跑慢，但 mtime 增量重跑秒级 +
  BETA-07 后台非阻塞兜底。批量 / 并行 OCR 留后续优化。
- **只取纯文本**：不存分辨率 / 文字坐标框 / 置信度。
- **OCR 语言固定**：Windows 用 `TryCreateFromUserProfileLanguages`、Tesseract 固定 `chi_sim+eng`；
  可选语言留后续。需用户已装对应 OCR 语言包（Windows 设置 → 语言）/ tesseract 语言数据。
- **超大图**（宽或高 > WinRT `OcrEngine.MaxImageDimension`）→ `win_ocr.ps1` 用 `BitmapTransform`
  等比缩到上限内再识别（`GetSoftwareBitmapAsync` 的缩放解码重载），不再整图计 failed。
- **扫描件 PDF** 仍无正文（本引擎面向图片文件；扫描 PDF 留后续，可复用本引擎）。

### 测试

- 单元测试用 **stub `OcrEngine`** 隔离真 OCR：`index_image_dirs` 端到端（扫描计数 / FTS 命中 /
  mtime skip / 删图回收 / OCR 失败计 failed）、回收扩展名收窄、`normalize_ocr_text` / base64 /
  `pick_engine` 优先级。
- `tests/real_ocr.rs`（`#[ignore]`，仅 Windows 真机 + 装 OCR 语言）：合成 PNG fixture
  `tests/fixtures/ocr_cjk.png`（白底黑字，无真实用户数据）端到端断言识别含「会议纪要测试」。
  `cargo test -p scout-indexer --test real_ocr -- --ignored`。
