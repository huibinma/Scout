# 索引性能优化设计

> **状态**：v1.6（§9 执行顺序调整 + 音频改名已落地；§10 落地 U1 + U3 核心部分——`IndexStatus` 新增 `phase_total`/`phase_scanned`/`phase_rate_per_min`/`last_run_stage_ms`，前端真百分比/ETA/耗时明细；U2/U4/U5/P3 仍是待办）。
> **背景**：用户真机反馈——首次全量索引约 1 万个文件耗时 1 小时以上。本文档分析索引构建架构瓶颈、评估复用 Windows Search / Spotlight / Everything 已有系统索引的可行性，并给出分优先级的优化任务规划。
> 对应 ROADMAP：[BETA-64](../ROADMAP.md)。上一轮相关优化见 BETA-60（并发化 + WAL，2026-07-09）。

## 1. 索引构建架构现状

### 1.1 整体流程：四阶段严格串行，互不重叠

首次全量索引入口在 [apps/daemon/src/main.rs](../apps/daemon/src/main.rs) `run_initial_collection_index`，在同一个 `tokio::task::spawn_blocking` 闭包里顺序执行：

```
① music_index.index_dirs_with_progress()          // 全量 WalkDir + 音频标签提取
② document_index.index_dirs_with_progress()        // 全量 WalkDir + 文档提取（含PDF/邮件）
③ document_index.index_image_dirs_...()            // 全量 WalkDir + 图片 OCR
④ document_index.embed_pending()                   // 语义向量嵌入
```

四步严格顺序执行——①②③各自都是**先扫完全部文件、提取完全部内容再进下一阶段**，④要等①②③全部落库才开始。任一阶段的性能问题都会完整累加到总耗时上，不会被并行掩盖。桌面端 `apps/desktop/src-tauri/src/search/index_status.rs` 的 `perform_reindex_for_roots` 走的是同一套底层 `packages/indexer` 增量骨架，架构结论同样适用。

### 1.2 瓶颈清单

**B1｜三次独立全盘目录遍历，文档/图片没有系统索引加速**

音乐索引已有 [packages/indexer/src/discovery.rs](../packages/indexer/src/discovery.rs)——用 Everything/Spotlight 全盘秒级枚举路径，不走 `WalkDir`。但文档和图片索引仍各自完整跑一遍 `WalkDir`（[scan.rs:344](../packages/indexer/src/scan.rs)），同一批目录被遍历三次。`discovery.rs` 文件头注释与 [ROADMAP.md BETA-01A 背景注记](../ROADMAP.md) 都写着"思路可推广至 BETA-02 文档全盘索引"——这个方向团队自己已经想到，但尚未落地。

**B2｜提取并发硬编码为 4，不区分轻重任务**

`const EXTRACT_PARALLELISM: usize = 4`（[scan.rs:30](../packages/indexer/src/scan.rs)），取 `min(4, 可用核数)`，且对音乐标签解析（纯内存、无子进程）、文档提取（多数轻量、少数扫描版 PDF 触发 OCR 子进程）、图片 OCR（恒触发子进程）三种负载一视同仁。该常量是 BETA-60 v0.9.27 热修的产物——注释记录了不限并行时 17 个 `pdftoppm` 子进程把机器打爆、首索引卡「0/50」十分钟的真机故障，是刻意用吞吐量换稳定性的权衡，但代价是无论 4 核还是 32 核机器，纯内存解析的 txt/md/docx 也被摁在 4 并发。

**B3｜单份 PDF 内多页 OCR 是纯串行**

[doc_extract.rs:409-413](../packages/indexer/src/doc_extract.rs)：

```rust
let page_ocr_results = rendered.pages().iter()
    .map(|(page_no, png)| (*page_no, ocr.recognize(png)))   // .iter().map()，非 par_iter()
    .collect();
```

一份 50 页的扫描版 PDF，即便机器有 16 核，也只能单核顺序跑完 50 次 OCR 子进程调用。企业档案/合同这类大扫描件场景会被这一点严重拖慢。

**B4｜OCR/PDF 光栅化是重量级子进程模型，且每文档重新探测可用性**

[ocr.rs](../packages/indexer/src/ocr.rs) Windows 走 `powershell.exe` 调 WinRT `Windows.Media.Ocr`，每张图片都新起一个 PowerShell 进程（几百 ms 级冷启动）；[pdf_rasterizer.rs](../packages/indexer/src/pdf_rasterizer.rs) shell-out `pdftoppm`。`default_pdf_rasterizer()`/`default_ocr_engine()` 是每份文档调用一次的探测，不是全局缓存一次。

**B5｜Embedding：batch size = 1，且每次调用都新建 llama.cpp context，与提取/OCR 阶段完全不重叠**

`embed_pending`（[doc_db.rs:904](../packages/indexer/src/doc_db.rs)）是 `for (i, ...) in pending.iter().enumerate()` 纯顺序循环；`model-runtime` 的 `run_embed` 每次调用都新建一个 context、用完即弃，没有批量 decode。GPU 全 offload（`gpu_layers: 99`）配置正确，但 batch=1 + 每次重建 context 意味着 GPU 利用率被人为拉低，且这个阶段要等①②③全部落库才开始，完全不与提取/OCR 阶段重叠。

**B6｜DB 写入按文件逐条事务提交，FTS5 trigram 索引边写边建**

[doc_db.rs](../packages/indexer/src/doc_db.rs) 里 `upsert_document_with_pages` 和 `MusicIndex::upsert_entry`（[db.rs:205](../packages/indexer/src/db.rs)）每篇文档单独 `unchecked_transaction()` + `commit()`，`upsert_vector` 甚至直接 autocommit——一万文件就是一万次独立事务提交。虽然 WAL + `synchronous=NORMAL` 已避免最坏情况，但比"按 chunk 攒批提交"仍多出可观的固定开销。FTS5 用 trigram 分词器，对 CJK 文本产生的 token 数远高于词级分词，这个成本摊在了写入关键路径上。

**B7｜性能参数全部硬编码，无运行时配置入口**

`EXTRACT_PARALLELISM`/`EXTRACT_CHUNK`/OCR 超时/PDF DPI/embedding batch size 均为源码 `const`，没有 env 或配置文件覆盖入口——用户或运维想根据自己机器调优，必须改代码重新编译。

**B8｜没有阶段级耗时埋点**

[progress.rs](../packages/indexer/src/progress.rs) 目前只统计"扫描数/入库数"，没有记录 discovery/extract/OCR/embed 各阶段的 wall time。以上判断目前基于代码走读 + 历史真机事故（v0.9.27）推断，还没有真实 profile 数据验证各阶段占比——这是本轮 P0 的第一步。

（次要项：PII 检测跑在串行写库路径而非并行提取路径，量级小；邮件递归提取最多 32 个附件，若附件里有多个扫描版 PDF 会在同一提取槽位内堆叠多次 OCR pipeline。）

## 2. 能否复用 Windows Search / Spotlight / Everything 的已构建索引

Scout 目前对这三者的使用只在**查询期**：[packages/search-backends/{spotlight,windows-search,everything}](../packages/search-backends) 把 `SearchIntent` 翻译成 `mdfind` 谓词 / `SystemIndex` SQL / `es.exe` 参数，作为搜索时的一条 fallback 通道，跟索引构建无关。音乐索引的 BETA-01A 验证了一种**索引构建期**的复用方式：

### 2.1 可行且已验证：用作"文件发现层"（目前只用在音乐上）

[ROADMAP.md BETA-01A](../ROADMAP.md) 记录了真机实测：Windows 上 `es.exe ext:...` **307ms 枚举全盘 1249 个音频文件**，比 Scout 自己的 `WalkDir` 递归遍历快几个数量级（尤其叠加 OneDrive 占位符检测这类会触发额外系统调用的场景）。[discovery.rs](../packages/indexer/src/discovery.rs) 已把这条通道实现为 `AudioDiscovery` trait，工具不可用时优雅回退到 `WalkDir`。这条路径可以直接扩展到文档/图片发现（`es.exe ext:pdf;docx;...` / `mdfind kMDItemContentTypeTree`），把三次全盘遍历替换成三次亚秒级系统索引查询——本次分析里投入产出比最高的一项。

### 2.2 可行：用作"增量变更检测"（非首次索引场景）

Spotlight/Everything/Windows Search 都持续跟踪文件系统变化（FSEvents / USN Journal）。对非首次的增量 reindex，可以直接查"自上次索引时间戳以来变更的文件"（`mdfind` 的 `kMDItemContentModificationDate >= ...` 或 Everything 的 `dm:>...`），而不必每次都全量 `WalkDir` + 逐文件 mtime 比对。对目录树大但变化少的场景（企业归档、长期使用用户）收益明显，但**不解决首次全量索引 1 万文件这个具体问题**——首次索引没有"上次时间戳"可比较。

### 2.3 不可行：直接复用 OS 已提取的正文来跳过 Scout 自己的文本提取/OCR

三个 backend 实现（[spotlight/src/lib.rs](../packages/search-backends/spotlight/src/lib.rs)、[windows-search/src/lib.rs](../packages/search-backends/windows-search/src/lib.rs)、[everything/src/lib.rs](../packages/search-backends/everything/src/lib.rs)）里，`mdfind`/`Search.CollatorDSO`/`es.exe` 全部只返回**匹配到的文件路径**——`kMDItemTextContent CONTAINS[cd] "X"` 只能用作查询谓词判断"是否命中"，不会把内容本身吐出来；Windows Search 的 `System.Search.Contents` 同样是"可查询但不可取值"的虚拟属性。操作系统索引只提供"这份文件是否匹配某关键词"的能力，不提供把已提取的全文本内容取回来的接口。Scout 自己的 OCR 文本、PII 类型词、语义向量更是操作系统索引完全没有的能力。B3-B5 的提取/OCR/embedding 性能问题无法通过复用 OS 索引绕开，必须靠自身架构优化。

### 2.3a 补充评估（2026-07-25 续轮）：结构化属性（title/author）取值 vs 全文取值

§2.3 的"不可行"结论针对的是**正文全文**（`kMDItemTextContent`/`System.Search.Contents`）——这两个只是查询谓词、不返值，结论成立。但用户提出的具体诉求是更窄的一类：**结构化属性**（`kMDItemTitle`/`kMDItemAuthors`、`System.Title`/`System.Author`）。这类属性在 API 语义上确实**可取值**而非只能匹配——macOS 有 `mdls <path>` / `MDItemCopyAttribute`，Windows Search OLE DB provider 支持 `SELECT System.Title, System.Author FROM SystemIndex WHERE System.ItemPathDisplay = '...'`，都是返回具体值、不是布尔匹配。所以"OS 索引完全不能读值"这个判断需要收窄——**结构化属性可读，全文不可读**。

但收窄之后评估这条路的实际收益，结论是**不建议投入**：

- Scout 自己对 docx/xlsx/pptx 的 title/author 提取（[doc_extract.rs:223](../packages/indexer/src/doc_extract.rs)、[doc_extract.rs:524](../packages/indexer/src/doc_extract.rs)）是解析 OOXML `core.xml` 时**顺带**拿到的，跟正文提取共用同一次 zip 打开 + XML 解析，没有独立成本可省——即便换成读 OS 索引的属性，也省不掉那次 zip 打开（body 还是要读）。
- PDF 同理：title/author（若有）是 pdf-extract/lopdf 解析文档信息字典时顺带读到的，不是独立步骤。
- 真正拖慢索引的是 B3/B4/B5（扫描版 PDF 的 pdftoppm 光栅化 + OCR 子进程、embedding 串行 batch=1）——这些都是 title/author 之外的正文/图像处理，OS 结构化属性帮不上忙。
- 唯二可能受益的场景：① title/author 在文件本身缺失、只有 OS 索引因为其他信号（如 NTFS 属主、Office 最近打开记录）侧面补全过的情况——真实占比未知，需要真机抽样才能判断值不值得；② 完全跳过 Scout 自己的 metadata 解析、只信 OS 索引——但那样会丢失"Scout 自己重新算一遍、跟正文 body 走同一条真值链路"的一致性保证，增加"两套 title 互相打架"的维护面。

**结论**：这项收益天花板低、且落地会引入新的一致性问题，本轮不安排实现；已有的 discovery 层（T5）已经把 Windows Search/Spotlight/Everything 用在了它们真正擅长的地方（路径枚举）。如果真机数据后续显示 title/author 缺失是真实的用户痛点（而非本文档现在的推测），再单独立项评估。

### 2.4 使用限制

- 工具可用性依赖第三方安装：Windows 上 `es.exe` 需要用户装 Everything；macOS 的 `mdfind` 系统自带但可能被用户关闭某些目录的 Spotlight 索引。必须像 `discovery.rs` 现在这样保留 `WalkDir` 全量回退，不能强依赖。
- 排除规则不同步：Scout 自己的 `ExcludeFilter` 与 Spotlight 隐私排除列表 / Everything 索引范围配置是两套独立配置，扩展到文档发现时需要在结果端做一次交叉过滤。
- OneDrive/网盘"仅在线"占位符判定仍需 Scout 自己做（[placeholder.rs](../packages/indexer/src/placeholder.rs)），OS 索引只给路径，不判定文件是否已下载到本地。

## 3. 优化设计与任务规划

### 3.1 设计原则

1. **先测量、后优化**——目前瓶颈判断基于代码走读和一次历史真机事故，第一步必须补上阶段级耗时埋点。
2. **保留优雅降级**——OS 索引加速永远是"可选加成"，工具不可用时回退现有 `WalkDir`/子进程链路，不引入新的强依赖。
3. **embedding 与 FTS 解耦**——daemon 已支持 `semantic_ready` 开关、embedder 不可用时优雅降级为纯 FTS；语义检索本身是暴力 cosine 扫描（无 ANN 索引），架构上早已承认 embedding 是"锦上添花层"。可以把 embedding 从首次索引关键路径里摘出来，做成后台低优先级补齐任务。

### 3.2 任务规划

**P0（1-3 天量级，低风险，本轮执行）**

| 任务 | 内容 | 收益 |
|---|---|---|
| T1 | 给 discovery/extract/write/embed 各阶段加 wall-time 埋点 | 验证瓶颈占比，为后续优先级排序提供真实数据 |
| T2 | DB 写入按 chunk（`EXTRACT_CHUNK`=64）批量提交事务，而非逐文件 commit | 减少上万次独立事务的固定开销 |
| T3 | 提取并发分级：音乐轮（无子进程）按核数放开；文档/图片轮（可能触发 OCR/pdftoppm 子进程）保持保守上限，env 可覆盖 | 高核数机器上音乐/轻量提取不再被摁在 4 并发；重活仍受控不打爆机器 |
| T4 | 单 PDF 内多页 OCR 改为在独立的、容量受限的子进程池内并行 | 大页数扫描件不再单核顺序跑完，同时子进程总并发仍有界 |

**P1（1-2 周量级，架构性改动，收益更大）**

| 任务 | 内容 | 收益 |
|---|---|---|
| T5 | 文档/图片发现层复用 Everything/Spotlight 全盘枚举，比照 `discovery.rs` 的 `AudioDiscovery` 模式加 `DocumentDiscovery`/`ImageDiscovery`，工具不可用回退 `WalkDir` | 把三次全盘遍历替换成秒级查询 |
| T6 | Embedding 阶段与提取/OCR 阶段 pipeline 化：文档一提取完 body 就立即入 embed 队列，或至少改成索引主流程完成后的后台低优先级异步任务 | 把 embedding 移出首次索引关键路径，用户更快能用 FTS 搜到刚索引的文件 |
| T7 | 增量 reindex 场景用 OS 索引的 mtime 查询代替全量 `WalkDir` + 逐文件 stat 比对 | 大目录树、小变化量场景的日常增量 reindex 显著加速 |

**P2（收益大但改动大，视 P0/P1 效果决定是否投入）**

| 任务 | 内容 | 收益 |
|---|---|---|
| T8 | OCR 常驻 worker 进程（尤其 Windows PowerShell+WinRT 冷启动重），改用长驻进程 + 管道通信 | 消除每张图/每页固定的进程启动开销 |
| T9 | Embedding 改批量推理（若 `llama_cpp_4` crate 支持多序列 batch），减少 per-call context 创建开销 | 提升 GPU 利用率、减少单文档 embedding 固定开销 |
| T10 | 评估 FTS5 索引延后批量重建（而非边插边建）的可行性，需权衡索引期间搜索可用性 gap | 降低写入路径上 CJK trigram 分词的实时成本 |

### 3.3 执行记录（2026-07-25）

**P0 全部完成**（[packages/indexer](../packages/indexer)）：T1 阶段耗时埋点（`scan.rs` 增量骨架 walk/extract/write/recycle 四段 + `daemon` 总耗时）、T2 `IncrementalStore::upsert_entries` 单事务批量提交（`MusicIndex`/`DocumentIndex` 均覆写，SQL 核心逻辑拆 `_tx` 内核供单条/批量共用）、T3 `extract_parallelism_for` 按音乐轮（无子进程，放开到核数）/文档图片轮（保守上限，`SCOUT_EXTRACT_PARALLELISM_LIGHT`/`_HEAVY` env 覆盖）分级、T4 单 PDF 页内 OCR 经独立容量受限 `page_ocr_pool`（默认 2，`SCOUT_PAGE_OCR_PARALLELISM` 覆盖）并行。`scout-indexer` 203 测试全绿。

**P1 完成 T5 + T6**：

- **T5**：[discovery.rs](../packages/indexer/src/discovery.rs) 新增 `PathDiscovery` trait + `default_document_discovery`/`default_image_discovery`（Windows `EverythingExtDiscovery` 泛化 `ext:` 查询、macOS `SpotlightExtDiscovery` 按扩展名 OR 谓词/图片走 `public.image` UTI），`DocumentIndex` 新增 `index_paths`/`index_image_paths`（比照 `MusicIndex::index_paths` 三段式）+ `prune_deleted`。[packages/search-backends/local-index](../packages/search-backends/local-index) 的三条生产 reindex 入口（`reindex_with`/`reindex_with_progress_inner`/`reindex_with_filter_and_progress_inner`）均接入文档/图片发现层，发现失败或不可用时逐级回退 `WalkDir`；BETA-47 的 `use_audio_discovery` 开关改名 `use_platform_discovery`、同时控制三路发现（用户心智是"关 Everything 集成"这一件事）。
- **T6**：[apps/daemon/src/main.rs](../apps/daemon/src/main.rs) 的 `embed_pending`/`purge_short_body_vectors` 从 `run_initial_collection_index` 的同步四阶段链路中摘出，改为 `spawn_background_embedding`——`CollectionRuntime` 构建完成（音乐/文档/图片三轮 FTS 索引已就绪）后即可搜索并对外提供服务，语义向量在后台 `tokio::spawn` 任务里补齐，不阻塞启动、也不阻塞后续 collection 的索引。

**T7 及 P2 本轮未做**：T7（增量 reindex 用 OS mtime 查询代替全量 walk）评估后判断工作量/风险相对 T5/T6 收益较低，留后续轮；P2（OCR 常驻 worker、embedding 批量推理、FTS5 延后重建）按原计划视 P0/P1 真机实测效果决定是否投入。

**已知限制（本轮未处理，非本轮引入）**：文档/图片发现层复用了与音乐发现相同的边界行为——真实 Spotlight/Everything 索引存在异步延迟，`discover()` 成功但返回空结果时不会自动回退 `WalkDir`（只有工具本身不可用才回退）。刚创建、尚未被系统索引收录的文件在这类场景下可能暂时搜不到，等系统索引追上或下次触发增量扫描即可恢复；这是 BETA-01A 音乐发现层已接受的既有特征，非本轮新增风险。

**验证现状**：本地 macOS 沙盒内 `cargo test`/`clippy -D warnings`/`fmt --check` 全绿（`scout-indexer`/`scout-local-index-backend`/`scoutd`/`scout-server`/`scout-harness` 等 CI 覆盖的 7 个 crate + `scout-local-index-backend`）。`scoutd` e2e 测试有 3 个失败，经 `git stash` 对照确认改动前基线同样失败，系本机沙盒 macOS `/var` vs `/private/var` 临时目录路径问题（与 ROADMAP BETA-63 记录的同一根因），与本轮改动无关。**2026-07-25 更新**：`v0.9.37` 已发布——GitHub CI 全绿；Release Windows（29m28s）与 Release macOS（7m43s）均构建成功，`#[cfg(windows)]` 代码路径（`EverythingExtDiscovery` 等）本机 macOS 无法编译验证的缺口已由 Windows Release 构建补上（编译通过，功能尚未真机验证）。**仍待验证**：真机功能验证（发现层实际枚举正确性、后台 embedding 与并发 search 交互、首次索引真实耗时改善程度）留待下一轮真机测试。

### 3.4 结论

当前"1 万文件 1 小时+"不是单点问题，而是**四阶段完全不重叠 + 提取并发被硬顶在 4 + embedding 纯串行单 batch**三者叠加的结果；Windows Search/Spotlight/Everything 能帮上忙的地方只有"发现文件路径"和"检测变更"这两步（且已有 BETA-01A 音乐索引的成功先例可直接复制），指望它们跳过 OCR/文本提取/embedding 本身不现实，那部分必须靠 P0/P1 的并发与流水线重构解决。

## 4. P1.5：下一轮迭代（2026-07-25 续轮）

用户复查 P0/P1 落地情况后指出四点，逐条排查代码现状如下，均已定位到具体文件行：

1. **批量 mtime 预加载未落地**——[scan.rs:430](../packages/indexer/src/scan.rs) 的增量比对循环此前对每个扫描到的文件单独调一次 `store.modified_time_of(&path_str)`（一次 `SELECT ... WHERE path=?1`），走的是 walk 主循环内的同步 DB 往返；`paths_under`（回收阶段用）已经证明了"整表 SELECT 一次、Rust 侧按 root 过滤"这个更便宜的模式，但 mtime 比对阶段没复用同款思路。
2. **reindex 时三轮扫描仍在一个 `spawn_blocking` 严格串行**——细查后发现准确说法是：`apps/daemon/src/main.rs` 的首次索引路径（`run_initial_collection_index`）在上一轮 T6 已经把 embedding 从同步链路摘出去了，**但 [reindex.rs](../packages/scout-server/src/reindex.rs) 的管理员触发增量 reindex（`/admin/reindex`）是独立实现、没有跟着改**——`run_collection_reindex` 仍在同一个 `spawn_blocking` 里顺序跑 音乐 → 文档 → 图片 OCR → `embed_pending` 四步，T6 的"FTS 就绪即可搜索、语义向量后台补"这条设计原则在这条路径上没有生效，是本轮改动前唯一还留着的四阶段串行入口。音乐/文档/图片三轮本身（FTS 部分）目前仍保持顺序——见下方"未做"说明。
3. **Windows Search/Spotlight 元数据预填充（title/author）未落地**——细查确认属实，且评估后判断投入产出比低，见上方 §2.3a，本轮不实现。
4. **embedding 索引构建仍无 batch 处理**——[doc_db.rs:874](../packages/indexer/src/doc_db.rs) `embed_pending` 确认是 `for (i, ...) in pending.iter().enumerate()` 纯顺序循环；深挖一层发现比 B5 描述的还要重：[model-runtime/src/llama.rs:402](../packages/model-runtime/src/llama.rs) 的 `run_embed` 是**每次调用都新建一个 llama.cpp context**（`model.new_context(...)`）、用一次就弃，`worker_main` 只对 `generate_cached_prefix` 复用了 `PrefixSession`，`Request::Embed` 完全没有等价的常驻 context。这是比"没有多序列 batch"更基础的问题——即使不做真正的多序列并行推理，光是消除每文档一次的 context 创建/销毁开销就已是独立可做的一步。

### 4.1 本轮已落地

**T7a｜DB 侧批量 mtime 预加载**（[scan.rs](../packages/indexer/src/scan.rs)、[db.rs](../packages/indexer/src/db.rs)、[doc_db.rs](../packages/indexer/src/doc_db.rs)）：`IncrementalStore` trait 新增 `modified_times_under(roots) -> HashMap<path, mtime>`，`MusicIndex`/`DocumentIndex` 各覆写为一条 `SELECT path, modified_time FROM ...` 全表查询（root 过滤逻辑同 `paths_under`，SQL 侧零额外成本）；`run_incremental_index_with_filter_and_progress` 在 walk 循环开始前调一次，比对阶段改查内存 `HashMap`；回收阶段直接复用同一份 map 的 key 集合，不再对 `paths_under` 二次全表查询。效果：单轮增量扫描的 DB 往返次数从"未变化文件数 N 次 + 1 次回收全表扫"降到"1 次全表扫"，对"大目录树、日常小变化"的典型增量 reindex 场景（改动文件占比低、多数文件走 skip 分支）收益最直接。**属于 T7（OS 索引 mtime 查询）的低风险子集**——T7a 只动 Scout 自己的 DB 查询模式，不引入 Everything/Spotlight 依赖，T7 本身（改用 OS 索引查"自上次时间戳以来变更"）仍留待后续评估。

**T6b｜reindex.rs 补齐 T6 的后台 embedding 解耦**（[reindex.rs](../packages/scout-server/src/reindex.rs)）：`run_collection_reindex` 拆成两段——`spawn_blocking` 内只跑音乐/文档/图片 OCR 三轮 FTS（返回后立即可搜索、guard 释放、HTTP 响应返回），`embed_pending`（含 ping 探测）移到独立的 detached `tokio::spawn` 后台任务 `spawn_background_embed`，与 `apps/daemon/src/main.rs` 的 `spawn_background_embedding` 同一模式（两处未合并成共享 helper——分属不同 crate、触发时机不同，勉强合并增加的间接层不划算）。`document_index` 是同一把 `parking_lot::Mutex`，后台 embedding 与后续并发 search / 下一次 reindex 天然靠锁排队，不会数据竞争；`embed_pending` 本身幂等可续（`vector_is_current`/`content_hash` 去重），并发触发不会重复计算。验证：`scout-server`/`scout-indexer` 全部单测（205 + 95）+ `clippy -D warnings` + `fmt` 全绿。

### 4.2 已设计、待定实现范围：T9 embedding 批量化

细分两层，风险和收益都不同，建议分开决策：

**T9a（快win，风险低）：embed context 常驻复用**。当前 [llama.rs](../packages/model-runtime/src/llama.rs) 的 `Request::Embed` 分支每次调 `run_embed` 都新建 context（`model.new_context`），而 `Request::GenerateCached` 分支已经证明了"context 跨调用复用"的模式（`PrefixSession` 存在 `worker_main` 的局部变量里，跨消息存活）。可以照此加一个 `embed_ctx: Option<LlamaContext>`（按需首次创建、复用到 worker 退出），把"新建 context"这个固定开销从"每文档一次"降到"整个 embedding 阶段一次"。`embed_pending` 一轮通常是几百到几千篇文档，这个改动预期直接把 context 创建/销毁的固定成本摊掉大半。

**T9b（大改动，风险较高）：多序列 batch decode**。`LlamaBatch::new(n_tokens, n_seq_max)` 的第二参数已经是"每 batch 支持的最大序列数"，当前固定传 1；理论上可以把 `pending` 里的多篇文档打包进同一个 `LlamaBatch`（每篇占一个 `seq_id`）、一次 `ctx.decode()` 出多篇的池化向量，真正提升 GPU 利用率。但这条改动：
- 需要确认 `llama_cpp_4` crate 版本对多序列 embedding pooling 的支持边界（`embeddings_seq_ith(seq_id)` 是否对每个 seq 独立正确池化，还是只在 seq 0 可靠——当前代码注释明确写着"llama.cpp 在 decode 后把池化结果写入 **seq 0** 的 embedding 槽"，如果这个假设对多 seq 场景不成立，需要先在真机 + 真模型上验证，不能只凭 stub 测试通过）；
- 涉及变长文本打包进同一 batch 时的 padding/position 处理，出错模式是"静默返回错向量"而非报错，比 T9a 的失败模式（顶多退化到原有单条串行）更隐蔽；
- 该模块历史上出现过 native crash（`ucrtbase 0xc0000409`，[embed.rs](../packages/indexer/src/embed.rs) 顶部注释有记录），属于"改错了会整进程炸、Rust `catch_unwind` 兜不住"的高风险区。

本轮不直接实现 T9a/T9b——**T9a 值得下一轮单独排期**（改动面小、有 `PrefixSession` 先例可循、失败模式温和）；**T9b 建议先补一份真机 profile（T1 埋点已能拿到 `embed_ms`/pending 篇数），确认"每次新建 context 的固定开销"和"单次 decode 本身的计算耗时"的实际占比——如果 T9a 落地后大部分耗时已经是 T9b 无法进一步优化的计算本身，T9b 的额外风险就不值得冒**。

## 5. P2 收尾（2026-07-25 三续轮）：T7/T8/T9 全部处理完毕，T10 评估后不投入

对照本文档 §3.2/§4 的任务表复查，本轮把清单剩余项逐一处理完：T7（完整版）落地、T8 落地、T9a 落地并**真机验证**（过程中揪出一个真实的 shutdown 竞态并修复）、T9b 用真机实验拿到确定性结论——**不可行**，不是"风险高所以不做"、而是当前依赖版本的 API 边界决定了做不了。T10 评估后判断与已落地的 T2/T6 冲突、净收益不明，不建议投入。

**验证方法论说明**：这一轮不少改动能拿到比"代码走读 + clippy/单测通过"更硬的验证——本机沙盒恰好有 `llama-cpp,metal` 可编译（Apple M5 Pro，真 Metal 后端）、且机器上有另一个应用（`ai.linkly.desktop`）留下的真实 `Qwen3-Embedding-0.6B-Q8_0.gguf` 模型文件，两者叠加让 T9a/T9b 可以用真模型 + 真 GPU 实测，而不是止步于"stub 测试通过"。T8（Windows-only、PowerShell+WinRT）和 T7 的 Windows 分支（`EverythingExtDiscovery`）无法在本机做 Windows 真机运行验证，但本轮已经补齐 `x86_64-pc-windows-gnu` 目标与 MinGW 工具链，并跑通 `cargo check`/`clippy -D warnings`；因此 Windows 分支已完成编译、类型与 lint 验证，仍需由本次 Release 产物补做真机功能验证，下面逐项如实标注。

### 5.1 T7（完整版）｜发现层批量 mtime 预取

§4.1 落地的 T7a 只覆盖了 `run_incremental_index_with_filter_and_progress`（`WalkDir` 路径）的 mtime 比对。复查后发现一个更关键的缺口：**T5 落地后，`WalkDir` 已经不是生产环境的主路径了**——[packages/search-backends/local-index](../packages/search-backends/local-index) 的三条生产 reindex 入口优先走发现层（`MusicIndex::index_paths` / `index_discovered_paths`，Everything/Spotlight 枚举出路径后调用），只有发现层不可用时才回退 `WalkDir`。而这两个发现层入口函数（[scan.rs](../packages/indexer/src/scan.rs)）当时仍是逐路径调 `modified_time_of`——也就是说，T7a 修的那条路径在真机上（装了 Everything/Spotlight 都可用的常见场景）反而**不会被走到**，性能提升主要惠及的是发现层不可用时的兜底路径，不是主路径。

修法：`MusicIndex`/`DocumentIndex` 各新增一个 `all_modified_times()`（[db.rs](../packages/indexer/src/db.rs)/[doc_db.rs](../packages/indexer/src/doc_db.rs)，一次 `SELECT path, modified_time FROM ...` 全表读、不做 root 过滤——发现层给的路径可能跨多个甚至没有 root 前缀，不能复用 `modified_times_under` 的 root 过滤语义），`MusicIndex::index_paths` 与 `index_discovered_paths`（后者是 `DocumentIndex` 的 `index_paths`/`index_image_paths` 共用骨架）都改成调用期开头预取一次、比对阶段查内存 `HashMap`。

这才是 T7 原始任务描述（"增量 reindex 场景用 OS 索引的 mtime 查询代替全量 `WalkDir` + 逐文件 stat 比对"）里"代替逐文件 stat 比对"这半句在 T5 已经把发现层做成主路径之后的真正落点——**不需要真的去问 Everything/Spotlight"自上次时间戳以来变了什么"**（那是 §2.2 讨论过的另一条路，收益递减：T5 已经把全盘枚举做到秒级，T7a+本次改动又把 mtime 比对从 O(N) 查询降到 O(1)，两者叠加后，"发现路径"和"比对 mtime"这两步都已经不是瓶颈，再加一层"只问变更"的 OS 时间戳查询边际收益很小、还要多扛一份"发现层结果与 Scout DB 记录的时间戳定义是否一致"的正确性负担）。

验证：`cargo test -p scout-indexer` 205→209 测试全绿（含 `document_index_paths_indexes_explicit_list_and_is_searchable`/`index_paths_real_wav_parallel_extracts_and_searchable`/`index_paths_skips_unchanged_on_rerun` 等已覆盖这两个函数的存量测试）；`clippy -D warnings`/`fmt` 全绿。跨平台代码，本机 macOS 验证有效、不存在"Windows-only 验证不到"的问题。

### 5.2 T9a｜embed context 常驻复用（已实现，真机验证通过，过程中修了一个真实 bug）

[llama.rs](../packages/model-runtime/src/llama.rs) 的 `run_embed` 从"每次调用新建 context"改为"`worker_main` 持有一个 `embed_ctx: Option<LlamaContext>`，首次懒创建、跨调用复用；每次 decode 前显式 `ctx.clear_kv_cache()`"——`clear_kv_cache()` 是必需的，不是可选优化：KV cache 跨 decode 累积（`generate_cached_prefix` 的 `PrefixSession` 正是利用这个特性做前缀复用），若不清，本次 embedding 的池化结果会掺进上一次调用残留在 KV cache 里的 token，这是与"context 复用"绑定出现的新正确性要求，`PrefixSession` 那条路径不需要处理是因为它故意要保留 KV。

**真机验证**（本机 Metal + 真实 `Qwen3-Embedding-0.6B-Q8_0.gguf`，见上方"验证方法论说明"）：三次调用 `a → 不同文本 b → 再 a` 验证 (1) 两次 `a` 的向量逐位几乎相同（`clear_kv_cache` 确实清干净了，没有把 `b` 的残留掺进第二次 `a`）、(2) `a`/`b` 的向量明显不同（cosine 0.35，不是"复用 context 后所有向量趋同"这种更隐蔽的池化污染）。数值结果（cosine=0.35020006）在改动前后逐位相同，证明改动不改变计算结果，只改变 context 生命周期。

**过程中发现并修复的真实 bug**：第一版实现（只加 `embed_ctx` 复用，不改其他）在测试进程退出时稳定触发 `ggml_metal_device_free` 里的 `GGML_ASSERT([rsets->data count] == 0)` 断言、SIGABRT。用同一份测试对照"改动前/改动后"代码定位到根因——`LlamaModelImpl` 的 worker 线程句柄字段原名 `_handle: JoinHandle<()>`，从未被 `join()`（`JoinHandle` 的 `Drop` 既不 join 也不 detach，线程会继续在后台跑）；T9a 之前，`run_embed` 每次调用都在函数体内创建、使用、drop 同一个 context，drop 早在 `reply.send()` 之前就已经跑完，context 生命周期被同步请求/响应边界天然框住，进程退出时不会有残留的 GPU 资源在等释放；T9a 起 context 挪到 `worker_main` 里跨调用常驻，只有 worker 线程真正退出主循环时才释放——不 join 就没有同步点保证这件事在进程退出前发生，与 ggml-metal 全局设备表在 `exit()` 时的静态析构产生竞态。修法：`LlamaModelImpl` 新增 `impl Drop`，显式关闭 channel + `handle.join()`，阻塞到 worker 线程完整退出（含释放常驻 `embed_ctx`/`session`/`model`）才返回。修复后同一测试跑 5 次全部干净退出（exit code 0），且这个 join 本身也是更正确的资源管理，不只是"绕开这一次崩溃"。

这个发现印证了"能在真机上跑就应该在真机上跑"——这类竞态在代码走读、`clippy`、甚至 stub 测试下完全不可见,只有真实创建/持有原生 GPU 资源、真实经历进程生命周期时才会暴露。

验证：`cargo test -p scout-model-runtime --features llama-cpp,metal` 24 passed（含新增的真机冒烟测试 `beta64_t9a_embed_context_reuse_does_not_leak_kv_state`，默认 `#[ignore]`，`SCOUT_BETA64_EMBED_MODEL=<gguf 路径>` 手动跑）；`--features llama-cpp,metal` 与默认 stub 两套 feature 组合的 `clippy -D warnings`/`fmt`/`cargo test` 均全绿。

### 5.3 T9b｜多序列 batch decode：真机实验确认不可行（非"风险高、待评估"）

§4.2 把 T9b 列为"待真机验证支持边界"的高风险项。本轮直接做了这个验证，结论是**确定性的不可行**，原因是依赖版本的 API 边界、不是运气或调参能解决的：

- `llama_context_params`（`llama_cpp_sys_4` 透出的原始 C 结构体）确实有 `n_seq_max` 字段，但 `llama-cpp-4 = "0.3.0"` 这个安全封装版本**没有暴露任何 builder 能修改它**——`LlamaContextParams::default()` 直接透传 llama.cpp 的 C 默认值，语义等价于 `n_seq_max = 1`；且该字段是 `llama_cpp_4` crate 内部 `pub(crate)`，`scout-model-runtime` 拿不到访问权，即便想用 `unsafe` 硬改也做不到（况且本仓库 `unsafe_code = "forbid"`，workspace lint 层面也不允许）。
- 用真实 gguf + 真机 Metal 做了直接实验（临时探针，验证后已移除，未进正式代码）：创建一个 `n_ctx=2048` 的默认 context（`ctx.n_seq_max()` 实测确实是 `1`），构造一个 2 序列的 `LlamaBatch::new(total_tokens, 2)`（两段不同文本各占一个 `seq_id`）尝试一次 `decode()`。结果**不是崩溃**（比最坏情况好——llama.cpp 自己的校验层挡住了），而是干净的失败：`init: invalid seq_id[4][0] = 1 >= 1` → `decode: failed to initialize batch` → `llama_decode: failed to decode, ret = -1` → Rust 侧 `ctx.decode()` 返回 `Err`，两个 `embeddings_seq_ith` 全部读不到值。

**结论**：T9b 在当前 `llama_cpp_4` 版本下**没有安全实现路径**，不是"值得为之冒进程崩溃风险的高收益项"，而是"这条路走不通"。若未来升级 `llama_cpp_4` 到暴露 `n_seq_max` builder 的版本，可以重新评估——但那是一次独立的依赖升级决策（要看升级本身的兼容性成本），不是本轮范围。已确认 T9a（context 复用，§5.2）已经吃掉了"每次新建 context"这个 B5 记录的主要固定开销，T9b 想在此基础上再挤的是"单次 decode 本身跨文档摊薄"这一块——挤不出来，不是遗憾，是收益本来就没有独立于 T9a 的空间。

### 5.4 T8｜OCR 常驻 worker（已实现；交叉编译验证通过，运行时行为仍待 Windows 真机）

[ocr.rs](../packages/indexer/src/ocr.rs) 的 `WindowsOcrEngine` 从"每张图片新起一个 PowerShell 进程、重新走一遍 WinRT 类型加载"改为常驻 worker 模式：

- 新增 [ocr/win_ocr_worker.ps1](../packages/indexer/src/ocr/win_ocr_worker.ps1)——与原 `win_ocr.ps1` 做同样的 WinRT 类型加载 + OCR 逻辑，但类型加载只在进程启动时做一次，随后循环处理请求：stdin 逐行读图片路径，stdout 逐行回响应（`OK:<base64 UTF-8 文字>` / `ERR:<原因>`）。响应正文 base64 编码是关键设计——识别文字可能含任意 Unicode（含换行），不编码会破坏"每行一响应"的 framing。
- Rust 侧 `WindowsOcrEngine` 新增 `worker: Mutex<Option<ResidentOcrWorker>>`（懒创建、跨 `recognize` 调用复用）；`spawn_worker` 起进程后另起一个读线程把 stdout 逐行转发进 `mpsc::channel`，`recognize` 侧用 `recv_timeout` 等一行响应（同步 `read_line` 没有超时机制，这是绕不开的必需设计，不是过度工程）。
- 降级路径：`recognize_via_worker` 内任何"worker 基础设施不可信"的失败（spawn 失败 / 拿不到管道 / 写入失败 / 超时 / 响应协议异常 / 响应正文 base64/UTF-8 解码失败）都会清空 worker 状态并返回 `Err`；`recognize` 据此为**当次调用**回退一次性子进程（原 `win_ocr.ps1` 路径，T8 之前的实现原样保留），给这张图一次不依赖常驻进程状态的机会。唯一不回退的是 worker 本身健康、只是这张图识别失败（脚本 `try/catch` 捕获后回的 `ERR:` 行，如文件不存在/损坏）——这种失败一次性进程大概率复现同样结果，直接计 `failed` 更诚实：提取失败的文件没有入库记录，下一轮增量 reindex 的 mtime 比对必然判定"待处理"，天然会重试。

**本轮验证能做到的**：`base64_encode`/`base64_decode`（worker 响应 framing 的编解码核心）被有意设计成不含平台依赖的纯函数、故意不 `#[cfg(windows)]`，本机 macOS 跑了真实的往返测试（含 UTF-8 多字节字符、边界情况）——**过程中真的抓到一个 bug**：`base64_decode("")` 最初被错误地判成非法输入返 `None`，但空字符串是 `base64_encode(b"")` 的合法产出（对应"图片里没识别出任何文字"这种很常见的真实场景——空白图片/纯图形截图），若不修，会导致这类图片在常驻 worker 路径下被错误地整张计 `failed`，而不是正确地记一条空文本；已修复并补充多条测试锁定（`base64_round_trips_arbitrary_bytes_including_utf8`/`base64_decode_rejects_malformed_input`）。`clippy -D warnings` 还额外抓到两处（`naive_bytecount`/`manual_contains`）——这两处如果代码留在 `#[cfg(windows)]` 里，本机 clippy 根本不会检查到，只会在 Windows CI 才报错；解禁平台限制后本机验证就已经拦住了。

**交叉编译验证（本轮补齐）**：worker 进程管理本体（spawn/管道读写线程/超时/kill 重来）整段是 `#[cfg(windows)]`，本机 macOS 原生 `cargo build`/`clippy`/`test` 完全碰不到它——最初一版只能靠代码走读自证正确。本轮装上 `x86_64-w64-mingw32-gcc`（`brew install mingw-w64`，编译耗时较长但最终跑通，卡点是 `libsqlite3-sys` 需要它编译内嵌 SQLite C 源码）后，补了一层此前没有的验证：`rustup target add x86_64-pc-windows-gnu` + `cargo check`/`cargo clippy -D warnings --all-targets -p scout-indexer --target x86_64-pc-windows-gnu`，进一步验证了 `scoutd`（daemon 实际发布的二进制）整体 `cargo check`/`cargo clippy -D warnings --target x86_64-pc-windows-gnu` 全绿。**这一步确实抓到了两类此前代码走读没发现的真实问题**：
- 编译错误：`match line { Ok(l) if tx.send(l).is_ok() => {} , _ => break }`——模式守卫（match guard）里不能移动被绑定的变量 `l`（Rust 规则：guard 只能读，不能 move），`String` 非 `Copy`，触发 `E0507`。改成先 `let Ok(l) = line else { break }` 再在 guard 外 `tx.send(l)`。
- 4 处 `clippy -D warnings` 才报的 lint（这些在 `#[cfg(windows)]` 门内、本机 macOS clippy 根本扫不到）：match 单分支应改 `if let`（两处，`recv_timeout` 结果分支 + base64 解码结果分支）、`unwrap_or_else(|e| e.into_inner())` 应写成方法引用、`use` 语句出现在其他语句之后。均已修复，改用 `let-else` 简化控制流。

这证明"交叉编译类型检查"和"同平台单测"是两种互补但都不可替代的验证手段——前者这次抓到的是编译期错误和 lint（本轮 T9a 真机测试没有、也不可能抓到这类问题，因为那次测试跑在 macOS/Metal，根本不经过这段 Windows-only 代码）；至此，T8 改动首次在**任何**编译器上验证通过（此前只有代码走读）。

**仍然验证不到的**：交叉编译只能证明"类型对、借用检查过、lint 干净"，不能证明运行时行为——`x86_64-pc-windows-gnu` 产出的二进制在这台 macOS 上不能执行（没装 Wine），常驻进程是否真的省下预期的固定开销、PowerShell `-EncodedCommand` 常驻脚本的协议 framing 在真实中文 OCR 文本上是否有未预料的边界情况、降级路径在真实故障场景（如用户中途拔网线导致的诡异管道状态）下是否如预期触发，仍然只能靠 Windows 真机或 Windows CI 补齐——这与 T5 落地时 `EverythingExtDiscovery` 走的路径一致（当时也是先靠 Windows Release CI 构建补上本机 macOS 编译不到的缺口，功能验证留给真机）。

**建议**：合并前跑一次 Windows CI（编译已在本机交叉验证过，预期通过）；真机功能验证与 T5 遗留的"真机功能验证"清单合并处理。

### 5.5 T10｜FTS5 延后批量重建：评估后不建议投入

复核当前写入路径（[doc_db.rs](../packages/indexer/src/doc_db.rs) `upsert_document_with_pages_tx`/`db.rs` `upsert_music_entry_tx`）确认 `documents_fts`/`music_fts` 的 `INSERT` 与主表 `INSERT`/`UPDATE` 在**同一个事务**里（T2 落地后是同一个 chunk 批量事务），FTS5 trigram 分词的成本目前确实摊在写入路径上，B6 的判断没错。但评估"延后到整轮结束批量重建"这个具体方案后，判断不建议投入：

1. **与已落地的 T6/T6b 设计原则直接冲突**：T6/T6b 把"FTS 就绪即可搜索、语义向量后台补"确立为本项目的架构原则（daemon 首次索引与管理员触发的增量 reindex 都已经照此改造）。FTS5 延后重建等于把这个"不可搜索窗口"从"零"（现在——文档一写入主表、同一事务里 FTS 立刻可查）拉长到"整轮索引结束前"，是对刚落地的核心设计原则的倒退，不是在它之外的独立优化。
2. **崩溃安全性倒退**：现在的事务边界保证"提交的主表行 = 可搜索的行"，进程中途崩溃/被杀，已提交的 chunk 依然可搜。延后重建意味着索引期间的所有主表写入在最终批量重建 FTS 之前都不可搜——如果整个索引过程被中断（真实场景：用户强制退出、断电、OOM kill），已经落盘的主表数据会处于"有记录但搜不到"的状态，且需要额外记录"FTS 是否已重建"这层状态才能安全恢复，复杂度净增。
3. **T2 已经把这一块的固定开销打掉了大半**：B6 原始测量的基线是"逐文件独立事务提交"（一万文件一万次 commit）；T2 落地后已经是"按 chunk（64）批量提交"，FTS insert 本身还是在写入路径上，但事务提交的固定开销已经不再是"每文件一次"。延后重建相对**当前**基线（T2 之后）的边际收益，与相对 T2 之前基线的边际收益不是一回事，前者明显更小，具体数字需要先有 profile 数据（T1 埋点目前的 `write_ms` 是"批量 upsert + FTS insert"合并计时，没有单独拆出 trigram 分词耗时这一项）才能量化，而不是可以推测的。
4. **收益方向如果成立，也有风险低得多的替代方案**：如果 profile 数据将来证明 trigram 分词确实是显著瓶颈，`documents_fts`/`music_fts` 的 `INSERT` 已经和主表 `INSERT` 在同一 chunk 事务里、按 [`EXTRACT_CHUNK`]=64 批量执行——调大这个 chunk size（`EXTRACT_CHUNK` 目前是编译期 const，B7 已经指出这类参数缺运行时覆盖入口）能进一步摊薄单行固定开销，且不改变"提交 = 可搜索"这条不变量，风险远低于整体重构成延后批量模式。

**结论**：不实现。若未来 profile 数据证明 FTS 写入是真实且显著的瓶颈，优先方向是"调大 chunk size + 补运行时可调参数"（成本低、不破坏崩溃安全 / 搜索可用性不变量），而非"延后重建"这个会引入新一致性问题的方案。

## 6. 再挖一轮：三个此前未提出的并行加速点（2026-07-28）

复查 §1-5 的执行记录后发现，P0-P2 解决的都是"单阶段内部"的并发问题（提取并发分级、单 PDF 页内 OCR 并行、embedding context 复用），但**阶段之间**（文档/图片/音频三轮）仍是严格顺序，且 macOS 的 OCR 路径从未被 T4/T8 覆盖过。走读 [apps/daemon/src/main.rs:349-370](../apps/daemon/src/main.rs)（`run_initial_collection_index`）确认：T6/T6b 只把 embedding 摘出了同步链路，音频 → 文档 → 图片三轮 FTS 本身仍是三次独立的 `.index_dirs_with_progress()` 顺序调用，没有变。

### 6.1 文档/图片/音频三轮 FTS 阶段间并行（新发现，P0-P2 均未覆盖）

**现状**：`run_initial_collection_index` 与 `reindex.rs` 的 `run_collection_reindex` 都是"doc → image → music"顺序执行，即便三者读写的是三张不同的表（`music`/`documents`/`document_pages` 等）、且三类负载的 CPU/IO 特征差异很大——音频轮几乎纯 CPU（lofty 内存解析，无子进程）、文档轮读写混合（多数轻量、少数触发 OCR 子进程）、图片轮恒定触发 OCR 子进程（IO 等待为主，CPU 反而空闲）。三轮顺序执行意味着**音频轮跑的时候，图片轮该发的 OCR 子进程一个都没发出去**——这是当前架构里最大的一块"本可重叠但没重叠"的时间。

**可行性**：SQLite 同库 WAL 模式支持多个写连接排队（`busy_timeout` 已有配置，[db.rs](../packages/indexer/src/db.rs)/[doc_db.rs](../packages/indexer/src/doc_db.rs) 各自持有独立连接），三轮写的是不同表、`upsert_entries` 的 chunk 事务本身已经很短，理论上可以让三轮各自在自己的 `tokio::task::spawn_blocking`（或独立线程）里跑，写库时天然靠 SQLite 的写锁排队，不需要额外加锁设计。发现层（T5 落地的 `PathDiscovery`）三路查询（`ext:` 文档 / 图片 / 音频）本身也可以一次性并发发起，而不是进哪个 phase 才发哪个查询。

**代价与风险**：① 三个提取线程池（`extract_pool`）同时活跃时，`SCOUT_EXTRACT_PARALLELISM_LIGHT/_HEAVY` 的预算需要重新设计成"全局子进程并发上限"而非"单轮内上限"——否则音频轮的核数并发 + 文档/图片轮各自的 OCR 子进程叠加，会重现 BETA-60 v0.9.27 那次"17 个 pdftoppm 打爆机器"的故障模式，这是本项最大的风险点，必须先做一个跨轮共享的信号量（如 `Arc<Semaphore>` 传给三轮共用）再动手，不能三轮各自独立限流；② `IndexStatus.current_phase` 目前是"当前唯一阶段"的单值语义（§7 会展开），并行后需要改成"三轮各自独立进度"的结构，UI 展示逻辑要跟着改；③ 进度条语义变化对 §7 设计有直接影响，两者需要同一轮做。

**建议**：作为独立任务排后续（不在本轮 P0/P1 打包），先补一次真机 profile（已有的 `walk_ms`/`extract_ms`/`write_ms` 三段 tracing 日志）看文档/图片/音频三轮各自实际耗时占比——如果某一轮（例如音频轮，通常文件数少、纯内存解析）本身耗时占比很小，并行它的收益有限、不值得为此扛共享限流的复杂度；如果三轮耗时量级相近，并行收益接近"三轮耗时最大值"而非"三轮之和"，值得投入。

### 6.2 macOS 图片 OCR：无原生引擎、且 Tesseract 路径未享受 T8 的常驻 worker 处理

**现状**：[ocr.rs:1-9](../packages/indexer/src/ocr.rs) 头部注释写明"macOS Vision 留后续（trait 已抽象）"——**macOS 上目前没有任何原生 OCR 实现**，`default_ocr_engine()`（[ocr.rs:256](../packages/indexer/src/ocr.rs) 附近）逻辑是"Windows 走 `WindowsOcrEngine`，否则 PATH 上有 `tesseract` 才用 `TesseractOcrEngine`，都没有则图片索引整体优雅跳过"。也就是说：**默认 macOS 用户完全不装额外软件时，图片里的文字是索引不到的**——这不只是性能问题，是功能缺口，且开发机恰好是 macOS，值得优先关注。即便用户装了 `tesseract`，[TesseractOcrEngine::recognize](../packages/indexer/src/ocr.rs:791) 仍是 B4 描述的"每张图新起一个子进程"模型，T8 只把 Windows 的 `WindowsOcrEngine` 改成了常驻 worker，Tesseract 引擎完全没有对应改造——`tesseract` 命令本身不支持"常驻监听 stdin"模式（每次调用都是完整的进程生命周期），无法照搬 T8 的"长驻进程 + 管道协议"模式，只能靠"批量一次调用处理多图"（`tesseract` 支持一次传入图片列表文件）或"提高子进程池并发度"来摊薄。

**建议**：① 短期（低风险）：给 `TesseractOcrEngine` 也接入 §5.4 同款的容量受限并发池（当前 macOS 图片轮的 OCR 调用是否已经过 `extract_parallelism_for` 分级需要复核，若仍是全局 `EXTRACT_PARALLELISM=4` 摁住，至少先放开到独立的 heavy 预算）；② 中期：评估 `tesseract` 的批量调用模式（一次进程处理一批图片路径，用输出文件名前缀区分结果）替代"一图一进程"，收益与 T8 常驻 worker 相近但实现方式不同（不是常驻协议、是攒批调用）；③ 长期：macOS 原生 `Vision` 框架（`VNRecognizeTextRequest`）识别质量和速度都显著优于 Tesseract 且系统自带零依赖，但项目 `unsafe_code = "forbid"` 约束下不能直接 Obj-C FFI——可以照搬项目对 WinRT 的处理方式：写一个极小的 Swift/Obj-C 命令行 helper 二进制（编译进 app bundle，类似 Windows 那支内嵌 `.ps1`），Rust 侧 shell-out 调它，同样能做成 T8 式的常驻 worker（stdin 收图片路径、stdout 吐 JSON 结果）。这一项收益最大（免用户装依赖 + 原生速度 + 可复用 T8 的常驻协议设计）但改动面也最大，建议单独立项评估，不并入本轮任务表。

### 6.3 Extract/Write 阶段流水线化（低优先级，T2 落地后收益已收窄）

[scan.rs:534-581](../packages/indexer/src/scan.rs) 的主循环是"提取一个 chunk（并行）→ 写这个 chunk（串行事务）→ 下一个 chunk"，提取与写入之间是硬同步点：写这 64 个文件时，刚才干活的提取线程池是闲着的。理论上可以用一个有界 channel 把"提取生产者"和"写入消费者"解耦成流水线（提取 chunk N+1 与写入 chunk N 同时进行），但 T2 落地后 `write_ms` 已经是"64 条一次事务"的量级，相对 `extract_ms`（尤其触发 OCR/pdftoppm 的轮次）占比预计已经很小——**先用 T1 埋点的真实 `extract_ms`/`write_ms` 比例判断值不值得**，如果 `write_ms` 占比已经压到个位数百分比，流水线化这点收益不值得引入 channel 背压/错误传播的复杂度。本轮不建议排期，留作"如果 profile 数据显示 write_ms 占比意外高"时的备选项。

## 7. 桌面索引交互体验设计（2026-07-28）

### 7.1 现状

索引进度的数据源是 [`IndexStatus`](../apps/desktop/src-tauri/src/search/index_status.rs)（`indexing`/`current_phase`/`current_root`/`fts_progress`/`semantic_indexing`/`semantic_progress`/`db_totals` 等字段），桌面前端（[IndexingPane.tsx](../apps/desktop/src/components/preferences/IndexingPane.tsx)、[FirstIndexStep.tsx](../apps/desktop/src/components/onboarding/FirstIndexStep.tsx)）用 `setInterval` **每 1.5 秒轮询** `get_index_status` 命令渲染。现状能看到的信息：

- 当前阶段 chip（`music_discovery`/`music_scan`/`doc`/`image` 四选一 + 文案映射）
- 当前文件的**父目录**（不是文件名）
- `fts_progress = (scanned, indexed)`：跨 root **累计不清零**的计数器，cycle 7-a 已经明确决定"不做百分比"（`scanned` 本身还在增长，做百分比会失真），只显示两个裸数字
- 语义嵌入进度 `(done, total)`：这个反而有真实分母（`embed_pending` 一开始就知道 `pending` 总数），能算百分比
- 完成后的静态摘要（文档/图片/音频计数）+ 折叠的"未能索引的文件"清单

**关键短板**（对照用户诉求"展示更多信息、实时展示进展"逐条列出）：

1. **没有真实分母、没有 ETA**——`fts_progress` 的 `scanned` 是"边扫边涨的计数器"而非"总数"，这是当前架构下 FTS 阶段没有百分比/剩余时间估算的根因。但 T5 落地的发现层（`PathDiscovery`）**已经解决了这个前提**：Everything/Spotlight/`WalkDir` 发现阶段本来就要先枚举出完整路径列表才能进入提取循环，也就是说"这一轮总共有多少个文件待处理"这个数字在提取开始前就已经算出来了（`to_extract.len()` 或发现层返回的路径数组长度），只是没有被写回 `IndexStatus`。这是**本节最值得做的一项**——把"发现阶段完成、已知总数"这个时机点暴露成一个新字段，FTS 阶段就能从"只增不减的计数器"变成"真正的 N/total 百分比 + 基于近期速率的 ETA"，不需要额外的枚举成本（发现层已经在算这个数了）。
2. **阶段粒度粗，看不出"卡在哪个子步骤"**——`current_phase` 只有 4 个值，没有区分"OCR 子进程正在跑第几页/等第几个 PowerShell 冷启动"这类会让用户觉得"卡死了"的场景（尤其大扫描件 PDF，B3/T4 已经优化过并行度，但用户仍然看不到"这份 PDF 正在处理第 23/50 页"）。
3. **没有吞吐量/资源感知信息**——用户不知道"现在同时有几个 OCR 子进程在跑"、"是不是该等等再干别的事"，这是 STATUS.md 里记录过的真实历史故障（v0.9.27 子进程风暴让用户误判"死机"）的同款体验问题，只是这次不是真的卡死，而是**看不见**导致的误判。
4. **T1 阶段耗时埋点数据完全没有暴露给 UI**——[scan.rs:604-618](../packages/indexer/src/scan.rs) 的 `walk_ms`/`extract_ms`/`write_ms`/`recycle_ms` 目前只进 `tracing::info!` 日志（写文件或 stdout），普通用户不会去看日志。这份数据如果透传到 `IndexStatus`，完成后的摘要页可以直接告诉用户"这次索引：发现 12s / 提取 38min / OCR 41min / 写入 2min / 语义嵌入 6min"，对诊断"为什么这次特别慢"（换了台电脑、目录里混进了一批扫描件）价值很高，且**后端数据已经现成，只差一步 plumbing**。
5. **索引进行中无法取消/暂停**——首次索引"几分钟到几十分钟不等"（[FirstIndexStep.tsx:118](../apps/desktop/src/components/onboarding/FirstIndexStep.tsx) 原话），中途想换个目录重来或者机器要临时干别的事，目前没有取消入口，只能等它跑完或杀进程。
6. **轮询而非事件推送**——`setInterval(1500ms)` 在索引量大、`on_file` 回调高频写 `Mutex<IndexStatus>` 时，前端轮询与后端写状态之间没有本质冲突（`Mutex` 争用可忽略），但 1.5s 的延迟意味着"当前目录/阶段"这类值变化时用户最多等 1.5s 才看到，且长期轮询有少量固定开销；改成 Tauri event（`on_file`/`on_phase` 直接 `emit` 到前端）能做到真正实时、且能顺带解决第 2 点的细粒度问题（不需要额外加一次轮询频率）。
7. **索引完成缺少系统级提醒**——首次索引这种"几十分钟"级别的任务，用户大概率会切走做别的事；完成时除非用户主动切回来看设置页，否则不会知道。桌面端已经有 Windows 托盘基础设施（`tray.rs`，2026-07-26 落地）和系统通知能力（Tauri notification 插件），可以直接复用。

### 7.2 设计方案

**后端（`IndexStatus` 结构扩展 + 事件化，[index_status.rs](../apps/desktop/src-tauri/src/search/index_status.rs)）**：

- 新增 `phase_total: Option<u64>`：本阶段（当前 `current_phase`）发现层给出的总路径数，`fts_begin`/`on_phase` 时机写入；发现层不可用回退 `WalkDir` 时，若能在遍历前先收集完整路径列表（`to_extract` 构建完成的时间点）也一并回填，只有"发现层不可用 + 边扫边发现"的极端兜底路径才继续没有分母（保留旧的"只显数字"展示作为该场景的降级）。有了它，`fts_progress.0 / phase_total` 就是真百分比。
- 新增 `phase_rate_per_min: Option<f64>`：滑动窗口（如最近 15 秒）算的处理速率，配合 `phase_total` 可以估算 ETA（`(phase_total - scanned) / rate`）；实现上在 `StatusProgressBridge::on_file` 里维护一个 `(Instant, count)` 环形缓冲即可，不需要额外定时器。
- 新增 `active_subprocess_count: Option<u32>`（AtomicU32，OCR/pdftoppm 子进程池可以在 spawn/exit 时 +1/-1）：给"资源感知"用，UI 可以显示"⏳ 正在处理扫描件（3 个 OCR 进程并行）"这类文案，替代用户自己猜"为什么风扇转"。
- 新增 `last_run_stage_ms: Option<StageTimings { walk_ms, extract_ms, write_ms, recycle_ms, embed_ms }>`：`scan.rs`/`doc_db.rs`/daemon 已经算出这些数字（§6 前置条件已满足），只需要把它们从 `tracing::info!` 顺带写一份到 `IndexStatus`（或者一个专门的 `Arc<Mutex<Option<StageTimings>>>`），完成后的摘要卡片直接渲染。
- `current_phase` 结构在 §6.1 若落地跨轮并行后需要从"单值"改成 `HashMap<Phase, PhaseProgress>` 或三个并列字段；**若 §6.1 本轮不落地，`current_phase` 单值语义不用改**，两者可以独立排期（UX 设计先按现状"仍是顺序三轮"来做，为并行预留字段扩展空间即可，不必反向阻塞）。
- `on_file`/`on_phase`/`on_batch_done` 之外新增一个显式的 Tauri event `emit("index-progress", &status_snapshot)`（节流到比如 200ms 一次，避免每文件一次 IPC 序列化开销），前端订阅事件而非轮询；`get_index_status` 命令保留作为"页面刚打开时拉一次初始状态"的兜底，不再承担实时更新职责。
- 取消支持：`IndexStatus` 加一个 `cancel_requested: Arc<AtomicBool>`，`perform_reindex_for_roots`/`run_incremental_index_with_filter_and_progress` 的主循环（walk 阶段的 chunk 边界、extract 阶段的 chunk 边界）定期检查该标志、命中则提前 `return Ok(...)` 走"部分完成"路径（已写入的 chunk 保留、不回滚——符合现有"提交=可搜索"的不变量，§5.5 T10 讨论过的崩溃安全同款逻辑，取消等价于一次"预期内的中断"）。前端加一个"取消"按钮，仅在 `indexing=true` 时可见。

**前端（`IndexingPane.tsx`/`FirstIndexStep.tsx` 及新增组件）**：

- **索引中的分阶段进度面板**：把当前"单行 phase chip + 两行进度条"升级成一个四段式管道视图（发现 → 提取/FTS → OCR → 语义嵌入），每段用 `phase_total` 驱动真实百分比条（有分母时）或 indeterminate 动画条（无分母的兜底场景，保留 cycle 7-a 已有的 `prefs-progress-indeterminate` 样式，不用推翻重做），当前活跃段高亮，已完成段显示对勾 + 该段实际耗时（用 `last_run_stage_ms` 里对应字段，即便索引还没完全结束，已完成的阶段也能立即显示"发现 12s ✓"）。
- **当前文件行**：从"当前目录"升级成"当前目录 + 文件名 + 类型图标"，命中 OCR/大 PDF 时额外一行"扫描件处理中（第 N/M 页）"（需要 doc_extract.rs 的分页 OCR 循环也打一次细粒度回调，成本已经在 T4 的并行池里顺带能拿到，不是新增大改动）。
- **速率 + ETA 一行**：「约 320 个/分钟 · 预计还需 4 分钟」，`phase_rate_per_min`/`phase_total`/`scanned` 三个字段直接算，无数据时不显示这一行（不编造）。
- **资源感知提示**：`active_subprocess_count > 0` 时显示一行"⚙️ N 个后台识别进程运行中，可能影响电脑响应速度"，把"为什么变慢/风扇转"的疑问提前解释掉，这是从 v0.9.27 真机故障学到的教训（当时用户把子进程风暴误判成死机）直接转化成的产品文案。
- **完成后的耗时明细**：摘要卡片从"文档 320 / 图片 58 / 音频 947"这一行纯计数，扩展成可展开的"本次索引用时明细"（发现/提取/OCR/写入/语义嵌入分段耗时的横向条形图或简单表格），复用 [dataviz skill](../docs) 的极简风格（几个 stat tile + 一条分段条足够，不需要引入图表库）。
- **取消按钮**：索引中状态下，主按钮从"索引中…"（disabled）改为可点的"取消"，点击后调用新命令、状态回落到"部分完成"文案（如"已中断：文档 320 / 音频 89（未完成）"）。
- **系统托盘 + 通知**：Windows 托盘图标索引中显示一个简单的忙碌态叠加（无需精确百分比动画，避免过度工程），索引完成后（尤其后台/首次这种长任务）用 Tauri 的 `notification` 插件弹一条系统通知"Scout 索引完成：共 1,325 条"，点击通知聚焦主窗口；macOS 侧用 Dock 图标 badge 或系统通知中心同款处理，两端复用同一条后端"完成"事件触发点（`fts_finish`/`semantic_done` 已经是天然的触发时机，不用新找）。
- **事件订阅替代轮询**：前端改用 `@tauri-apps/api/event` 的 `listen("index-progress", ...)` 替代 `setInterval`，`FirstIndexStep.tsx`/`IndexingPane.tsx` 两处轮询逻辑统一收敛成一个共享 hook（如 `useIndexStatus()`），避免两处各自维护订阅生命周期。

### 7.3 与 §6 的依赖关系

§7 的 `phase_total`/ETA 设计**不依赖** §6.1（跨阶段并行）落地——即便三轮仍是顺序执行，单轮内部的"发现完→知道总数→显示真百分比"这条链路已经独立成立，可以先做、收益立等可用。若后续做了 §6.1，`IndexStatus` 需要从单阶段扩展成多阶段并列展示，是在 §7 数据结构基础上的增量改动，不是推倒重来。

## 8. 整合任务规划（v1.5）

在 §3.2/§4/§5 已完成任务（P0 全部、P1 T5/T6、P1.5 T7a/T6b、P2 T7/T8/T9a）之上，本轮新增两条独立轨道：

**P3（架构性，收益不确定，需先 profile 再决定是否投入）**

| 任务 | 内容 | 前置条件 |
|---|---|---|
| T11 | 文档/图片/音频三轮 FTS 阶段间并行（§6.1），配套设计跨轮共享子进程信号量防打爆机器 | 先用现有 T1 埋点拿三轮真实耗时占比，判断值不值得扛这份并发控制复杂度 |
| T12 | macOS 图片 OCR：Tesseract 批量调用模式 / 独立并发预算（§6.2 短中期部分） | 无强前置，可独立排期 |
| T13 | macOS 原生 Vision OCR helper 二进制（§6.2 长期部分） | 收益最大但改动面最大，建议单独立项评估，不与 T11/T12 打包 |
| T14 | Extract/Write chunk 流水线化（§6.3） | 先看 T1 埋点里 `write_ms` 占比，若已很低则不值得做 |

**P-UX（桌面索引交互体验，§7，与 P3 相对独立，可并行推进）**

| 任务 | 内容 | 收益 |
|---|---|---|
| U1 | `IndexStatus` 扩展 `phase_total`/`phase_rate_per_min`/`last_run_stage_ms`/`active_subprocess_count`，发现层完成时回填分母 | 是后续所有 UI 改进的数据前提，优先做 |
| U2 | Tauri event 化（`index-progress` emit）替代前端 1.5s 轮询，`FirstIndexStep`/`IndexingPane` 收敛成共享 hook | 实时性 + 代码去重 |
| U3 | 前端分阶段管道视图 + 真百分比/ETA + 当前文件行 + 资源感知提示 | 直接回应用户"展示更多信息、实时展示进展"的诉求 |
| U4 | 索引取消支持（后端 `cancel_requested` 标志 + 前端取消按钮） | 长任务可控性 |
| U5 | 完成后耗时明细展开 + 系统通知/托盘完成态 | 诊断价值 + 长任务不需要用户守着 |

**建议顺序**：U1 → U2 → U3 是一条强依赖链，先做；U4/U5 可在 U3 之后并行插入。P3 的 T11-T14 都建议先拿 profile 数据再决定，不要凭代码走读直接开工（延续 §3.1 设计原则"先测量、后优化"）。P3 与 P-UX 两条轨道彼此独立，可以交替排期，不必等一条轨道完全收尾再开始另一条。

## 9. 执行顺序调整：文档优先（2026-07-28）

用户反馈：Scout 面向工作场景，**文档**文件量最大、最先被搜索，**图片**次之，**音频**（原文一直称"音乐"，本轮同步改称"音频"，见下）文件量通常最少、优先级最低——此前 §1-8 沿用的"音乐 → 文档 → 图片"执行顺序与这个优先级不符，音频轮排在最前反而挡住了文档尽快可搜。已落地，不再是待办：

- **执行顺序**：`apps/daemon/src/main.rs`（`run_initial_collection_index`）、[packages/scout-server/src/reindex.rs](../packages/scout-server/src/reindex.rs)（`run_collection_reindex`）、[packages/search-backends/local-index/src/lib.rs](../packages/search-backends/local-index/src/lib.rs) 的三个生产 reindex 入口（`reindex_with_filter_and_progress_inner`/`reindex_with_progress_inner`/`reindex_with`，桌面端实际走的路径）均改为**文档 → 图片 → 音频**顺序执行 + `on_phase` 通知顺序。返回的 `(IndexStats, IndexStats, IndexStats)` 元组形状/位置未变（仍是 music/doc/image 三个位置），只改了**计算顺序**，不改元组结构——避免牵动所有下游解构调用点，改动面收窄到"谁先跑"而非"数据怎么传"。
- **"媒体"分类不采用**：讨论中曾考虑把音频和未来可能的视频合并成"媒体"一类展示/排序，用户明确表示视频暂不实现、"媒体"分类先不用——本节及 §6-8 涉及该分组的表述已改回单指"音频"，不再提"媒体"。**视频文件目前在 Scout 里没有任何内容索引能力**（无提取、无 metadata、无缩略图），这是一个独立于本次改动的功能缺口，不在本轮范围内。
- **"音乐"改称"音频"**：用户反馈"音乐"这个词不如"音频"准确（内容不止音乐，也可能是播客/录音等）。已改的范围——**仅用户可见文案**：桌面端前端全部展示文案（索引概貌统计、进度阶段 chip、设置项说明、清空索引提示等）、后端产出的用户可见摘要/错误文本（如 `IndexStatus.last_summary`、`purge_root_from_db` 错误信息）、对应测试里断言这些文案的字符串常量，以及本文档 §6-9 的正文表述。**未改的范围**：Rust 内部类型名（`MusicIndex`/`MusicEntry`）、数据库表名（`music`/`music_fts`）、字段名（`music_count`/`music_added`/`music_stats` 等）、`IndexPhase::MusicDiscovery`/`MusicScan` 枚举变体、`packages/indexer`/`packages/scout-server` 内部大量既有的"音乐"字样架构注释（§1-5 记录的历史设计过程同样不动）——这类改名涉及数据库 schema、跨全仓库标识符，属于单独的、高风险的工程决策（需要给已有本地库设计迁移/兼容路径），本轮不做，用户已确认现阶段不需要。
- **验证**：`cargo check`/`clippy -D warnings`/`fmt --check` 在改动的 4 个 crate（`scoutd`/`scout-server`/`scout-indexer`/`scout-local-index-backend`）全绿；`cargo test` 337 个测试通过（`scout-indexer` 210 + `scout-local-index-backend` 30 + `scout-server` 97 + `scoutd` 单测 8）；`scoutd` e2e 3 个失败是既有的本机 macOS 沙盒 `/var` vs `/private/var` 路径问题（同 ROADMAP BETA-63/BETA-65 记录的根因），与本次改动无关。前端 TSX/TS 文案改动未跑 `tsc`/`vite build`/浏览器走查（本轮未启动桌面开发环境），下次真机验证时一并覆盖。

## 10. 落地 §7/§8 的 U1 + U3 核心部分（2026-07-28 续轮）

用户要求"按设计文档启动新一轮性能优化"。§8 把后续工作分 P3（架构改动，卡在"先测量"前提）和 P-UX（桌面体验，文档自己建议"U1 → U2 → U3 先做，不需要真机 profile 前提"）——选了 P-UX 的 U1 + U3 核心部分（真百分比/ETA + 完成后耗时明细），跳过 U2（event 推送）/U4（取消）/U5（通知/托盘）留后续，P3 继续等真机数据。

**本轮过程中发现的一个比"没有真百分比"更基础的缺口**：走读 `index_discovered_paths`（Document 的 `index_paths`/`index_image_paths` 共用骨架、T5 落地后已是桌面端主路径——Everything/Spotlight 发现成功时走这条）和 `MusicIndex::index_paths` 发现，这两个函数**完全不接受 `progress` 参数、完全不调用 `on_file`**。也就是说，真机装了 Everything/Spotlight 的常见场景下，`IndexStatus.fts_progress` 这个计数器实际上纹丝不动，只有发现层不可用回退 `WalkDir` 时才会动——用户"实时展示进展"的诉求在这条最常见的路径上此前根本没被满足。本轮把这个缺口和真百分比一起修了。

### 10.1 后端：`IndexProgress` trait 新增两个回调（`packages/indexer/src/progress.rs`）

```rust
fn on_scope_known(&self, _total: u64) {}              // 本 phase 总文件数已知
fn on_stage_timings(&self, _timings: StageTimings) {}  // walk/extract/write/recycle 耗时
```

默认 no-op（沿用 `on_phase` 的引入模式），daemon 走 `NoopProgress`/`tracing` 零行为变更。`StageTimings{walk_ms, extract_ms, write_ms, recycle_ms}` 从 `lib.rs` 顶层 re-export。

### 10.2 `packages/indexer/src/scan.rs`：三处接入点

1. `run_incremental_index_with_filter_and_progress`（`WalkDir` 路径，Music/Document 共用泛型骨架）：walk 循环结束、`stats.scanned` 定型后调 `on_scope_known`；耗时日志旁调 `on_stage_timings`。
2. `index_discovered_paths`（发现层共用骨架）：新增 `progress` 参数——skip 分支内联 `on_file`、`extract_chunk` 的 `par_iter` 内每条结果后 `on_file`（照抄 `WalkDir` 路径写法），预检循环结束后 `on_scope_known`，耗时日志旁 `on_stage_timings`（`recycle_ms` 恒 0——本函数不做回收）。4 个公开 wrapper（`index_paths`/`index_paths_with_known_mtimes`/`index_image_paths`/`index_image_paths_with_known_mtimes`）透传 `progress`。
3. `MusicIndex::index_paths`（独立实现，不走 `index_discovered_paths`）：同款补齐——之前完全没有任何耗时埋点（不只是没有 progress），本轮顺带加了 `Instant` 计时。

新增测试：`document_index_dirs_with_progress_reports_scope_and_stage_timings`、`index_paths_reports_scope_known_and_on_file_via_spy`、`document_index_paths_with_known_mtimes_reports_scope_known_and_on_file_via_spy`——最后两个专门锁定"发现层此前不报进度"这个回归点。

### 10.3 调用点接线（`packages/search-backends/local-index/src/lib.rs`）

三个生产函数各自的 3 处 `index_paths`/`index_paths_with_known_mtimes`/`index_image_paths_with_known_mtimes` 调用加 `progress` 实参：`reindex_with_filter_and_progress_inner`/`reindex_with_progress_inner`（真实 progress 直接透传）传自己收到的 `progress`；`reindex_with`（测试注入用的无 progress 语义骨架 API，桌面不走）传 `&NoopProgress`。

### 10.4 桌面状态桥（`apps/desktop/src-tauri/src/search/index_status.rs`）

`IndexStatus` 新增：

- `phase_total: Option<u64>` —— 当前 phase 总数。
- `phase_scanned: Option<u64>` —— **phase 内**计数，`on_scope_known` 时归零、每次 `on_file` +1。**与既有 `fts_progress.0`（跨全轮所有 phase 累计、不因 phase 切换清零）是两个不同的计数器**——`fts_progress` 保留给"本轮到目前为止总共处理了多少"这个既有心智模型，`phase_scanned` 专供配 `phase_total` 算百分比用，语义上不能混用（`fts_progress.0` 在第二个 phase 开始时不会归零，用它除 `phase_total` 会得出错误的百分比）。
- `phase_rate_per_min: Option<f64>` —— 累计平均速率（`phase_scanned` / 已过去分钟数），非滑动窗口。起始时间戳存在 `StatusProgressBridge` 私有字段（`Instant` 不可序列化，不进 `IndexStatus`）。
- `last_run_stage_ms: RunStageTimings`（`{doc, image, music}` 三槽位）—— `on_stage_timings` 按当时 `current_phase` 落到对应槽位；`fts_begin`/`fts_finish` 都不清这个字段，索引空闲后 UI 仍能展示"上次索引用时明细"。

新增测试：`scope_known_and_on_file_track_phase_scanned_independent_of_fts_progress`（锁定 phase_scanned 与 fts_progress 两个计数器不互相干扰）、`stage_timings_land_in_correct_slot_and_survive_fts_finish`。

### 10.5 前端（`shared.ts`/`IndexingPane.tsx`/`FirstIndexStep.tsx`）

`shared.ts` 新增 `StageTimings`/`RunStageTimings` 类型 + `phaseProgressText(total, scanned, ratePerMin)` 纯函数（算百分比 + "约 N 个/分钟 · 预计还需 M 分钟"文案，任一输入缺失返回 `null`，调用方退回裸数字展示、不编造）。

`IndexingPane.tsx` 管道卡片：`phaseProgressText` 有返回值时渲染真百分比条 + 速率/ETA，`null` 时保留原有"已扫描 X · 已入库 Y"裸数字（walk 还没扫完等极端 fallback 场景）。新增"本次索引用时明细"可展开区块（读 `last_run_stage_ms`，文档/图片/音频各一行 walk/extract/write/recycle + 合计），旧库从未产生过这个字段时不渲染整块。

`FirstIndexStep.tsx`（onboarding）复用同一套字段和 `phaseProgressText`。**顺带修正一个既有 bug**：这里此前的"FTS 进度百分比"是拿 `ftsIndexed / ftsScanned` 算的——语义其实是"扫描到的文件里有多少是新增/变更"，不是"整体进度百分比"，跟 `IndexingPane.tsx` 当初"不做百分比"的决定（cycle 7-a）实际上是同一个顾虑，只是 `FirstIndexStep.tsx` 没有同步改，这次一并用真百分比替换掉。

### 10.6 验证

`cargo check`/`clippy -D warnings`/`fmt --check` 在 5 个改动 crate（`scout-indexer`/`scout-local-index-backend`/`scoutd`/`scout-server`/`scout-desktop`）全绿；`cargo test` 对应 5 个 crate 全部通过（`scout-desktop` 185、`scout-indexer` 217，含本轮新增测试）；`scoutd` e2e 3 个失败仍是同一个已知的本机沙盒问题，与本轮无关。前端 `tsc --noEmit` 与 `npm run build`（`vite build`）均通过；起了一次 `scout-desktop-web` 预览（`http://localhost:5180`，浏览器直接跑、无 Tauri IPC），确认应用壳层与"设置"弹窗能正常渲染、控制台里除了预期的"没有 Tauri 后端"报错（`Cannot read properties of undefined (reading 'invoke')`，`PreferencesDialog`/`StatusIndicator` 等既有代码本来就有的模式，与本轮改动无关）之外没有新增报错——但设置弹窗的数据加载卡在 `invoke` 失败上，没能实际点进"索引" tab 看到新 UI 的真实渲染效果，这部分仍需下次接了真实 Tauri 后端时补做真机走查。

### 10.7 待办（本轮不做，留后续）

- **U2**：Tauri event 推送替代前端 1.5s 轮询。
- **U4**：索引取消支持。
- **U5**：完成后系统通知 + 托盘完成态。
- **P3（T11-T14）**：跨阶段并行、macOS OCR 改造、extract/write 流水线化——仍在等真机 `last_run_stage_ms` 数据积累后再决定是否投入，本轮新增的埋点正是为了产出这份数据。

## 11. 真机复盘：14570 文档索引耗时 25 小时（约 10 个/分钟）（2026-07-29）

用户真机测试 v0.9.43：扫描一个 14570 个文档的目录，构建耗时 25 小时（约 10 个/分钟）——比触发本项目整个优化工作的原始投诉（约 1 万文件 1 小时以上，折算约 166 个/分钟）还慢 15-17 倍。排查过程与结论：

### 11.1 用户提供的两条关键线索

1. **`spawn_semantic_index: 默认禁用`**——检查代码确认这不是异常，是 v0.8.5（BETA-31-v3 cycle 4）就存在的安全开关：`apps/desktop/src-tauri/src/search/index_status.rs` 的 `spawn_semantic_index` 默认直接 `return`，除非设了环境变量 `SCOUT_ENABLE_EMBED=1`，原因是当时 embedding 真跑到真文档会触发 llama-cpp native crash（`ucrtbase.dll` 0xc0000409，整进程被杀、`catch_unwind` 兜不住）。**这个开关至今没有被移除或重新评估**——即便 2026-07-25 的 T9a（context 常驻复用）已经在真机 Metal 上验证过一次相关修复，那次验证的是 T9a 自己要解决的 KV cache 泄漏问题，**不能确认就是 v0.8.5 这次崩溃的根因**，也从未在 Windows 上针对原始崩溃场景重新测试过。结论：**用户这次测的"关键词+语义"其实只有关键词在跑**，语义嵌入全程没有执行——这解释了为什么日志里看不到语义相关的耗时数据，但不是本次慢的原因（本来就没跑，谈不上慢）。是否重新评估默认开启，因为有真实崩溃历史、风险由用户自行承担，本轮未处理，留待用户决定是否要专门测试。
2. **"删除整个应用数据目录（含旧 `index.db`）重装后，速度从约 10 个/分钟提升到约 80 个/分钟"**——同一份代码、同一批文件，只换了一个全新空库，吞吐提升 8 倍。这把"变慢"的原因锁定在**旧数据库的累积状态**上，不是本轮流水线逻辑本身的 bug（14570 文档里大部分是普通文本/Office 文档，用户确认，排除了"这批文件恰好都是扫描版 PDF 因而慢"这个可能性）。

### 11.2 代码走读确认的两个具体机制

1. **批量预取查询是全表扫描，代价随全库累计量增长，不随本轮实际处理量增长**：`paths_under_impl`/`modified_times_under_impl`/`failure_paths_under_impl`（[doc_db.rs:692-753](../packages/indexer/src/doc_db.rs)）都是 `SELECT path[, ...] FROM documents`（或 `index_failures`）不带 `WHERE`，root 过滤在 Rust 侧对拉回内存的全部行做字符串前缀匹配。这是 T7a 刻意的设计取舍（注释原文："一次全表扫描换掉…逐文件…往返"），对"DB 总量 ≈ 本次索引量"的场景没问题，但对"DB 里还留着很多历史目录/历史版本测试数据"的场景，每次索引开始前都要多付出与历史累积量成正比的一次性代价。**这是一次性开销**（每个 phase 一次，不是每文件一次），单独不足以解释"全程吞吐慢 8 倍"，但会拉长小库场景下不明显的启动延迟。
2. **FTS5 索引从未做过 optimize/VACUUM 之外的整理，长期反复增删会累积碎片，直接拖慢后续每一次写入**（找到的更可能的主因）：全仓库搜索确认 `VACUUM`/FTS5 `optimize` 只在"一键清空索引"（[db.rs:61-81](../packages/indexer/src/db.rs)，`clear_index`，DROP 全部表 + VACUUM）这一条路径上出现过，日常的增量索引/reindex 生命周期里完全没有任何整理动作。`documents_fts`/`music_fts` 用的 trigram 分词器（CJK 场景的刚需），FTS5 官方文档明确说明反复 INSERT/DELETE 会让内部 segment b-tree 碎片化、写入随之变慢，需要靠 `optimize` 命令定期合并 segment。一台跑过 v0.9.27 到 v0.9.43 之间几十个版本真机测试的开发机，`index.db` 大概率经历过大量"索引→删除→改配置→再索引"的循环，这正是 FTS5 最怕的使用模式——直接换一个从未写过的全新库，前述碎片化历史清零，写入自然快回去，与用户观察到的"8 倍"现象吻合度最高。

### 11.3 已落地的两个低风险增量修复

1. **`MusicIndex::optimize_fts`/`DocumentIndex::optimize_fts`**（[db.rs](../packages/indexer/src/db.rs)/[doc_db.rs](../packages/indexer/src/doc_db.rs)）：执行 `INSERT INTO {table}(...) VALUES('optimize')`，FTS5 官方支持的轻量整理命令（不像 `VACUUM` 需要独占锁 + 整库拷贝）。`packages/search-backends/local-index/src/lib.rs` 新增 `optimize_fts_if_changed` 辅助函数，在三条生产 reindex 入口收尾时调用——只在本轮 `added+updated+removed>0` 时才真的调用，避免空轮（如无变化的定期自动增量扫描）跑无谓 I/O；失败静默忽略（该 crate 无 tracing 依赖，且这是尽力而为的维护操作，不影响本轮已经成功落库的索引结果）。这是**预防性**修复——针对性解决"长期使用的库会不会重新累积同样的碎片化"，不是本次这一个具体案例的事后补救（旧库已被用户删除，补救不到）。
2. **`log_db_size_diagnostics`**（[index_status.rs](../apps/desktop/src-tauri/src/search/index_status.rs)）：`perform_reindex_for_roots` 每轮开始前记一条日志——`index.db` 文件体量 + `documents`/`document_vectors`/`index_failures`/`music` 行数。这次事后诊断完全靠用户回忆"删除重装前后速度差异"这种会丢失证据的办法，下次再出现类似情况，能直接从日志里读到"是不是撞上大库/历史累积"这个方向，不需要再靠重装来验证假设。

### 11.4 验证

`packages/indexer` 新增 4 个单测（`optimize_fts` 在空库/有数据库上均安全、调用后查询仍正常命中）；`scout-desktop` 新增 1 个单测（`log_db_size_diagnostics` 在库不存在/真实库上均不 panic）；`local-index-backend` 既有 30 个测试覆盖三条生产入口，新增的 `optimize_fts_if_changed` 调用点随之被完整跑过、无回归。5 个改动 crate `cargo check`/`clippy -D warnings`/`fmt --check` 全绿，`scoutd` e2e 3 个失败仍是既有本机沙盒问题、与本次改动无关。**未验证**：`optimize_fts` 对已经严重碎片化的真实大库能带来多少实际提速——本地没有这样的库可复现，需要真机场景再次出现类似"慢"的报告时，用新加的诊断日志确认修复是否生效。

### 11.5 未决问题（留用户决定）

- `SCOUT_ENABLE_EMBED` 默认禁用是否应该重新评估——现状是**语义索引在生产默认配置下完全不工作**，这是一个比本次性能排查更大的产品缺口，但直接改默认值有真实的历史崩溃风险，不应在没有专门验证的情况下顺手改掉。
