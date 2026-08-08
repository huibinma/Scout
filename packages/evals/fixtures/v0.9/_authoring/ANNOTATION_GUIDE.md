# BETA-13 v0.9 覆盖驱动标注指南（子 agent 共享信源）

你在为 Scout 的自然语言搜索 parser 撰写**评测用例**。每条用例 = 一句自然语言 query + 这句 query **应该**被解析成的 `SearchIntent` JSON（ground truth）。

## 0. 最重要的纪律：两轴标注

你标注的是 **「这句 query *应该*解析成什么」**（依 schema 语义 + 功能设计意图），**不是「parser 现在解析成什么」**。你**没有也不应**运行 parser。失败的用例（parser 实际输出 ≠ 你的标注）正是评测要发现的缺口。

但字段分两类，处理方式不同：

- **语义轴**（query 明确表达的约束）：**按意图标**。这是要暴露缺口的地方。
  例：`上个月改过的发票` → 必含 `modified_time={relative,last_month}` + `keywords=["发票"]`。
- **约定轴**（query 没明说、但有惯例默认的字段）：**按下方「约定表」填**，消除无意义的差异噪声。
  例：file_search 没提排序 → 一律 `sort="modified_desc"`。

> 语义轴暴露缺口，约定轴消除噪声。两轴都**不读 parser 源码**。

## 1. 输出契约

- 产出一个 **JSON 数组**，每个元素是一条用例，**写入指定的分片文件**（用 Write 工具）。
- 每条用例形如：

```json
{
  "id": "v09-<domain>-<lang>-001",
  "query": "上个月改过的发票",
  "language": "zh",
  "expected_intent": {
    "intent": "file_search",
    "schema_version": "1.0",
    "language": "zh",
    "keywords": ["发票"],
    "modified_time": { "type": "relative", "value": "last_month" },
    "sort": "modified_desc"
  }
}
```

- `id`：`v09-<domain>-<lang>-NNN`，NNN 从 001 起在你这个域内递增；`<lang>` ∈ `zh`/`en`/`mixed`。域代号由你的任务指定（如 `d1`）。
- `language`（顶层 + expected_intent 内）：`zh`（纯中文）/ `en`（纯英文）/ `mixed`（中英混合）。两处一致。
- `expected_intent` 必须是 **序列化形态**（下文逐字段给形态），顶层恒含 `intent` 与 `schema_version:"1.0"`。
- **只写 query 真正表达的字段 + 约定轴默认字段**。不要硬塞无关字段（比如 query 没提位置就别写 `location`）。

### 质量优先于数量

- 用例要**自然、真实、多样**——像真实用户会打的话，不要机械替换模板。覆盖不同措辞、长度、口语/正式、同义表达。
- 标注**拿不准**的（schema 表达不了、语义有歧义）：宁可不写这条、换一条清晰的。**宁缺毋滥**。
- 每条 query 力求**只有一个合理的标注**（否则它对评测无意义）。

## 2. SearchIntent 五个 variant（serde internally-tagged，字段平铺顶层）

`intent` 字段区分 5 个 variant。所有 variant 顶层都含 `intent` + `schema_version:"1.0"` + 可选 `language`。

### 2.1 `file_search` —— 通用文件搜索

可选字段（按需，序列化形态）：
- `keywords`: `string[]` —— 文件名 / 内容关键词。
- `extensions`: `string[]` —— 扩展名（不含点），如 `["pdf"]`、`["doc","docx"]`。
- `file_type`: **单值写标量字符串**、多值写数组。枚举：`document` / `spreadsheet` / `presentation` / `image` / `screenshot` / `video` / `audio` / `archive` / `code` / `executable`。例：`"presentation"` 或 `["audio","video"]`。
- `location`: `{ "hint": "下载" }` —— 自然语言路径线索（只填 hint，不填 include/exclude）。常见 hint：下载/桌面/文稿(文档)/图片/音乐/影片/截图，英文 downloads/desktop/documents/pictures/music/videos/screenshots。
- `modified_time` / `created_time` / `accessed_time`: TimeExpression（见 §3）。
- `size`: SizeExpression（见 §3）。
- `exclude_extensions`: `string[]`；`exclude_file_type`: `string[]`（FileType 值）。
- `sort`: SortOrder（见 §3）。
- `limit`: 整数。

### 2.2 `media_search` —— 媒体专项（音频/图片/视频/截图）

必填：`media_type`（标量）：`audio` / `image` / `video` / `screenshot`。
媒体专有可选：
- `artist`: string（演唱者/作者）；`title`: string；`album`: string；`genre`: string。
- `quality`: `lossless` / `high` / `standard` / `low`。
- `duration`: SizeExpression，但 `unit` 用 `s`/`m`/`h`（如 `{"type":"greater_than","value":5,"unit":"m"}`）。
其余字段（keywords/extensions/location/时间/size/sort 等）同 file_search。

### 2.3 `file_action` —— 对已选文件执行操作

必填：
- `action`: `open` / `locate` / `copy` / `move` / `rename` / `delete`（完整枚举，序列化为 snake_case）。
- `target_ref`: 目标指代，形态：
  - `{ "source": "last_results", "selector": { "type": "index", "value": 3 } }`（第 3 个）
  - `{ "source": "last_results", "selector": { "type": "indices", "values": [1,3,5] } }`
  - `{ "source": "last_results", "selector": { "type": "all" } }`（全部）
  - `{ "source": "path", "value": "/abs/path" }`（绝对路径）
- `requires_confirmation`: bool —— **写操作（rename/move/copy/delete）恒 `true`**；只读（open/locate）为 `false`。
可选：`destination`（copy/move 的目标路径，绝对路径）；`new_name`（rename 的新名）。

### 2.4 `refine` —— 在上一轮结果上二次筛选

必填：
- `base_ref`: 恒 `"last_intent"`。
- `delta`: 一个对象，含要**新增/覆盖**的字段（keywords/extensions/file_type/location/各时间/size/artist/title/album/genre/quality/duration/sort/limit，形态同上）。
可选：
- `clear`: `string[]` —— 要**清空**的字段名，取值 ∈ `location`/`extensions`/`file_type`/`keywords`/`modified_time`/`created_time`/`accessed_time`/`size`/`exclude_extensions`/`exclude_file_type`/`artist`/`title`/`album`/`genre`/`quality`/`duration`。

`refine` 用于「只看 pdf 的」「再小一点的」「换成上周的」这类**承接上一轮**的追加约束。`delta` 放新增约束，`clear` 放要去掉的约束。

### 2.5 `clarify` —— 查询太模糊，反问澄清

必填：
- `reason`: `ambiguous_time` / `ambiguous_location` / `ambiguous_type` / `ambiguous_action` / `unsafe_action` / `unknown`。
- `question`: 给用户的问题文本（**评测忽略文本内容**，给一句合理中文/英文即可）。
可选：`options`: `string[]`（评测只看结构存在，不看内容）。

**clarify 触发场景**：query 模糊到无法产出可执行 intent。如纯时间词无约束（`昨天的`）、无法解析的位置、类型模糊、高风险写操作要确认（`unsafe_action`）。**注意**：有其他强约束时通常**不**该 clarify（如 `昨天的ppt` 是合法 file_search，不是 clarify）。

## 3. 公共字段形态

### TimeExpression
```json
{ "type": "relative", "value": "yesterday" }
{ "type": "absolute", "from": "2026-05-20", "to": "2026-05-24" }
{ "type": "before",   "value": "2026-05-20" }
{ "type": "after",    "value": "2026-05-20" }
```
relative 的 value 枚举：`today` `yesterday` `last_3_days` `last_7_days` `last_14_days` `last_30_days` `this_week` `last_week` `this_month` `last_month` `this_year` `last_year`。

### SizeExpression
```json
{ "type": "greater_than", "value": 100, "unit": "MB" }
{ "type": "less_than",    "value": 1,   "unit": "GB" }
{ "type": "between",      "min": 10, "max": 100, "unit": "MB" }
```
unit：`B`/`KB`/`MB`/`GB`（duration 用 `s`/`m`/`h`）。

### SortOrder 枚举
`relevance_desc`（media 默认）`modified_desc`（file 默认）`modified_asc` `created_desc` `created_asc` `accessed_desc` `size_desc` `size_asc` `name_asc` `name_desc`。

### Location
`{ "hint": "下载" }` —— 只填 hint。

## 4. 约定表（约定轴查表依据）

| 字段 | 约定 |
|---|---|
| `schema_version` | 恒 `"1.0"` |
| `language` | 回显 case 语言 |
| file_search 未提排序 | `sort="modified_desc"` |
| media_search 未提排序 | `sort="relevance_desc"` |
| 含 size 约束、语义指向大文件 | `sort="size_desc"`；指向小文件 → `sort="size_asc"` |
| "最新/最近改的" 显式 | `sort="modified_desc"`；"最早" → `modified_asc" |
| Screenshot + 时间词 | 时间归 `created_time`（截图创建时间语义） |
| file_action 写操作 | `requires_confirmation=true` |
| refine | `base_ref="last_intent"` |
| 纯"文件"不指明类型 | 不填 file_type/extensions |
| 指明具体类型（word/ppt/图片…） | 填对应 file_type（必要时 extensions）|

## 5. 范例（各 variant 各一，照着这个精度标）

```json
[
  {
    "id": "v09-d0-zh-001",
    "query": "找下载目录里上周改过的 Excel 表格",
    "language": "zh",
    "expected_intent": {
      "intent": "file_search", "schema_version": "1.0", "language": "zh",
      "file_type": "spreadsheet",
      "location": { "hint": "下载" },
      "modified_time": { "type": "relative", "value": "last_week" },
      "sort": "modified_desc"
    }
  },
  {
    "id": "v09-d0-en-002",
    "query": "songs by Taylor Swift longer than 4 minutes",
    "language": "en",
    "expected_intent": {
      "intent": "media_search", "schema_version": "1.0", "language": "en",
      "media_type": "audio", "artist": "Taylor Swift",
      "duration": { "type": "greater_than", "value": 4, "unit": "m" },
      "sort": "relevance_desc"
    }
  },
  {
    "id": "v09-d0-zh-003",
    "query": "把第二个文件重命名为 报告终稿",
    "language": "zh",
    "expected_intent": {
      "intent": "file_action", "schema_version": "1.0", "language": "zh",
      "action": "rename",
      "target_ref": { "source": "last_results", "selector": { "type": "index", "value": 2 } },
      "new_name": "报告终稿",
      "requires_confirmation": true
    }
  },
  {
    "id": "v09-d0-zh-004",
    "query": "只保留 pdf 的",
    "language": "zh",
    "expected_intent": {
      "intent": "refine", "schema_version": "1.0", "language": "zh",
      "base_ref": "last_intent",
      "delta": { "extensions": ["pdf"] }
    }
  },
  {
    "id": "v09-d0-zh-005",
    "query": "昨天的",
    "language": "zh",
    "expected_intent": {
      "intent": "clarify", "schema_version": "1.0", "language": "zh",
      "reason": "ambiguous_type",
      "question": "你想找昨天的什么类型的文件？"
    }
  }
]
```

> 不确定某字段的确切枚举名时，**严格照本指南**；本指南没有的形态，不要臆造——换一条能确定标注的 query。
