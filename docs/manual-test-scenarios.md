# Scout 手动测试场景速查

> 用途:在桌面 App / `npm run tauri dev` 里手动验证各项能力的自然语言查询速查表。
> 直接把「提示词」复制到搜索框即可。维护者:任一协作工具,随能力增加更新。

## 通用坑点

1. **Spotlight 偶发 `io error:`** —— mdfind 后端间歇性失败,重试该查询即可(环境问题,非功能缺陷)。
2. **copy / move / rename 会真改文件** —— 先用桌面副本之类的安全文件练手;不要拿重要文件试。
3. **文件操作需先搜索** —— `打开第N个` / `复制` 等都引用「上一轮搜索结果」,必须先做一次搜索。
4. **delete 全程禁用** —— schema + Policy 双重硬禁用,任何删除请求都会被拒。

---

## A. 基础文件搜索(FileSearch)

| 提示词 | 预期 |
|---|---|
| `find pdf` / `查找 pdf` | 列出 pdf,intent=file_search · via search.spotlight |
| `查找包含 报告 的文档` | 关键词 "报告" + file_type 文档 |
| `find files containing invoice` | 关键词 invoice |
| `找 ppt` / `查找 keynote` | 按扩展名 / 类型 |
| `找叫 example 的文件` | 关键词 example |

## B. 时间 / 大小 / 排序

| 提示词 | 预期 |
|---|---|
| `查找昨天编辑过的 ppt` | 时间过滤(modified)|
| `找最近一周的 pdf` | 近 7 天 |
| `find files modified this month` | 本月 |
| `找最大的 10 个视频` | size 排序 + limit |
| `查找大于 100MB 的文件` | size 过滤 |
| `最新的 5 个文档` | sort=newest + limit 5 |

## C. 位置 / 目录

| 提示词 | 预期 |
|---|---|
| `找下载目录里的 pdf` | location=下载 |
| `查找桌面上的图片` | location=桌面 |
| `find pdf in documents` | location=文稿 |
| `截图文件夹里的 png` | location=截屏目录 |

## D. 媒体搜索(MediaSearch)

| 提示词 | 预期 |
|---|---|
| `找截图` / `find screenshots` | media_type=screenshot |
| `找最近一周截的图` | screenshot + 时间 |
| `查找 jpg 图片` | image + 扩展名 |
| `找周杰伦的歌` | audio + artist=周杰伦 |
| `查找时长超过 3 分钟的视频` | video + duration |
| `find mp4 videos` | video + 扩展名 |

## E. 多轮细化(Refine —— 在一次搜索之后追加)

> 先 `find pdf`,再输入下面任一条,会**合并到上一轮**继续收窄(链式叠加,渐进收窄)。

| 提示词 | 预期 |
|---|---|
| `只看 png` | 把上一轮结果类型改为 png |
| `只看下载目录` | 给上一轮加 location=下载 |
| `只要最近三天的` | 给上一轮加时间过滤 |
| `只看大于 1MB 的` | 加 size 过滤 |

## F. 打开 / 定位(open / locate —— 引用上一轮第 N 个,立即执行无需确认)

> 先做一次搜索拿到结果列表,再:

| 提示词 | 预期 |
|---|---|
| `打开第1个` / `打开第二个` | 用默认应用打开第 N 个,UI「已打开 …」|
| `open the 2nd one` / `open the second` | 同上(英文)|
| `在访达里显示第3个` | Finder 高亮第 3 个 |
| `show in finder the 1st` / `reveal the first` | 同上(英文)|
| `打开这些` / `open all of them` | 打开全部(All selector)|
| `打开第99个`(越界)| 友好错误「第 99 个结果不存在(上一轮共 N 条)」|

## G. 复制 / 移动 / 重命名(确认流 —— 会真改文件,带确认对话框)

> 先搜索,再输入。会弹**确认对话框**,点「确认」才执行,点「取消」不动文件。**copy/move 支持多目标**(`把这些…`,上限 10 个),**rename 限单目标**。

| 提示词 | 预期 | 坑点 |
|---|---|---|
| `把第1个复制到桌面` | 确认框「复制 X 到 …/Desktop?」→ 确认后复制 | 介词连写 `复制到` |
| `把第2个移动到下载` | 确认框「移动 X 到 …/Downloads?」 | **必须** `把第N个移动到X`,不能写「移动第N个到X」(序数插中间不匹配 `移动到`)|
| `把第1个重命名为 myfile` | 确认框「重命名 X 为 myfile?」 | 用 `重命名为` / `改名为` / `改成` |
| `copy the 1st to desktop` | 英文 copy 确认 | 连写 `copy to` |
| `rename the 1st to newname` | 英文 rename 确认 | |
| `把这些复制到桌面`(多目标,先搜 2–10 条)| 确认框「**复制 N 个文件到 …/Desktop?**」→ 确认后复制 N 个 | 多目标触发词只有 `这些`/`these`/`all of them`(=上一轮全部);`前面两个` 这种取前 N 不支持 |
| `把这些移动到下载`(多目标)| 确认框「移动 N 个文件到 …/Downloads?」 | move 会真移走源 |
| `把这些重命名为 X`(多目标)| 友好错误「一次只能重命名单个文件(多文件待后续)」 | rename 维持单目标 |
| `把这些复制到桌面`(上一轮 >10 条)| 友好错误「目标过多(最多 10 个),请缩小范围」 | batch 上限保护 |
| `把第1个复制到桌面` → 点**取消** | 对话框消失,文件**不动** | 验证取消路径 |

**安全验证点**:点确认前文件不该有任何变化;点取消后文件系统零改动;多目标若有同名碰撞(不同目录同名文件 join 到同一落点)整体中止零落盘。支持的目标目录:桌面 / 下载 / 文稿 / 图片(`~/Desktop`、`~/Downloads`、`~/Documents`、`~/Pictures`)。

## H. 澄清 / 边界(Clarify & 错误 UX)

| 提示词 | 预期 |
|---|---|
| `找最近的` | Clarify(时间不明)→ 错误态 / 澄清提示 |
| (重启 App 后首次)`打开第1个` | 友好错误「没有可操作的上一轮搜索结果,请先发起一次搜索」|
| 首次直接 `只看 png`(无上一轮)| 友好错误「没有可细化的上一轮搜索」|
| `删除第1个` | 「删除操作不支持」(delete 硬禁用)|

---

## 推荐冒烟顺序(5 分钟跑核心能力)

1. `find pdf` → 看结果
2. `打开第1个` → 真打开(F)
3. 重新 `find pdf` → `只看下载目录` → 链式细化(E,基于上一轮)
4. 重新 `find pdf` → `把第1个复制到桌面` → 确认 → 看桌面(G)
5. `把第1个复制到桌面` → **取消** → 确认文件没动
6. `把这些复制到桌面` → 看多目标友好错误

---

## 观测(可选:开 trace)

启动时带环境变量可把工具层事件写成 JSONL,便于核对:

```bash
SCOUT_TRACE=/tmp/scout-trace.jsonl npm run tauri dev
```

- 搜索成功:`tool_call(search.spotlight)` + `tool_result(N)`
- open/locate 执行:`tool_call(file_action.local)` + `tool_result(1)`
- copy/move/rename 首次下发(弹确认框):**不进 trace**(pre-tool);点确认后才有 `tool_call(file_action.local)` + `tool_result`
- 取消 / 澄清 / 越界等 pre-tool 失败:**不进 trace**
- **同义词扩展命中** (BETA-11):`synonym_expand`(每组非 singleton 一条,含 `head` / `group` / `source` / `truncated`),发于 `tool_call` 之前

---

## BETA-11:同义词关键词扩展

> 关联:[spec](./superpowers/specs/2026-05-30-synonym-keyword-expansion-design.md) / [plan](./superpowers/plans/2026-05-30-synonym-keyword-expansion.md)

### 准备 fixture

在 `~/Desktop/scout-beta11-fixtures/` 下创建以下文件(可用 `touch` + 简单内容):

```bash
mkdir -p ~/Desktop/scout-beta11-fixtures
cd ~/Desktop/scout-beta11-fixtures
touch 述职.ppt 屏幕截图_2024-01-01.png foo.pptx 购房协议.pdf bar.jpg synthetic-place-note.md
# 让 Spotlight 重新索引这个目录
mdimport ~/Desktop/scout-beta11-fixtures/
```

> 等 30~60 秒,确认 `mdfind -onlyin ~/Desktop/scout-beta11-fixtures kMDItemFSName=述职.ppt` 能命中。

### 词典准备

仓内已 ship 词典:`resources/synonyms/zh.yaml` + `en.yaml`(BETA-11 Task 10 落地)。dev 模式自动从 workspace 根读;打包 `.app` 后从 `Contents/Resources/synonyms/` 读。

关键 demo 组:
- zh:`工作汇报 → [述职, 年度总结, 季度汇报, 月度汇报]`
- zh:`截图 → [截屏, 屏幕截图]`
- zh:`合同 → [协议, ...]`
- en:`slides → [slideshow, pptx]`(需查 en.yaml 实际 alias)
- en:`photo → [picture, pic]`

### Scenario 清单

| # | 自然语言 query | 期望 | 验证什么 |
|---|---|---|---|
| 1 | `找一份工作汇报相关的ppt` | 命中 `述职.ppt` | **用户原 case**;zh office 桶 |
| 2 | `找最近的截图` | 命中 `屏幕截图_2024-01-01.png` | zh media 桶 |
| 3 | `find a slideshow` | 命中 `foo.pptx`(若 en.yaml 含 `slides ↔ pptx`)| en file_type 桶 |
| 4 | `找合同` | 命中 `购房协议.pdf` | zh document 桶(协议) |
| 5 | `find a photo` | 命中 `bar.jpg`(若文件名含 picture/pic alias)| en media 桶 |
| 6 | `找 synthetic-place 的笔记` | 命中 `synthetic-place-note.md`(精确)| hyphenated 标识符**不被误扩**(classify → Skip)|
| 7 | `SCOUT_TRACE=/tmp/b11.jsonl npm run tauri dev` + 跑 #1 | `/tmp/b11.jsonl` 含 `"tag":"synonym_expand"` 一行 | 可解释 trace 接通 |
| 8 | 把 `resources/synonyms/zh.yaml` 临时改名 + 重启 dev + 跑 #1 | dev stderr 出 `synonym: 词典加载失败,退到 noop` + #1 退化为不命中"述职" | `NoopExpander` fallback 路径 |

### 观测要点

- scenario 1-6:正常 UI 看结果列表。
- scenario 7:`cat /tmp/b11.jsonl` 看 `tag` 字段:期望出现 `synonym_expand`(head=工作汇报、group=[工作汇报, 述职, ...]、source=zh.yaml、truncated=false)接着 `tool_call(search.spotlight)` 与 `tool_result(N)`。
- scenario 8:dev stderr 第一行就该出 fallback warn。改完记得把 zh.yaml 改回去。

### 退出条件

scenario 1-8 全过(尤其 #1 用户原 case 必过)即 BETA-11 真机端到端验收通过。

---

## BETA-15D:macOS 26 谓词回归修复

macOS 26 拒绝 keyword+比较约束复合谓词（`kMDItemTextContent CONTAINS … && kMDItemFSName == …`），导致 `Failed to create query`。已修：Spotlight 后端改为双查询 Q1（glob 文件名）/ Q2（content keyword），PostFilter 在 Rust 端完成扩展名/大小/时间约束。

> 环境：`SCOUT_TRACE=/tmp/b15d.jsonl npm run tauri dev`
> fixture：`~/Desktop/scout-beta11-fixtures/述职.ppt` 已存在（见 BETA-11 节）

> ✅ **BETA-15E 已修复（2026-05-30）**：此前 parser 对自然中文 query 不产 keyword（`找一份工作汇报相关的ppt` → `keywords=None`），使下表 scenario 1/4 退化为扩展名/match-all、无法验证复合修复。BETA-15E 在 harness 层加词典 gazetteer：parser 无 keyword 时扫 query 命中词典内容词（如"工作汇报"）即注入 keyword group 触发同义词扩展。**现 scenario 1（`找一份工作汇报相关的ppt`）经 gazetteer → 工作汇报组 → 扩展 Q1 复合谓词 → 命中 述职.ppt**，自然语言 demo 解锁。注：类型/媒体词（幻灯片/截图等）被重解析守护跳过、不会误注入。

| # | 自然语言 query | 期望 | 验证什么 |
|---|---|---|---|
| 1 | `找一份工作汇报相关的ppt` | 命中 `述职.ppt` | **BETA-15 原始 case**；同义词扩展 + keyword+扩展名复合，端到端必过 |
| 2 | `找最近一周的 pdf` | 有结果且无 `Failed to create query` | keyword+时间复合谓词，macOS 26 回归已修 |
| 3 | `找大于 1MB 的 ppt` | 有结果且无 `Failed to create query` | keyword+大小复合谓词，PostFilter Rust 端约束 |
| 4 | `述职` | 命中 `述职.ppt` | 纯 keyword CJK 文件名子串匹配（旧 CONTAINS 路径命中不到） |
| 5 | `find pdf` | 有结果，与 BETA-11 前行为一致 | 纯扩展名回归，双查询路径不破 |

### 观测要点

- scenario 1–5：`cat /tmp/b15d.jsonl` 确认无 `"tag":"error"` 含 `Failed to create query`。
- scenario 1 trace：应见 `synonym_expand`（工作汇报→述职）+ `tool_call(search.spotlight)` + `tool_result(≥1)`。
- scenario 2–3 trace：`predicate(Q1)` 仅含 glob，`predicate(Q2)`（若 trace 可见）含 keyword CONTAINS，大小/时间约束由 PostFilter 在 Rust 端执行，不出现在 mdfind 谓词中。

### 退出条件

scenario 1–5 全过（尤其 #1/#4 必过），且 trace 中无 `Failed to create query` 错误，即 BETA-15D 真机端到端验收通过。

---

## BETA-04 多源融合（Result Normalizer + LocalIndexBackend）

> 前置：`npm run tauri dev` 起窗。**先在「设置 → 本地索引 → 立即索引」点一次**（或确保
> 系统音乐/文档目录有内容），让本地索引有数据。索引库在 `<data_dir>/Scout/index.db`。

### scenario 1：音乐 artist 命中本地索引（核心）

1. 系统音乐目录放几首带 artist 标签的歌（如周华健的歌，**文件名不含 artist**）。
2. 设置页「立即索引」→ 提示「音乐 新增 N…」。
3. 搜索 `查找周华健的歌` → 应命中那些歌（**即使文件名不含"周华健"**，靠本地音乐 metadata 索引）。
4. 打开「来源」列：命中项来源应含 `native_index`（本地索引）；若系统搜索也命中同文件，显示
   `spotlight + native_index`（多源合并去重为一条）。

### scenario 2：文档正文命中本地索引

1. 文档目录放一个正文含「季度预算」的 docx/pdf/txt（**文件名不含该词**）。
2. 「立即索引」→ 提示「文档 新增 N…」。
3. 搜索 `找包含季度预算的文档` 或 `find files containing budget`（英文）→ 应命中该文档
   （靠 DocumentIndex 正文 FTS）。

### scenario 3：纯文件名查询不变（回归）

1. 搜索 `find pdf`（纯扩展名）→ 仍走系统后端 fallback 链路（不 fan-out），结果与之前一致。

### scenario 4：未索引时不破坏系统搜索

1. 删掉 `<data_dir>/Scout/index.db`（或换新机首次启动，未点「立即索引」）。
2. 搜索任意内容查询 → 本地索引返回空、不报错，系统搜索照常返回结果。

### 退出条件

scenario 1–2 命中本地源（来源列见 `native_index`，多源时合并显示），scenario 3–4 无回归，
即 BETA-04 真机端到端验收通过。**fan-out / 合并 / 路由逻辑已由 harness + backend 单测覆盖**，
本手测专验 Tauri UI 端到端 + 真实索引数据。

---

## BETA-01A 全盘音频索引（发现层）

> 前置：Windows 装 Everything CLI（`winget install voidtools.Everything.Cli`）+ Everything 服务运行。
> `npm run tauri dev` 起窗。

### scenario 1：全盘索引（超越 Music 目录）

1. 确保电脑里有音频散落在非 Music 目录（如 OneDrive、下载）。
2. 设置页「立即索引」→ 提示「音乐 新增 N…」。N 应远多于固定 Music 目录（全盘发现）。
3. 观察：耗时应明显短于单线程（rayon 并行）；不应卡在 OneDrive "仅在线"文件上。

### scenario 2：OneDrive 占位符不触发下载

1. 索引前记下某「仅在线」（云图标）OneDrive 音频的状态。
2. 「立即索引」后，该文件**仍是「仅在线」**（未被下载到本地）——占位符只存了文件名、没读内容。
3. 该文件仍可按**文件名**搜到（artist 标签则无，因未读标签）。

### scenario 3：跨目录搜索命中

1. 搜索某散落音频的 artist（有标签者）或文件名片段（≥3 字符）。
2. 命中本地索引（来源列含 `native_index`），路径在非 Music 目录。

### 退出条件

scenario 1 全盘入库（N 远超 Music 目录）、scenario 2 占位符不下载且按名可搜、scenario 3 跨目录命中，
即 BETA-01A 真机端到端验收通过。**发现/占位符/并行/file_name FTS 逻辑已由单测覆盖**，本手测专验真机
Everything 发现 + OneDrive 真实占位符行为。

---

## BETA-06 操作审计日志

> 前置：`npm run tauri dev` 起窗。

### scenario 1：文件操作被记录

1. 搜索一些文件，双击打开 1 个、右键「在文件夹中显示」1 个。
2. 复制/移动/重命名 1 个文件（走确认流，点确认）。
3. 设置页 →「操作记录」→ 应见对应条目（时间 / 操作 / 文件路径 / 已执行）。

### scenario 2：失败也记录

1. 触发一次失败操作（如复制到已存在的目标 → PathConflict）。
2. 「操作记录」该条结果为「失败(PathConflict)」。

### scenario 3：一键清除 + 不上传

1. 「清除记录」→ 列表空、`<data_dir>/Scout/audit.jsonl` 被删。
2. （隐私）确认 audit.jsonl 仅在本机，无任何网络上传。

### 退出条件

scenario 1 操作入审计、scenario 2 失败入审计、scenario 3 一键清空，即 BETA-06 真机端到端验收通过。
**审计写入/读取/清除/分类逻辑已由单测覆盖**，本手测专验 Tauri UI 端到端 + 真实文件操作。

---

## BETA-07 后台索引调度

> 前置：`npm run tauri dev` 起窗。Windows 装 Everything CLI。

### scenario 1：启动后台自动索引（不点任何按钮）

1. **首次启动后不点「立即索引」**，稍等（首次较久、后续启动秒级）。
2. 设置页「本地索引」→ 应显示「⏳ 正在后台索引…」→ 完成后变「上次索引: <时间>（音乐 N / 文档 M）」。
3. 直接搜本地音乐/文档 → **已有结果**（无需手动索引）。

### scenario 2：并发守卫

1. 后台索引进行中（显示「正在后台索引…」）点「立即索引」→ 提示「正在索引中，请稍候」（不并发重索引）。

### scenario 3：stale 回收

1. 索引完成后，删除一个已被索引的音乐文件。
2. 重启 app（或再次「立即索引」）→ 该文件从搜索结果消失（prune_deleted 回收）。

### 退出条件

scenario 1 启动后台自动索引、状态可见；scenario 2 守卫生效；scenario 3 已删文件回收，
即 BETA-07 真机端到端验收通过。**prune/守卫/状态逻辑已单测覆盖**，本手测专验启动后台索引 + UI 状态。

---

## BETA-03 图片 OCR 内容索引

> 前置：`npm run tauri dev` 起窗。Windows 需已装 OCR 识别语言包（设置 → 语言 → 中文简体），
> 检查见 [windows-setup §6.1](./windows-setup.md)。无 OCR 能力时图片轮静默跳过（音乐/文档照常）。

### scenario 1：截图含字可搜

1. 准备一张含明显中文文字（如「会议纪要」「项目预算」）的截图，放进系统**图片目录**
   （`dirs::picture_dir()`，Win 通常 `图片`/`Pictures`，含其截图子目录）。
2. 设置页「立即索引」（或重启后台自动索引）→ 完成摘要应含「图片 N」（N ≥ 1）。
3. 搜该截图里的文字（如「会议纪要」）→ **命中该截图**，结果列表能打开/定位到图片。

### scenario 2：无内容词的图片查询交系统后端

1. 搜「截图」「找图片」这类**无具体内容词**的查询 → 走系统后端按文件名/类型搜（不依赖 OCR）。

### scenario 3：OCR 能力缺失优雅降级

1. （可选）在无 OCR 语言包的机器上 → 索引摘要「图片 0」、不报错；音乐/文档结果正常。

### scenario 4：超大图（宽或高 > OcrEngine.MaxImageDimension）自动缩放

1. 准备一张宽或高超过引擎上限（通常约 8192px，以 `[Windows.Media.Ocr.OcrEngine]::MaxImageDimension`
   真机取值为准）且含明显文字的图片，放进图片目录。
2. 索引 → 不应再报 `OCR 进程失败: The parameter is incorrect. Image dimensions are too large!`；
   摘要「图片 N」计入该图（非 failed）。
3. 搜图中文字 → 命中该图。

### 退出条件

scenario 1 截图文字命中、scenario 2 无内容词走系统后端、scenario 3 无 OCR 能力不崩、
scenario 4 超大图自动缩放后仍能识别，即 BETA-03 真机端到端验收通过。
**引擎/归一/路由/回收逻辑已单测 + `tests/real_ocr.rs`（`#[ignore]`）
真机断言覆盖**，本手测专验图片入图片目录 → reindex → 搜命中的端到端体验。

## fallback chain 后端回退（Windows，真双后端）

> 前置：Windows 真机，已装 Everything + ES CLI（`es.exe` 在 PATH）+ Windows Search 服务运行。
> 真双后端回退的**编排核心 + telemetry 归属**已由 `#[ignore]` 集成测试
> `fallback_chain_windows_search_misses_then_everything_serves`（`apps/desktop/src-tauri/src/main.rs`）
> 真机断言覆盖：WindowsSearch 对未索引 scope 干净返回 0 → `SwitchReason::Empty` 切到 Everything、
> `served_by` 归属 Everything、命中探针文件。运行：
> `cargo test -p scout-desktop fallback_chain_windows -- --ignored --nocapture`。
> 本手测专验**前端回退提示条**（`BackendSwitched` 事件的 UI 呈现）。

### scenario 1：回退提示条可见（UI）

1. `npm run tauri dev` 起窗。
2. 触发一次会真实发生后端切换的查询（最稳的造法：把一个唯一命名文件放进 `%TEMP%` 子目录
   ——Windows Search 默认不索引 `AppData\Local\Temp`——再用该文件名做**纯文件名查询**）。
3. 若该查询经 fallback chain 且首候选无结果 → 结果区上方应出现一行浅黄提示
   「↪ Windows Search 无结果，已改用 Everything」，且结果来自 Everything。
4. 重新发起任意查询 → 提示条应被清空（每轮重置）。

### 已知架构限制（验证中发现，记 backlog）

- 生产 wiring 里**内容查询**（含 keyword）走 **fan-out**（`search.local` + `search.windows` 同查合并），
  **不经 fallback chain，且 fan-out 不含 Everything**；只有**纯文件名/扩展名查询**才走 chain，
  且候选序 `[everything, local, windows]` 中 Everything（扫全盘 MFT）排首位、几乎不漏
  → **真实切换是很少触发的安全网**。
- 因此「WindowsSearch 漏 → Everything 兜底」的价值场景实际落在内容查询路径，而该路径目前不咨询
  Everything ⇒ 非索引位置的**内容**查询会被漏掉。属真实产品缺口，见 ROADMAP B2 backlog。

### 退出条件

scenario 1 回退提示条按预期出现并在新查询重置即通过（UI 层）。**回退机制本身（切换/去重/归属/
取消）已由 harness mock 单测 9 例 + 本节真机集成测试覆盖**，本手测专验前端呈现。

---

## BETA-19 跨范畴多类型均衡 + 跨范畴视觉媒体路由（2026-06-03）

> 前置：Windows 真机，盘内**同时有图片和视频**（你的真机正是发现此 bug 的环境：
> 图片 10 万+ / 视频 162）。用 `npm run tauri dev` 起窗（**便携 exe 不含本特性**，它构建于特性落地前）。
> 关联：[BETA-19 spec](./superpowers/specs/2026-06-03-cross-category-balanced-display-design.md)。

本节验两件事：① **BETA-19** 多 `file_type` 查询按类型分查 + round-robin 交错，少数派不被碾压；
② **跨范畴视觉媒体路由** 带修饰的「最大的图片和视频」落 file_search 复用 ① 的均衡（而非 media 丢一类）。

### scenario 1：「图片和视频」少数派可见（BETA-19 核心）

1. 起窗后搜 `图片和视频`（或 `找图片和视频`）。
2. **期望**：结果列表里**视频**（.mp4/.mov/.mkv 等）和图片（.jpg/.png 等）**交错出现**，
   前若干条里两类都能看到——而不是整屏全是图片、视频一个不见。
3. 对照（修复前行为）：旧版单后端 limit-50 + 按修改时间排 → top-50 几乎全是图片，视频不可见。

### scenario 2：「最大的图片和视频」路由 + 均衡 + 按 size 排（跨范畴媒体路由核心）

1. 搜 `最大的图片和视频`（英文 `find the biggest images and videos`）。
2. **期望**：① 顶部 intent 徽章显示 **file_search**（不是 media_search）；
   ② 结果**最大的图片、最大的视频交错**——既均衡又按体积从大到小。
3. trace 佐证：该查询的 `intent_summary` 应是 file_search 类型（见下「观测」）。

### scenario 3：单类型 / 带 artist 不受影响（回归守护）

1. 搜 `最大的视频`（单视觉类型）→ **期望** intent 徽章 **media_search**、media_type=video（行为不变）。
2. 搜 `找周华健的图片和视频`（带 artist）→ **期望** 仍 **media_search**（artist 语义优先，不误降级）。
3. 搜 `find pdf`（单类型文档）→ **期望** 行为完全不变（均衡分支不触发）。

### 观测（开 trace）

```bash
# Windows PowerShell
$env:SCOUT_TRACE="$env:TEMP\scout-b19.jsonl"; npm run tauri dev
```

- scenario 1/2：跑完后看 JSONL 里 `tool_call` 的 `tool_id` 与 `tool_result` 的 `result_count`。
  跨范畴查询会对**每个类型各发一轮**后端调用（图片一轮、视频一轮）。
- scenario 2：确认走的是 file_search 路径（不是 media_search）——可由 intent 徽章 + 结果含两类型佐证。
- 把 JSONL 路径发我，我可读 trace 帮你核对路由与结果计数。

### 退出条件

- scenario 1：「图片和视频」前列能同时看到图片与视频（少数派可见）。
- scenario 2：「最大的图片和视频」走 file_search、两类型按体积交错。
- scenario 3：单类型 / 带 artist / 单类型文档三项行为均无变化（无回归）。

---

## BETA-23 模型 fallback（需 --features model-fallback-metal 构建 + 模型文件就位）

> **实测结论（2026-06-13，macOS release bundle）：场景 1-3 全过**。铁证 = 重搜后意图行出现「模型补全」徽标（两实例复现）+ 设置页状态三态（已就绪 / 未找到+放置提示 / 文件恢复后实时翻转「首次触发时自动加载」）。「正在理解查询…」提示因 metal warm 推理过快（≪500ms）肉眼/截图难捕捉——以徽标为准。场景 4 未跑（默认构建行为由单测覆盖）。
> **打包注意（BETA-25 前）**：`tauri build --features model-fallback-metal` 的 .app 缺 llama 动态库会启动即崩，需手工 `cp target/llama-cmake-cache/*/lib/*.dylib → Contents/Frameworks/` + `install_name_tool -add_rpath @executable_path/../Frameworks <二进制>` + `codesign --force --deep -s -`；首次构建若曾以旧 minimumSystemVersion 配置过，先 `rm -rf target/llama-cmake-cache`。

前置：mkdir -p ~/Library/Application\ Support/Scout/models && cp models/qwen3-0.6b-q4_k_m.gguf ~/Library/Application\ Support/Scout/models/

1. 搜「2025年的会议纪要文件名包含运维」→ 出现「正在理解查询…」提示；首次触发本次结果为 parser（后台加载+预热约 3-4s），稍候再搜同句 → 提示短暂出现后正常出结果（warm 推理 ~350ms）。注：当前 LoRA 模型对 keywords 补全输出空 patch（BETA-24 重训后才会真正补出「会议纪要」），本场景验证的是触发/等待/回落链路。
2. 设置页：模型状态行显示「已就绪 + 路径」；关闭「模型 fallback」开关再搜 → 不再出现提示；改 model_path 后状态行提示重启生效。
3. 移走模型文件重启 → 状态行「未找到模型文件 + 放置路径」，搜索全功能正常（parser-only 降级）。
4. 默认构建（不带 feature）→ 状态行「本构建不含模型支持」，一切照旧。

## BETA-24 keywords 补全重训（接 BETA-23，需换上 BETA-24 模型）

> **前置**：BETA-24 GGUF 已部署到 `models/qwen3-0.6b-q4_k_m.gguf`（sha256 `3aef6efb…`，会话已换）。手测前同 BETA-23 拷到 `~/Library/Application Support/Scout/models/`。构建/打包注意同 BETA-23 节（BETA-25 动态库手工补）。
> **与 BETA-23 的区别**：BETA-23 模型对 keywords 补全输出空 patch（链路通但补不出词）；BETA-24 重训后**真正补出内容词**。evals 已验 held-out 90% 补全、v0.9 零回归。本节验证真机端到端补全生效。

1. **问题 4 端到端补全（核心）**：搜「2025年的会议纪要文件名包含运维」→ 触发模型 → 重搜出现「模型补全」徽标 → **结果应含「会议纪要」命中**（对照合成 fixture `Documents/合成-会议纪要-001.md`，或用户真实同型文件）。区别于 BETA-23：此前模型补不出「会议纪要」，本轮应补出。
2. **媒体主题补全**：搜「放点适合海边日落的歌」→ 触发 → 模型补出主题词「海边日落」（媒体臂 audio-only 生效）。
3. **不过度积极（回归守护）**：搜「找最大的视频」「找上周截的截图」→ **不应**出现幻觉关键词（修复前模型会吐「大小单位/热门」等）；结果与 parser 一致。
4. **BETA-23 回归**：BETA-23 场景 2/3（设置页状态三态 / 开关 / 无模型降级）重过，确认换模型不影响这些行为。

---

## BETA-11D 用户同义词库

> 关联：[spec](./superpowers/specs/2026-06-13-beta-11d-user-synonym-dictionary-design.md) / [plan](./superpowers/plans/2026-06-13-beta-11d-user-synonym-dictionary.md)

> 前置：`npm run tauri dev` 起窗。用户同义词持久化文件位于 `~/Library/Application Support/Scout/user-synonyms.yaml`（macOS）/ `%APPDATA%\Scout\user-synonyms.yaml`（Windows）。

### scenario 1：设置页「我的同义词」增 / 删 / 导入导出

1. 打开设置页 →「我的同义词」→ 列表初始为空（或显示已有词条）。
2. 新增一组：head 填 `测试词`，aliases 填 `alias1, alias2`，点「保存」→ 列表立即出现新条目（无需重启）。
3. 编辑该条目（修改 aliases），点「保存」→ 列表即时更新。
4. 删除该条目 → 列表立即消失；`user-synonyms.yaml` 中对应条目已移除。
5. 点「导出」→ 下载或另存 `user-synonyms.yaml`；点「导入」→ 选择一个合法 yaml → 词条合并入列表，无效文件给出错误提示。

**验证点**：增 / 删 / 导入导出均不需要重启；列表实时反映文件状态。

### scenario 2：lint 错误内联提示

| 操作 | 期望 |
|---|---|
| 新增一组，aliases 填超过 8 个词（如 `a,b,c,d,e,f,g,h,i`）| 保存被拒，内联红字提示「别名超过 8 个」|
| head 填 `synthetic-place`（ASCII 标识符含连字符）| 保存被拒，提示「标识符型词不支持作为 head 或 alias」|
| head 填 `abc123测试`（混合语言单词）| 保存被拒，提示「混合语言词不支持」|

**验证点**：lint 错误内联显示、不落盘、不弹全页弹窗。

### scenario 3：端到端目标 case（记住映射 → 重启仍命中）

> 前置：在桌面或文档目录创建合成 fixture（让 Spotlight 索引到）：
> ```bash
> mkdir -p ~/Desktop/scout-beta11d-fixtures
> touch "~/Desktop/scout-beta11d-fixtures/aws计算产品分析.md" \
>       "~/Desktop/scout-beta11d-fixtures/Azure功能洞察.md"
> mdimport ~/Desktop/scout-beta11d-fixtures/
> ```
> 等 30~60 秒确认 `mdfind aws计算产品分析` 能命中。

1. 搜「友商竞争分析」→ 返回**零结果**。
2. UI 出现提示条「未找到结果，是否扩展搜索？」（或类似措辞）→ 点击进入扩展流程。
3. 在输入框手填 `AWS, Azure, 产品分析, 功能洞察` → 点「扩展搜索」→ 结果区应命中 `aws计算产品分析.md` 和 `Azure功能洞察.md`。
4. UI 提示「是否记住此映射？」→ 点「**记住**」→ 弹框消失。
5. **重启 app**（`Ctrl+C` 停开发服务器，再 `npm run tauri dev`）。
6. 重启后再搜「友商竞争分析」→ **直接命中**（无需再走扩展流程），延迟与普通搜索相当。

**验证点**：用户词典沉淀 → 重启后用户层同义词覆盖生效；trace 中 `synonym_expand` 事件的 `source` 字段为 `"user"`。

### scenario 4：「不记住」分支

1. 重复 scenario 3 的步骤 1–4，但最后点「**不记住**」。
2. **重启 app** 后再搜「友商竞争分析」→ 仍返回**零结果**（映射未沉淀）。

**验证点**：「不记住」路径不落盘；重启后行为与未教学时一致。

### scenario 5：设置页可见用户同义词库落盘

> 2026-07-29 起「隐私与记录」并入「索引」tab，原「数据存在哪」逐文件路径表格已简化为
> 单条「数据存储位置」（统一目录 + 总大小，见 IndexingPane）；同义词组数不再在此重复
> 展示——「术语与同义词」tab 本身就是这些组的完整实时列表，直接看那里更准确。

1. 按 scenario 3 记住至少一组映射。
2. 打开「术语与同义词」tab → 应看到新记住的这组映射出现在「已有同义词组」列表。
3. 打开「索引」tab →「数据存储位置」→ 应显示统一数据目录路径 + 总大小（含
   `user-synonyms.yaml` 后应比记住前更大，非精确到单文件、但能确认确实落盘了）。
4. 在「术语与同义词」删除所有词条后 → 该 tab 列表变为「暂无同义词组」。

### 退出条件

scenario 1–4 全过（尤其 #3 端到端「记住 → 重启 → 直接命中」必过）+ scenario 5 设置页可见
用户同义词库确实落盘，即 BETA-11D 真机端到端验收通过。**持久化 / 双层叠加 / lint / 卸载
逻辑已由单测覆盖**，本手测专验 Tauri UI 端到端 + 真实 user-synonyms.yaml 行为。

---

## BETA-25：静态链接打包真机验收（macOS）

前置：`--features model-fallback-metal` 打 release bundle；模型已部署
`~/Library/Application Support/Scout/models/qwen3-0.6b-q4_k_m.gguf`。

1. **未修补启动**：直接双击 `Scout.app`（不做任何 Frameworks/rpath/重签手工补丁），应正常打开，不报
   `Library not loaded: @rpath/libggml-base.0.dylib`。
2. **零 dylib 依赖**：`otool -L <app>/Contents/MacOS/scout-desktop | grep -iE "ggml|llama|mtmd"` 应无输出。
3. **问题 4 端到端**：搜「2025年的会议纪要文件名包含运维」→ 触发模型 fallback → 结果出现「模型补全」徽标、
   补出「会议纪要」关键词（与 BETA-24 手测一致）。
4. **无模型降级**：临时移走模型文件 → 同一 query 不崩、静默降级 parser-only。

Windows（留下个 Windows 会话）：CI 打出的 NSIS 安装包安装后启动不缺 DLL、模型放置后问题 4 端到端通。

---

## BETA-15B-1：语义召回纵切真机验收（macOS）

前置：`--features semantic-recall-metal` 打 release bundle（或 `cargo tauri dev --features semantic-recall-metal`）；
把 Qwen3-Embedding-0.6B GGUF 放到 `~/Library/Application Support/Scout/models/qwen3-embedding-0.6b-q4_k_m.gguf`
（或在设置页填 `embedding_model_path`）。放好后 reindex 一次（设置页或重启触发后台索引）。

### scenario 1：模型状态行三态

1. 不放 embedding 模型 → 设置页「语义召回」状态行显示「模型未找到 —— 放到 {路径} 后将自动启用」。
2. 放入模型文件 → 状态行变「已就绪」（或首次触发时「加载中…」）。
3. 关闭 `semantic-recall` feature 的构建 → 显示「不可用（feature 未开启）」。

### scenario 2：跨语言召回（最硬卖点）

1. 索引一个含**英文文档**的目录（reindex 后终端日志出「语义索引：嵌入 N 篇」）。
2. 用**中文 query** 搜一个只在英文文档里出现的概念（query 不含任何英文精确词，如中文搜「退款政策」找英文 refund policy 文档）。
3. **期望**：该英文文档出现在结果里，「匹配方式」列显示蓝色「按意思找到」徽标。
4. 对照：移走 embedding 模型（或关 feature）重搜 → 该文档搜不到（纯 FTS5 跨语言结构性命中不了）。

### scenario 3：模糊同义召回

1. query 用与正文不同的措辞（如「讲怎么处理退货的文档」对正文写「退款流程」的文件）。
2. **期望**：命中，徽标「按意思找到」。

### scenario 4：exact-name 不回退

1. 用精确文件名/标题词搜一个已知文件。
2. **期望**：仍稳定排在前列（语义臂只增不减，不拖垮精确名）。BETA-26 守护桶已证 0/12 回退。

### scenario 5：无模型优雅降级

1. 不放 embedding 模型（或 default 构建无 `semantic-recall`），重启。
2. **期望**：搜索行为与今天完全一致（纯 FTS5），无报错；设置页显示模型未找到。reindex 不因缺模型变慢/报错。

### 退出条件

scenario 2（跨语言）+ scenario 4（exact-name 不退）+ scenario 5（无模型降级）全过，
即 BETA-15B-1 旗舰语义召回纵切真机端到端验收通过。**向量存储 / 融合 / 内联嵌入 / 句柄并发已由单测覆盖**，
本手测专验 Tauri 端到端 + 真实 embedding 模型在真机语料上的召回手感（尤其跨语言徽标）。

> 注（已知限制，记 15B-3/schema 跟进）：语义臂当前 embed 的是 parser 抽取的**关键词拼接**（Search Intent schema 无原始 query 字段），
> 非用户原始整句。跨语言核心价值保留（多语言 embedding 把中文内容词映射到英文文档），损失主要是停用词/语序框架细节。

## BETA-15B-2：向量索引后台预热 + 解耦调度

前提：feature `semantic-recall`（+ metal）构建，约定目录放 embedding 模型（qwen3-embedding-0.6b-q4_k_m.gguf）。

### scenario 1：冷启动消除

1. 放好模型 → 启动 app → 设置页等「🧠 语义索引就绪 N 篇」出现 → 执行跨语言 query（中文「年假和休假规定」命中纯英文 leave policy 文档）。
2. **期望**：**首个查询不再 16.8s**（对比 15B-1 实测冷加载）——模型在启动后台已预热加载，首查询走暖路径。

### scenario 2：解耦可搜

1. 删 index.db（或大量改动）触发全量 → FTS 结果**秒级可搜**的同时，设置页状态条显示「🧠 语义索引中 X/Y」渐进推进。
2. **期望**：期间执行普通文件名查询应正常返回（FTS 不被嵌入阻塞）。

### scenario 3：手动 reindex 不被挡

1. 语义索引中点「立即索引」。
2. **期望**：FTS 照常完成（不报「正在索引中」），语义 worker 见守卫跳过本轮（已知限制：本轮新增文档延到下次触发补嵌）。

### scenario 4：无模型降级

1. 移走模型文件 → 启动。
2. **期望**：无语义行、搜索行为与今天一致（FTS-only）。

### scenario 5：双平台

1. macOS + Windows 各验 scenario 1、2。
2. Windows 是 16.8s 冷启动证据来源，**必验暖机后首查询提速**。

实测记录（填入会话日志）：暖机耗时、FTS 可搜时延、语义 worker 全量耗时、常驻内存。

## BETA-15B-3 簇 B（列 UX 迁移 + worker panic 兜底）

1. **列迁移（老用户补显）**：用一份「匹配方式」列隐藏的旧 localStorage（`scout.columns.v1` 的 visible 不含 `match`、无 version）→ 升级后启动 app → 结果列表「匹配方式」列**自动出现**，语义命中显「按意思找到」徽标。
2. **迁移只一次 + 尊重意图**：步骤 1 后手动隐藏「匹配方式」列 → 重启 app → 该列**仍隐藏**（迁移已标 version=2，不再强加）。
3. **新安装**：清空 localStorage → 启动 → 「匹配方式」列默认可见（`defaultVisible:true`）。
4. **worker 兜底**（难自然触发，主要靠单测 `run_semantic_worker_clears_guard_on_panic`）：若能注入会 panic 的 embedding 模型，验证设置页不卡「语义索引中」、降级 FTS-only。

## BETA-15B-3 簇 A-1（语义臂相似度下限）

前提：feature `semantic-recall`（+ metal）构建 + 放 embedding 模型 + 已 reindex 出向量。

1. **低相关不再凑数**：用一个与本机文档都不太相关的查询 → 结果不再出现一堆打「按意思找到」徽标的凑数语义项（cosine < 0.30 的被挡）。
2. **真实跨语言命中仍浮现**：复跑 15B-1 的跨语言用例（中文「年假和休假规定」命中纯英文 leave policy 文档）→ 仍正常返回 + 打「按意思找到」徽标（高 cosine，不受 0.30 下限影响）。
3. **阈值观感**：若仍觉得有低相关项漏网（偏松）或真实命中被挡（偏紧），记录现象——`SIMILARITY_FLOOR` 是一行可调的命名常量，留簇 A held-out 评测精调。

## BETA-15B-3 簇 A-1 续（相似度下限可调 + cosine 分数可见）

前提：feature `semantic-recall`（+ metal）+ 放 embedding 模型 + 已 reindex 出向量。

1. **分数可见**：做语义查询 → 结果「匹配方式」列显「按意思找到 · 0.XX」（cosine 2 位小数）。看不相关项 vs 真实命中各多少分。
2. **调下限即时生效**：设置页把「语义相似度下限」从 0.30 调高（如 0.45）→ 保存 → **回搜索框重新搜同一查询（无需重启）** → 低分项消失、只留高分命中。
3. **找到合适值**：据步骤 1 看到的分数分布，把下限调到「不相关项被挡、真实命中保留」的甜点值，记录之（反馈给开发 bake 为新默认）。
4. **越界/默认**：留空或填非法值 → 回落 0.30；填 >1 或 <0 → 后端 clamp。未碰设置的用户行为与之前一致。

## BETA-27 可配置索引目录 + 排除规则

前提：feature `semantic-recall`（+ metal）+ 放 embedding 模型。

1. **加目录**：设置页 → 索引目录「+ 添加目录」选一个含文档的文件夹（如桌面 / D:\工作）→ 保存 → 立即索引 → 设置页语义索引篇数增加、该目录文档可「按意思找到」。
2. **排除规则**：在含 `node_modules` 的项目夹上加索引目录 → 排除规则含 `node_modules`（默认即有）→ 立即索引 → `node_modules` 内文件搜不到。
3. **空配置 = 默认**：清空索引目录列表 → 立即索引 → 仍索引系统音乐/文档/图片（与之前一致）。
4. **隐私面板**：隐私面板「索引范围」显示真实索引根（你配的目录），而非旧的 `~`。
5. **通配符**：加排除 `*cache*` → 名字含 cache 的子目录被剪掉。
6. **Tauri 权限冒烟（重点）**：本版新增 dialog 插件 + capability，确认现有功能不被 ACL 锁掉——全局快捷键唤起、搜索、设置读写、立即索引均正常；「+ 添加目录」能真正弹出系统文件夹对话框。

## BETA-15B-5 语义召回可信化（可解释 v1）

前提：feature `semantic-recall`（macOS 加 `-metal`，Windows 走 CPU）构建 + 放好 embedding 模型 + 已 reindex 出文档向量（设置页语义索引篇数 > 0）。
核心要验的是「**为什么这条被召回**」可感知、可信——段落高亮 / 来源标注 / 置信档位三件套。

### scenario 1：跨语言段落高亮真命中（杀手特性，核心）

1. 用一个**跨语言**查询：中文 query 命中纯英文文档（复用 15B-1 的「年假和休假规定」→ 英文 leave policy 文档；或自备一篇英文文档 + 中文意思查询）。
2. 结果列表里该英文文档应打「按意思找到」徽标。**点选它**打开右侧预览面板。
3. **观测**：预览正文里**与你查询语义对应的那一段被蓝色高亮**（区别于 FTS 关键词命中的黄色高亮），哪怕该段与中文 query **零词面重叠**。鼠标悬停高亮段显「语义相似度 0.XX」。
4. **判定**：高亮的是不是真正讲「年假/休假」的那句英文？高亮**命中**＝杀手特性成立；高亮到无关段或不高亮＝记录 query/文档/截图反馈。

### scenario 2：召回来源标注

1. 做一个纯语义命中的查询（结果靠意思召回、非文件名/关键词）→ 该结果「匹配方式」列显「**纯语义命中**」。
2.（若本机能造出关键词+语义双中的结果）显「**关键词+语义双中**」。**注**：当前单源 fanout 架构下双中分支通常不出现，是为未来多源融合铺路，不出现属正常，不算 bug。
3. 非语义结果（纯文件名/关键词命中）「匹配方式」列照旧显「文件名/内容」等，**不**打语义徽标。

### scenario 3：置信档位 + 下限说明

1. scenario 1 的预览面板里，正文上方应有「**语义命中**」行：「最相似段落 · 语义相似度 0.XX（强/中/弱相关）」+「低于 X 的弱相关结果已隐藏」。
2. **档位**：cosine ≥0.5 显「强相关」、≥0.3「中相关」。注意预览里出现的段落分**至少 0.30**（低于 `EXPLAIN_MIN_SCORE=0.30` 的段不高亮），故「弱相关」档基本只出现在边缘。
3. **下限随设置刷新**：去设置页把「语义相似度下限」调高（如 0.50）保存 → 回搜索页重开预览 → 说明行的 X 变成 0.50（验证不再显示过期阈值）。

### scenario 4：延迟与体验（双平台，Windows 重点）

1. **macOS（Metal）**：点选语义结果到高亮出现应近乎无感（百毫秒级）。
2. **Windows（CPU）**：当前 v1 **无独立 spinner**——点选后预览正文先以**无高亮**显示，单篇切句+逐段 embed（≤16 段）算完后高亮**渐进浮现**。记录这段「正文先出、高亮后到」的延迟是否可接受（点击触发、非搜索主路径）。**若觉得空窗期需要提示**，记下来作为后续 polish（spec 曾设想骨架占位，v1 未实现，留待按需补）。
3. 复点同一结果不应重复 invoke（依赖已精确化为 path+match_type）。

### scenario 5：退化等价（关键回归）

1. **关 feature / 无模型**：用不含 `semantic-recall` 的构建，或移走模型文件 → 搜索照常工作；点选结果**无任何语义高亮 / 无来源徽标 / 无置信行**，正文渲染与加本特性前**逐字节一致**。
2. **非文档结果**：点选音乐/未索引结果 → 无段落高亮（无正文可解释），不报错不卡。

### 退出条件

- scenario 1 跨语言高亮**真命中语义对应段**（双平台至少 macOS 验通）。
- 来源标注、置信档位、下限说明显示正确且与直觉一致。
- Windows 延迟可接受（骨架占位 + 算完替换，无卡死）。
- 退化路径（关 feature / 无模型 / 非文档）零异常、与改前一致。
- 全程隐私：预览正文/路径/向量不外发、不进 trace（按构造，无需手验，留意无异常网络即可）。

## BETA-33 cycle 9：单实例锁 + embed 失败降级 + WSearch 状态条

### scenario 1：单实例锁（Windows 重点）

1. 启动 Scout（装机版或 dev 均可）→ 再次双击启动第二个实例。
2. **判定**：第二实例**不出现新窗口**、进程自行退出；既有窗口被**取消最小化并带到前台**（先手动最小化再启动第二实例可同时验证两点）。
3. 任务管理器确认仅一个 Scout 进程（不算 WebView2 子进程）。
4. 回归：关闭应用后再启动，正常打开（锁随进程释放，不残留）。

### scenario 2：embed 失败整链降级（无模型 / 加载失败）

1. **模型缺失**：移走 `models/embeddinggemma-300m-q8_0.gguf` → 启动 → 任意内容关键词搜索。
   - **判定**：结果正常出（系统搜索 + 本地索引照常）、无「embedding 模型不可用」红色错误；零结果时报「未找到结果」而非 embed 错误。顶栏「语义召回」灯不亮、设置页 EmbedStatus 显「未找到模型」——真实状态只在状态面呈现，不冒充搜索错误。
2. **模型就位后回归**：放回模型文件 → **无需重启**再搜 → 语义臂自动回归（「按意思找到」徽标重新出现；首查询或后台暖机付一次加载成本属正常）。
3. **暖机窗口**：冷启动后立即搜索（后台暖机进行中）→ 结果正常、无 embed 报错；暖机完成后再搜语义徽标出现。

### scenario 2b：顶栏语义灯 tooltip 文案（StatusIndicator 文案重写）

1. scenario 2 各态下悬停顶栏「语义召回」灯：tooltip 不再出现完整模型路径或英文 Rust 错误串——
   未找到显「模型未找到 — 可在「选项 → 语义召回」下载」、失败显「模型加载失败 — 详见「选项 → 语义召回」」。
2. 按引导打开「选项 → 语义召回」pane：完整期望路径 / 失败原因在此如实展示（诊断细节的「详见」落点）。

### scenario 3：快速入门第 1 步 Windows 搜索服务状态条（check_windows_search_indexed 真做）

1. 清掉 onboarding 完成态（删 `%APPDATA%` 下 app config 目录的 `onboarding.json`）→ 启动 → 进快速入门第 1 步。
2. **服务运行中**（默认）：说明段下方出现绿色状态条「✓ Windows 搜索服务运行中」。
3. **服务停止**：管理员在「服务」里停止 Windows Search（WSearch）→ 状态条 3s 内自动翻为橙色「⚠ 未运行 + 建议启动」；重新启动服务 → 3s 内翻回绿色（无需重进页面）。
4. 状态条不阻塞流程：任一状态下「我已设置好，下一步」「跳过此步」照常可点。

### scenario 4：全库 vs 概貌口径差提示（「本地索引」口径统一）

1. 造出口径差：在「选项 → 索引」加一个含文档的自定义目录 → 立即索引 → 移除该目录时选「**仅从索引配置移除**」（保留索引记录）。
2. **判定**：概貌卡下方出现提示行「ℹ️ 库内另有 N 条记录在当前生效目录之外…搜索仍会命中它们」，N ≈ 该目录此前入库条数；「总条数」格 hover 显示「当前生效目录内的条数合计」tooltip。
3. 验证提示属实：搜索该已移除目录里的文档 → 仍能命中（全库口径搜索不按 roots 过滤，提示语义正确）。
4. 消除差值：再次移除时选「移除并清除索引记录」（或本页下方「清空本地索引」后重建）→ 提示行消失、概貌合计 =「本地索引」行全库数。
5. 无差值时（全新索引、未移除过目录）：提示行不出现、无噪音。

### scenario 5：设置流回归（useAppSettings 抽取 + 旧 /settings 路由删除）

1. 「工具 → 选项」（或 Ctrl+;）打开选项对话框：改任一字段 → 「应用」→ 绿字「设置已保存」3s 消失；「确定」保存并关；带未保存改动点 ✕ / Esc → 二次确认弹窗照旧。
2. 重启后改动持久化（settings.json 落盘不回退）。
3. 手动导航 `/settings`（如地址栏可达）→ 空白/不匹配路由，不崩（旧路由已删、无入口，属预期）。
4. 快速入门第 5 步「打开索引选项」仍能打开对话框并直达「索引」分类。

### 退出条件

- 第二实例不产生并发写（index.db / settings.json 无双写风险）、既有窗口聚焦。
- 无模型 / 加载失败 / 暖机中三态下搜索全程无「embedding 模型不可用」误导错误，模型就位后免重启自动回归。
- 快速入门第 1 步真实反映 WSearch 服务状态、轮询自动刷新、不拦流程。
- 全库 vs 概貌口径差有据可查（差值提示 + tooltip + 清理路径引导），零差值时零噪音。
- 选项对话框设置流（应用/确定/关闭守卫/持久化）零回归；旧 /settings 删除无遗留入口。

---

## BETA-12:卸载流程(删索引/模型/日志、保留配置)

两条清理路径**语义不同、不是同一份清单**:**Windows 安装版** = NSIS 卸载器 hook(`apps/desktop/src-tauri/nsis/uninstall-hooks.nsh`,卸载时自动执行)——模型 + 索引默认都**保留**(询问一次,静默卸载默认保留),覆盖「卸载是为了重装」场景;**应用内** = 「索引」页「卸载清理」按钮(2026-07-29 起「隐私与记录」并入「索引」;macOS / 免安装版 / 真要清空数据前手动清)——**全删**含索引含模型,是「彻底清空我的本机数据」的显式动作。仓内闸门:`uninstall.rs` 单测 ×5(含 hook 在位 + `$UpdateMode` 升级守卫 + 模型/索引保留路径校验)。

> 前置:装机版已完成一轮索引 + 已下载 embedding 模型 + 加过 ≥1 组用户同义词 + 搜过几次(产生历史/审计/日志)。

| # | 操作 | 期望 |
|---|---|---|
| 1 | 「索引」页 → 「卸载清理」→ 确认 | 逐项报告全绿;`%APPDATA%\Scout` 下 index.db/models/scout.log*/audit.jsonl/user-synonyms.yaml/search_history.json 消失、**settings.json 仍在**(2026-07-29 起设置/历史/同义词库统一收进 `%APPDATA%\Scout`,不再散落到 `%APPDATA%\ai.scout.desktop`);「索引」页「数据存储位置」总大小归零附近(只剩 settings.json) |
| 2 | 清理后重建:「立即索引」 | 索引正常重建,配置(索引目录/开关)未丢 |
| 3 | 索引进行中点「卸载清理」 | 拒绝并提示「正在索引中」;模型下载中同理 |
| 4 | 系统「设置 → 应用」卸载 Scout,MessageBox 询问是否删除模型+索引 → 选**否**(或静默卸载 `/S`) | **索引 index.db(+ -wal/-shm)、models、settings.json 都保留**(同卷暂存 → `%APPDATA%\Scout` 整目录删 → 移回);user-synonyms.yaml/search_history.json 消失(卸载即视为要清这两项敏感派生数据,与模型/索引/配置的「保留」语义不同) |
| 4b | 同 4,但 MessageBox 选**是**(要彻底清空) | `%APPDATA%\Scout` 整目录消失(含 index.db、models、user-synonyms.yaml、search_history.json);**settings.json 仍保留**(两条分支都显式暂存/移回) |
| 4c | 升级前装的老版本曾在 `%APPDATA%\ai.scout.desktop` 留有 user-synonyms.yaml/search_history.json(数据目录统一之前的安装,从未启动过新版本迁移逻辑) | hook 兜底 Delete 仍能清掉这份遗留(见 uninstall-hooks.nsh 注释) |
| 5 | 旧版覆盖升级(装新 setup.exe 不先卸载) | **索引/模型/日志全保留**(`$UpdateMode` 守卫生效,升级不清数据) |
| 6 | 场景 4 卸载(保留索引)后,装新版 setup.exe → 「立即索引」 | 无需整库重扫:新版启动直接沿用保留的 index.db,「立即索引」按 mtime 增量只处理卸载期间新增/改动的文件,原有条目 skip |

### 退出条件

- 场景 1 全删、场景 4(选否)保留模型与索引、场景 4b(选是)全删——三者行为符合各自设计意图,不再要求"清理范围一致"。
- 场景 5 升级零数据损失——此条不过 = 每次发版用户重下数百 MB 模型,视为发版阻断。
- 场景 6 验证"先卸载旧版再装新版"这条手动两步路径下,索引真被复用、后续走增量而非全量重扫——此条不过 = 每次手动重装用户白等一次小时级全量重建。

---

## BETA-29:查询意图可编辑草稿(v1)

意图信息条新增「调整 ▾」入口(默认折叠,不打断主流程);展开后显示本轮生效 intent 的关键字段——关键词/扩展名 chips(可移除/可增)+ 类型/修改时间/排序三个下拉——「按此条件重跑」经 `search_with_intent` 跳过 parser 直接执行(serde 强校验,只收 file_search/media_search)。仓内闸门:search/tests.rs BETA-29 段 ×5。

| # | 操作 | 期望 |
|---|---|---|
| 1 | 搜 `找上周的 pdf` → 意图条「调整 ▾」展开 | 面板显示扩展名 `.pdf`、修改时间「上周」,与实际执行一致 |
| 2 | 把修改时间改「最近 30 天」→ 重跑 | 结果变多(时间窗放宽);意图条摘要仍为 file_search;面板保持展开且回显新值 |
| 3 | parser 误抽关键词的 query(如关键词 chips 里混进容器词)→ × 移除 → 重跑 | 移除后命中改善;这是「parser 长尾→用户一键修正」主场景 |
| 4 | 类型改「图片」+ 排序改「从大到小」→ 重跑 | 结果为图片且按大小降序;未编辑字段(location 等)不丢 |
| 5 | 重跑后接一句细化(如「只要今天的」) | Refine 以草稿修正后的 intent 为基准合并(草稿轮已 record 上下文) |
| 6 | 流式中「按此条件重跑」按钮 | 置灰不可点;完成后恢复 |

### 退出条件

- 修正→重跑→结果变化与修正一致;未编辑字段零丢失(location/size/媒体字段)。
- 场景 5 细化链不断(草稿轮 record 语义与普通搜索一致)。
- 折叠态零打扰:不展开时主搜索流与 v0.9.12 无任何差异。

## BETA-29 v2:草稿保存进保存的搜索 + 搜索前预览

草稿面板新增「保存草稿…」(带 intent 存入 BETA-22 saved searches,重跑走 `search_with_intent`);搜索框新增 ⚙ / Shift+Enter「搜索前预览」(新命令 `preview_intent`,只解析不执行,parser 视角;确认或修正后再搜)。仓内闸门:history.rs 新测 ×2(round-trip/闸门)+ search/tests.rs `preview_intent_parses_without_executing`。

| # | 操作 | 期望 |
|---|---|---|
| 1 | 搜索后展开「调整 ▾」→ 改类型/时间 → 「保存草稿…」→ 命名保存 | 保存条上出现带 ⚙ 角标的 chip;tooltip 注明「含意图草稿」 |
| 2 | 点击带 ⚙ 的保存条目 | 按草稿条件直接执行(跳过 parser),输入框同步 query 文本;结果与保存时修正一致 |
| 3 | 点击不带 ⚙ 的旧保存条目 | 走普通搜索,行为与 v0.9.13 一致(向后兼容,旧 search_history.json 正常加载) |
| 4 | 输入 query 后按 Shift+Enter(或点 ⚙) | 出现「预览」意图条 + 草稿面板,**不执行搜索**(无结果流);「按此条件搜索」后正常出结果 |
| 5 | 预览态直接按 Enter 普通搜索 | 预览条自动收起,普通搜索照常 |
| 6 | 对动作类 query(如「打开第一个」)用 Shift+Enter | 提示「动作/澄清类不支持草稿编辑」并直接执行普通搜索(不挡人) |
| 7 | 预览态「保存草稿…」 | 未执行过搜索也能保存(query 取输入框文本) |

### 退出条件

- 带草稿条目「所存即所跑」:保存时面板显示的条件 = 重跑实际条件。
- 旧版 search_history.json(无 intent 字段)加载零回归。
- 预览全程零 tool call(后端不触发任何搜索后端)。
