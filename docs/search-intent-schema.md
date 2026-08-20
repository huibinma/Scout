# Search Intent JSON Schema v1.0

> 状态：**首版（已采纳 Codex 审阅意见修订）**，未实施。
> 维护：本文件由 [packages/intent-parser](../packages/intent-parser/) 与 [packages/search-backends/common](../packages/search-backends/common/) 共同遵循。任何字段变更必须升版本号并更新本文件。
> 本版变更摘要见本文件最后一节。

## 1. 设计目标与原则

### 1.1 角色

Search Intent JSON 是**模型 / 规则解析器**与**搜索后端**之间的统一中间层。

```text
自然语言输入
    ↓ （规则解析器 + 本地小模型）
Search Intent JSON ← 本文档
    ↓ （SearchBackend 适配层）
Spotlight 谓词 / Windows Search SQL / 内置原生索引查询 / 自建索引查询
```

### 1.2 原则

1. **模型不直接生成后端查询语法**。任何后端语法的拼装都在程序侧完成。
2. **跨平台中立**。Schema 不出现平台特定字段；具体后端如何映射是后端的事。
3. **语义优先**。时间用 `yesterday` / `last_7_days` 等语义值，不让模型算具体日期。
4. **可扩展可校验**。字段稳定可机器校验，可向后兼容地增加字段。
5. **职责清晰**。模型负责输出"意图"，程序负责把意图映射到"执行"。
6. **错误友好**。模型不确定时必须用 `clarify` intent 表达，而不是猜。

### 1.3 不放在 Intent 里的东西

- 后端查询语法（mdfind 表达式、SQL、内置原生索引查询语法）
- 具体日期边界（由程序按本地时区/locale 计算）
- 具体路径（除非用户明确说了；否则模型只给 `path_hint`，程序解析）
- 排序权重、向量检索参数（属于 Ranker 与 Indexer 的内部参数）

---

## 2. 顶层结构

每个 SearchIntent JSON 至少有：

```json
{
  "schema_version": "1.0",
  "intent": "file_search | media_search | file_action | refine | clarify",
  "language": "zh | en | mixed | unknown"
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `schema_version` | string | 是 | 本 schema 的语义版本号。首版 `"1.0"`。 |
| `intent` | enum | 是 | 五种之一，见 §3。 |
| `language` | enum | 否 | 模型识别到的用户输入语言。便于 Tracing 与训练数据回收。默认 `"unknown"`。 |

随 `intent` 不同，附加字段见 §3。

---

## 3. Intent 详解

### 3.1 `file_search`

通用文件搜索。最常用的 intent。

```json
{
  "schema_version": "1.0",
  "intent": "file_search",
  "language": "zh",
  "keywords": ["预算"],
  "extensions": ["ppt", "pptx"],
  "file_type": "presentation",
  "location": { "hint": null, "include": null, "exclude": null },
  "modified_time": { "type": "relative", "value": "yesterday" },
  "created_time": null,
  "accessed_time": null,
  "size": null,
  "sort": "modified_desc",
  "limit": 50
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `keywords` | string[] | 否 | 文件名 / 内容关键词。空数组表示"任意"。v1.0 默认作用域是"文件名 + 内容 + metadata 的宽匹配"（详见 [search-backend-trait.md §4.1](./search-backend-trait.md) 与 §8.2 v1.1 候选 `keyword_scope`）。 |
| `extensions` | string[] | 否 | 扩展名（不含点）。优先级高于 `file_type`。 |
| `file_type` | FileType（见 §4.4） | 否 | 高层文件类型；后端把它展开为扩展名集合。 |
| `location` | Location（见 §4.3） | 否 | 路径线索 / 约束。 |
| `modified_time` | TimeExpression（见 §4.1） | 否 | 修改时间。 |
| `created_time` | TimeExpression | 否 | 创建时间。 |
| `accessed_time` | TimeExpression | 否 | 最后访问时间。 |
| `size` | SizeExpression（见 §4.2） | 否 | 文件大小。 |
| `exclude_extensions` | string[] | 否 | 排除的扩展名。与 `extensions` 互斥地表达反向过滤。 |
| `exclude_file_type` | FileType[] | 否 | 排除的高层类型。 |
| `sort` | SortOrder（见 §4.5） | 否 | 默认 `"modified_desc"`。 |
| `limit` | int | 否 | 返回上限。默认 50，最大 500。 |

> `exclude_extensions` / `exclude_file_type` 是 v1.0 **通用字段**（由 Codex 审阅 must-fix #2 升级而来）：合并后的 intent 也可携带，不仅限于 refine 的临时层；backend 必须能处理这两个字段。

### 3.2 `media_search`

媒体专项搜索（音乐 / 图片 / 视频）。包含 `file_search` 的所有字段，外加媒体专有字段。

```json
{
  "schema_version": "1.0",
  "intent": "media_search",
  "language": "zh",
  "media_type": "audio",
  "artist": "周华健",
  "title": null,
  "album": null,
  "genre": null,
  "quality": null,
  "duration": null,
  "extensions": ["mp3", "flac", "wav", "m4a", "ape", "ogg"],
  "keywords": [],
  "location": null,
  "modified_time": null,
  "sort": "relevance_desc",
  "limit": 50
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `media_type` | enum: `audio` / `image` / `video` / `screenshot` | 是 | 媒体大类。 |
| `artist` | string | 否 | 音频：演唱者 / 作者。 |
| `title` | string | 否 | 音频 / 视频标题。 |
| `album` | string | 否 | 音频专辑。 |
| `genre` | string | 否 | 音频流派。 |
| `quality` | enum: `lossless` / `high` / `standard` / `low` | 否 | 音频质量。 |
| `duration` | SizeExpression（语义复用，单位为秒） | 否 | 时长。 |

`screenshot` 是 `image` 的特化，提示后端优先在 macOS 的"截屏"位置 / Windows 的"截图"目录搜索，或按文件名前缀启发式过滤。

### 3.3 `file_action`

对已选中文件执行操作。

```json
{
  "schema_version": "1.0",
  "intent": "file_action",
  "language": "zh",
  "action": "open | locate | copy | move | rename | delete",
  "target_ref": { ... },
  "destination": null,
  "new_name": null,
  "requires_confirmation": true
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `action` | enum | 是 | 见下方权限分级。 |
| `target_ref` | TargetRef（见 §4.6） | 是 | 目标文件的指代。 |
| `destination` | string \| null | `copy`/`move` 时必填 | 目标路径。 |
| `new_name` | string \| null | `rename` 时必填 | 新文件名。 |
| `requires_confirmation` | bool | 是 | 是否需要用户确认。所有写操作默认 `true`。 |

权限分级（与 [docs/local-personal-search-agent-project-plan.md §8.1 权限检查](./local-personal-search-agent-project-plan.md) 对应）：

| action | Level | 默认是否需要确认 |
|---|---|---|
| `open` | L3 | 轻确认或用户点击触发 |
| `locate`（在文件管理器中显示） | L1 | 否 |
| `copy` | L4 | 是 |
| `move` | L4 | 是 |
| `rename` | L4 | 是 |
| `delete` | L5 | **MVP 不开放**；开放后必须强确认 |

### 3.4 `refine`

基于上一轮结果的二次筛选。

```json
{
  "schema_version": "1.0",
  "intent": "refine",
  "language": "zh",
  "base_ref": "last_intent",
  "delta": {
    "extensions": ["pdf"]
  },
  "clear": null
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `base_ref` | enum: `last_intent` | 是 | 基准。v1.0 只支持基于最近一次 intent。 |
| `delta` | object | 是 | 部分字段集合，**覆盖**基准对应字段。允许字段是 `file_search` / `media_search` 字段（含 §3.1 的 `exclude_*`）。 |
| `clear` | string[] \| null | 否 | 字段路径列表，表示移除基准 intent 中这些字段的约束。允许值见下方"清空字段白名单"。 |

**v1.0 合并语义（必读 — 由 Codex 审阅 must-fix #2 确定的保守规则）**

1. **覆盖**：`delta` 中出现的任何字段（含 list 字段如 `extensions` / `keywords`）**完整覆盖**基准对应字段。例如基准 `extensions = ["doc","docx"]`，`delta.extensions = ["pdf"]`，合并后是 `["pdf"]`。
2. **追加**：v1.0 **不支持** "追加"原生语义。用户说"再加上 pdf"时，parser 负责把上下文合并成完整列表 `["doc","docx","pdf"]` 后整体覆盖。
3. **排除**：用户说"排除视频"时，向 `delta.exclude_file_type` 写入 `["video"]`（注意：`exclude_*` 是 §3.1 通用字段，合并后的 intent 也保留）。
4. **清空**：用户说"不限制下载目录了"时，使用 `clear` 字段：
   ```json
   { "intent":"refine", "base_ref":"last_intent", "clear":["location"] }
   ```
   不要用"`delta.location = null`"表达清空（JSON 中 null 与"未设置"难区分）。

**清空字段白名单（v1.0）**

`clear` 中允许出现的字段路径：

- `location`
- `extensions`
- `file_type`
- `keywords`
- `modified_time` / `created_time` / `accessed_time`
- `size`
- `exclude_extensions` / `exclude_file_type`
- `artist` / `title` / `album` / `genre` / `quality` / `duration`（仅 media_search 基准）

**不允许清空**：`schema_version` / `intent` / `language` / `sort` / `limit`（这些字段有默认值或本就是元数据）。

### 3.5 `clarify`

模型不确定 / 输入过于模糊时使用。

```json
{
  "schema_version": "1.0",
  "intent": "clarify",
  "language": "zh",
  "reason": "ambiguous_time",
  "question": "你说的「最近」是指最近几天？",
  "options": ["今天", "过去 3 天", "过去一周", "过去一个月"]
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `reason` | enum | 是 | `ambiguous_time` / `ambiguous_location` / `ambiguous_type` / `ambiguous_action` / `unsafe_action` / `unknown` |
| `question` | string | 是 | 给用户的问题文本。 |
| `options` | string[] | 否 | 建议选项；UI 可以渲染成快捷按钮。 |

**安全策略**：用户说"全部删掉"等高风险操作时，模型应输出 `clarify` 配 `reason: "unsafe_action"`，由 UI 做明确确认，不应直接生成 `file_action` 进入 Policy Engine。

#### Clarify 触发规则（v1.0 可执行规则）

由 Codex 审阅 must-fix #4 落地。Parser / 模型必须按以下规则决定是否触发 `clarify`，而不是凭感觉。

**强约束清单**（任一项存在即视为有强约束）：

- `keywords` 非空
- `extensions` 非空
- `file_type` 已指定
- `location.hint` 或 `location.include` 非空
- `media_type` 已指定（media_search）
- `artist` / `title` / `album` 任一非空（media_search）

**规则**

| 用户表达 | 强约束情况 | 输出 |
|---|---|---|
| "最近"、"recent" 等模糊时间词作为修饰 | 至少一个强约束存在 | **不 clarify**，使用 `sort: "modified_desc"`（或 `created_desc` / `accessed_desc` 取最贴近的） |
| "最近"作为唯一有效约束 | 无强约束 | `clarify(reason: "ambiguous_time")` |
| "最近几天 / 最近一段时间"显式问时间窗 | 任意 | `clarify(reason: "ambiguous_time")`，options 给若干常用窗口 |
| 位置词无法解析（如"项目归档"）且无其他强约束 | 无其他强约束 | `clarify(reason: "ambiguous_location")` |
| 位置词无法解析但有其他强约束 | 有强约束 | **不 clarify**，把位置词作为 `keywords` 参与搜索（详见 §4.3 注释） |
| 用户表达高风险写操作（删除、批量移动）但 `target_ref` 不明确 | — | `clarify(reason: "unsafe_action")`，必须展示目标列表 |
| 高风险操作 `target_ref.selector.type = "all"` 且基准结果超过阈值（建议 10） | — | `clarify(reason: "unsafe_action")` |

> 不要为这条规则引入新的"模糊时间"字段（如 `relative_recent`）—— 由 parser 在产出 intent 前做出抉择，让 backend 拿到的总是清晰约束。

---

## 4. 公共字段类型

### 4.1 TimeExpression

```json
{ "type": "relative", "value": "yesterday" }
{ "type": "absolute", "from": "2026-05-20", "to": "2026-05-24" }
{ "type": "before",   "value": "2026-05-20" }
{ "type": "after",    "value": "2026-05-20" }
```

`type` 枚举：

| type | 字段 | 说明 |
|---|---|---|
| `relative` | `value` | 见下方相对语义表 |
| `absolute` | `from`, `to` | 闭区间 `[from, to]`，ISO-8601 日期 |
| `before` | `value` | 严格早于该日期 |
| `after` | `value` | 严格晚于该日期 |

**相对语义**（`type: "relative"` 时 `value` 的取值）：

`today` / `yesterday` / `last_3_days` / `last_7_days` / `last_14_days` / `last_30_days` / `this_week` / `last_week` / `this_month` / `last_month` / `this_year` / `last_year`

> 模型只输出语义；具体边界由程序按用户系统时区和 locale 计算。`this_week` 的"周首"按系统 locale。

### 4.2 SizeExpression

```json
{ "type": "greater_than", "value": 100, "unit": "MB" }
{ "type": "less_than",    "value": 1,   "unit": "GB" }
{ "type": "between",      "min": 10, "max": 100, "unit": "MB" }
```

`unit` 枚举：`B` / `KB` / `MB` / `GB`（十进制 1000 进制；后端可在 metadata 中标注是否按 1024 进制存储）。

复用：`media_search.duration` 使用同一结构，`unit` 用 `s` / `m` / `h`。

### 4.3 Location

```json
{
  "hint": "下载",
  "include": null,
  "exclude": null
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `hint` | string \| null | 模型从自然语言提取的路径线索（自然语言），如 "下载"、"桌面"、"文稿"、"camera roll"。 |
| `include` | string[] \| null | 解析后的绝对路径数组；模型一般不直接输出，由程序把 hint 解析成此字段。 |
| `exclude` | string[] \| null | 排除路径。 |

**hint → include 解析规则**（由程序实现，跨平台）：

| hint | macOS | Windows |
|---|---|---|
| 下载 / downloads | `~/Downloads` | Known Folder `Downloads` |
| 桌面 / desktop | `~/Desktop` | Known Folder `Desktop` |
| 文稿 / documents | `~/Documents` | Known Folder `Documents` |
| 截屏 / screenshots | 优先读 `com.apple.screencapture` 的 `location` 偏好；失败 fallback 到 `~/Desktop` 与 `~/Pictures/Screenshots` | Known Folder `Screenshots` |
| 图片 / pictures | `~/Pictures` | Known Folder `Pictures` |
| 影片 / videos / movies | `~/Movies` | Known Folder `Videos` |
| 音乐 / music | `~/Music` | Known Folder `Music` |

**解析约定（由 Codex 审阅 should-have #9 落地）**

- **Windows 必须通过 Known Folders API 解析**（`SHGetKnownFolderPath` / WinRT `KnownFolders`），不要硬编码路径或中文显示名。中文版 Windows 资源管理器显示"桌面 / 下载 / 文档"，但实际路径仍是 `Desktop / Downloads / Documents`；用户也可能把这些目录移到非默认位置。
- **macOS 截屏目录**：读 `defaults read com.apple.screencapture location` 或等价 API；用户可通过系统设置或 `⌘⇧5` 改位置。Resolver 失败时再回退到 `~/Desktop` 与 `~/Pictures/Screenshots`。
- **`location.include` 一律由 resolver 生成**，模型不直接输出绝对路径；唯一例外是用户输入了明确的绝对路径，此时模型可以直接填 `include`。
- **未识别的 hint**：由 Clarify 触发规则（§3.5）决定 —— 有其他强约束就把 hint 当 keywords 兜底匹配；无其他强约束就 `clarify(reason: "ambiguous_location")`。

### 4.4 FileType

高层文件类型，由后端展开为扩展名集合。

| FileType | 包含扩展名（示例，可在配置中调整） |
|---|---|
| `document` | doc, docx, pdf, txt, md, html, rtf, pages, odt |
| `spreadsheet` | xls, xlsx, csv, numbers, ods |
| `presentation` | ppt, pptx, key, odp |
| `image` | jpg, jpeg, png, gif, heic, heif, webp, bmp, tiff, svg |
| `screenshot` | image 子集，按文件名 / 路径启发式 |
| `video` | mp4, mov, avi, mkv, webm, m4v, wmv, flv |
| `audio` | mp3, flac, wav, m4a, ape, ogg, aac, wma, aiff |
| `archive` | zip, rar, 7z, tar, gz, bz2, xz |
| `code` | py, js, ts, rs, go, java, c, cpp, h, hpp, swift, kt |
| `executable` | exe, msi, dmg, pkg, app, deb, rpm |

当 `file_type` 与 `extensions` 同时存在时，**`extensions` 优先**，`file_type` 视为对未指定字段的兜底提示。

### 4.5 SortOrder

枚举：

- `relevance_desc`（默认 media_search）
- `modified_desc`（默认 file_search）
- `modified_asc`
- `created_desc`
- `created_asc`
- `accessed_desc`
- `size_desc`
- `size_asc`
- `name_asc`
- `name_desc`

### 4.6 TargetRef

用于 `file_action.target_ref` 和未来扩展。

```json
{ "source": "last_results", "selector": { "type": "index",   "value": 3 } }
{ "source": "last_results", "selector": { "type": "indices", "values": [1, 3, 5] } }
{ "source": "last_results", "selector": { "type": "all" } }
{ "source": "path",          "value": "/Users/alice/Documents/budget.pdf" }
```

`source` 枚举：

- `last_results`：上一轮搜索结果（由 Context Memory 保存）
- `path`：直接指定绝对路径

`selector.type` 枚举（仅 `source = "last_results"` 时）：

- `index`：单个，1-based
- `indices`：多个，1-based 数组
- `all`：上一轮全部结果

> **v1.0 不支持 `filter` selector**（由 Codex 审阅 must-fix #3 落地）。
>
> `filter` 在写操作（copy/move/rename/delete）下过度灵活，容易让用户没看清目标集就批量执行。v1.0 的处理方式：用户说"把这些 pdf 复制到桌面"时，parser 先用 `refine` 把结果集筛成 pdf，再要求用户对结果列表做一次显式确认（UI 层），最后用 `selector.type = "all"` 触发 `file_action`。
>
> `filter` selector 移至 **v1.1** 并附加约束：(a) 必须列入字段白名单；(b) 必须限制最大命中数量；(c) UI 必须在执行前展示完整目标列表。

---

## 5. JSON Schema（机器可校验）

> 实际部署时拆为独立 `.json` 文件并随代码引用（`docs/schema/search-intent.v1.json`）。
>
> **本节已根据 Codex 审阅 must-fix #1 收紧**：所有对象显式 `"type": "object"`，默认 `"additionalProperties": false`，并对 `file_action` 加入条件校验。`refine.delta` 是唯一受控扩展点。

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "$id": "https://scout.dev/schema/search-intent/v1.json",
  "title": "Scout Search Intent",
  "type": "object",
  "required": ["schema_version", "intent"],
  "properties": {
    "schema_version": { "const": "1.0" },
    "intent": { "enum": ["file_search", "media_search", "file_action", "refine", "clarify"] },
    "language": { "enum": ["zh", "en", "mixed", "unknown"] }
  },
  "allOf": [
    { "if": { "properties": { "intent": { "const": "file_search" } } },
      "then": { "$ref": "#/$defs/FileSearch" } },
    { "if": { "properties": { "intent": { "const": "media_search" } } },
      "then": { "$ref": "#/$defs/MediaSearch" } },
    { "if": { "properties": { "intent": { "const": "file_action" } } },
      "then": { "$ref": "#/$defs/FileAction" } },
    { "if": { "properties": { "intent": { "const": "refine" } } },
      "then": { "$ref": "#/$defs/Refine" } },
    { "if": { "properties": { "intent": { "const": "clarify" } } },
      "then": { "$ref": "#/$defs/Clarify" } }
  ],

  "$defs": {

    "TimeExpression": {
      "oneOf": [
        { "type": "object", "additionalProperties": false,
          "required": ["type", "value"],
          "properties": {
            "type":  { "const": "relative" },
            "value": { "enum": ["today", "yesterday", "last_3_days", "last_7_days", "last_14_days", "last_30_days", "this_week", "last_week", "this_month", "last_month", "this_year", "last_year"] }
          } },
        { "type": "object", "additionalProperties": false,
          "required": ["type", "from", "to"],
          "properties": {
            "type": { "const": "absolute" },
            "from": { "type": "string", "format": "date" },
            "to":   { "type": "string", "format": "date" }
          } },
        { "type": "object", "additionalProperties": false,
          "required": ["type", "value"],
          "properties": {
            "type":  { "enum": ["before", "after"] },
            "value": { "type": "string", "format": "date" }
          } }
      ]
    },

    "SizeExpression": {
      "oneOf": [
        { "type": "object", "additionalProperties": false,
          "required": ["type", "value", "unit"],
          "properties": {
            "type":  { "enum": ["greater_than", "less_than"] },
            "value": { "type": "number", "exclusiveMinimum": 0 },
            "unit":  { "enum": ["B", "KB", "MB", "GB", "s", "m", "h"] }
          } },
        { "type": "object", "additionalProperties": false,
          "required": ["type", "min", "max", "unit"],
          "properties": {
            "type": { "const": "between" },
            "min":  { "type": "number", "minimum": 0 },
            "max":  { "type": "number", "exclusiveMinimum": 0 },
            "unit": { "enum": ["B", "KB", "MB", "GB", "s", "m", "h"] }
          } }
      ]
    },

    "Location": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "hint":    { "type": ["string", "null"] },
        "include": { "type": ["array", "null"], "items": { "type": "string" } },
        "exclude": { "type": ["array", "null"], "items": { "type": "string" } }
      }
    },

    "FileType": {
      "enum": ["document", "spreadsheet", "presentation", "image", "screenshot", "video", "audio", "archive", "code", "executable"]
    },

    "SortOrder": {
      "enum": ["relevance_desc", "modified_desc", "modified_asc", "created_desc", "created_asc", "accessed_desc", "size_desc", "size_asc", "name_asc", "name_desc"]
    },

    "TargetRef": {
      "oneOf": [
        { "type": "object", "additionalProperties": false,
          "required": ["source", "selector"],
          "properties": {
            "source":   { "const": "last_results" },
            "selector": { "$ref": "#/$defs/TargetSelector" }
          } },
        { "type": "object", "additionalProperties": false,
          "required": ["source", "value"],
          "properties": {
            "source": { "const": "path" },
            "value":  { "type": "string", "minLength": 1 }
          } }
      ]
    },

    "TargetSelector": {
      "oneOf": [
        { "type": "object", "additionalProperties": false,
          "required": ["type", "value"],
          "properties": {
            "type":  { "const": "index" },
            "value": { "type": "integer", "minimum": 1 }
          } },
        { "type": "object", "additionalProperties": false,
          "required": ["type", "values"],
          "properties": {
            "type":   { "const": "indices" },
            "values": { "type": "array", "items": { "type": "integer", "minimum": 1 }, "minItems": 1 }
          } },
        { "type": "object", "additionalProperties": false,
          "required": ["type"],
          "properties": {
            "type": { "const": "all" }
          } }
      ]
    },

    "FileSearch": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "schema_version":      { "const": "1.0" },
        "intent":              { "const": "file_search" },
        "language":            { "enum": ["zh", "en", "mixed", "unknown"] },
        "keywords":            { "type": "array", "items": { "type": "string" } },
        "extensions":          { "type": "array", "items": { "type": "string" } },
        "file_type":           { "$ref": "#/$defs/FileType" },
        "location":            { "$ref": "#/$defs/Location" },
        "modified_time":       { "$ref": "#/$defs/TimeExpression" },
        "created_time":        { "$ref": "#/$defs/TimeExpression" },
        "accessed_time":       { "$ref": "#/$defs/TimeExpression" },
        "size":                { "$ref": "#/$defs/SizeExpression" },
        "exclude_extensions":  { "type": "array", "items": { "type": "string" } },
        "exclude_file_type":   { "type": "array", "items": { "$ref": "#/$defs/FileType" } },
        "sort":                { "$ref": "#/$defs/SortOrder" },
        "limit":               { "type": "integer", "minimum": 1, "maximum": 500 }
      }
    },

    "MediaSearch": {
      "type": "object",
      "additionalProperties": false,
      "required": ["media_type"],
      "properties": {
        "schema_version":      { "const": "1.0" },
        "intent":              { "const": "media_search" },
        "language":            { "enum": ["zh", "en", "mixed", "unknown"] },
        "media_type":          { "enum": ["audio", "image", "video", "screenshot"] },
        "artist":              { "type": ["string", "null"] },
        "title":               { "type": ["string", "null"] },
        "album":               { "type": ["string", "null"] },
        "genre":               { "type": ["string", "null"] },
        "quality":             { "enum": ["lossless", "high", "standard", "low", null] },
        "duration":            { "$ref": "#/$defs/SizeExpression" },
        "keywords":            { "type": "array", "items": { "type": "string" } },
        "extensions":          { "type": "array", "items": { "type": "string" } },
        "file_type":           { "$ref": "#/$defs/FileType" },
        "location":            { "$ref": "#/$defs/Location" },
        "modified_time":       { "$ref": "#/$defs/TimeExpression" },
        "created_time":        { "$ref": "#/$defs/TimeExpression" },
        "accessed_time":       { "$ref": "#/$defs/TimeExpression" },
        "size":                { "$ref": "#/$defs/SizeExpression" },
        "exclude_extensions":  { "type": "array", "items": { "type": "string" } },
        "exclude_file_type":   { "type": "array", "items": { "$ref": "#/$defs/FileType" } },
        "sort":                { "$ref": "#/$defs/SortOrder" },
        "limit":               { "type": "integer", "minimum": 1, "maximum": 500 }
      }
    },

    "FileAction": {
      "type": "object",
      "additionalProperties": false,
      "required": ["action", "target_ref", "requires_confirmation"],
      "properties": {
        "schema_version":        { "const": "1.0" },
        "intent":                { "const": "file_action" },
        "language":              { "enum": ["zh", "en", "mixed", "unknown"] },
        "action":                { "enum": ["open", "locate", "copy", "move", "rename", "delete"] },
        "target_ref":            { "$ref": "#/$defs/TargetRef" },
        "destination":           { "type": ["string", "null"] },
        "new_name":              { "type": ["string", "null"] },
        "requires_confirmation": { "type": "boolean" }
      },
      "allOf": [
        { "if": { "properties": { "action": { "enum": ["copy", "move"] } }, "required": ["action"] },
          "then": { "required": ["destination"],
                    "properties": { "destination": { "type": "string", "minLength": 1 } } } },
        { "if": { "properties": { "action": { "const": "rename" } }, "required": ["action"] },
          "then": { "required": ["new_name"],
                    "properties": { "new_name": { "type": "string", "minLength": 1 } } } },
        { "if": { "properties": { "action": { "enum": ["copy", "move", "rename", "delete"] } }, "required": ["action"] },
          "then": { "properties": { "requires_confirmation": { "const": true } } } }
      ]
    },

    "Refine": {
      "type": "object",
      "additionalProperties": false,
      "required": ["base_ref", "delta"],
      "properties": {
        "schema_version": { "const": "1.0" },
        "intent":         { "const": "refine" },
        "language":       { "enum": ["zh", "en", "mixed", "unknown"] },
        "base_ref":       { "const": "last_intent" },
        "delta":          { "$ref": "#/$defs/RefineDelta" },
        "clear":          { "type": ["array", "null"],
                            "items": { "enum": ["location", "extensions", "file_type", "keywords", "modified_time", "created_time", "accessed_time", "size", "exclude_extensions", "exclude_file_type", "artist", "title", "album", "genre", "quality", "duration"] } }
      }
    },

    "RefineDelta": {
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "keywords":            { "type": "array", "items": { "type": "string" } },
        "extensions":          { "type": "array", "items": { "type": "string" } },
        "file_type":           { "$ref": "#/$defs/FileType" },
        "location":            { "$ref": "#/$defs/Location" },
        "modified_time":       { "$ref": "#/$defs/TimeExpression" },
        "created_time":        { "$ref": "#/$defs/TimeExpression" },
        "accessed_time":       { "$ref": "#/$defs/TimeExpression" },
        "size":                { "$ref": "#/$defs/SizeExpression" },
        "exclude_extensions":  { "type": "array", "items": { "type": "string" } },
        "exclude_file_type":   { "type": "array", "items": { "$ref": "#/$defs/FileType" } },
        "artist":              { "type": ["string", "null"] },
        "title":               { "type": ["string", "null"] },
        "album":               { "type": ["string", "null"] },
        "genre":               { "type": ["string", "null"] },
        "quality":             { "enum": ["lossless", "high", "standard", "low", null] },
        "duration":            { "$ref": "#/$defs/SizeExpression" },
        "sort":                { "$ref": "#/$defs/SortOrder" },
        "limit":               { "type": "integer", "minimum": 1, "maximum": 500 }
      }
    },

    "Clarify": {
      "type": "object",
      "additionalProperties": false,
      "required": ["reason", "question"],
      "properties": {
        "schema_version": { "const": "1.0" },
        "intent":         { "const": "clarify" },
        "language":       { "enum": ["zh", "en", "mixed", "unknown"] },
        "reason":         { "enum": ["ambiguous_time", "ambiguous_location", "ambiguous_type", "ambiguous_action", "unsafe_action", "unknown"] },
        "question":       { "type": "string", "minLength": 1 },
        "options":        { "type": "array", "items": { "type": "string" } }
      }
    }
  }
}
```

> **运行时附加约束**（schema 校验之外，由 Harness Policy Engine 强制）：
>
> - `file_action.action = "delete"`：MVP 必须拒绝（即使 schema 校验通过）。返回 `clarify(reason: "unsafe_action")` 或直接拒绝。
> - `file_action.target_ref.selector.type = "all"`：当基准结果数超过阈值（建议 10）时，必须改为 `clarify(reason: "unsafe_action")`。
> - `refine.clear` 与 `refine.delta` 中同名字段同时出现时：以 `clear` 为准（先清空再覆盖），并在 tracing 中记录冲突。

---

## 6. 版本管理

- 当前版本 **`1.0`**。
- **patch（1.0.x）**：仅修正注释/文档。
- **minor（1.x.0）**：向后兼容地新增字段、新增枚举值。
- **major（x.0.0）**：移除字段、改变字段语义、收紧枚举。模型与后端都要明确升级。
- Schema 文件命名：`docs/schema/search-intent.v1.json`（实际生效时落地）。
- 模型输出的 `schema_version` 与运行时支持版本不一致时，由 Harness 决定降级或重试，不静默忽略。

---

## 7. 验收用例（42 条）

每条用例附 expected SearchIntent JSON（精简，省略全 null 字段）。这套用例同时是 v1.0 schema 的**覆盖性验证**：如果某条无法用 schema 表达，必须补 schema 后再升版。

### 7.1 中文 — 文件搜索（11 条）

**1. 查找昨天编辑过的 ppt**
```json
{ "schema_version":"1.0","intent":"file_search","language":"zh",
  "extensions":["ppt","pptx"], "file_type":"presentation",
  "modified_time":{"type":"relative","value":"yesterday"},
  "sort":"modified_desc" }
```

**2. 找最近三天修改的 Excel**
```json
{ "schema_version":"1.0","intent":"file_search","language":"zh",
  "extensions":["xls","xlsx"], "file_type":"spreadsheet",
  "modified_time":{"type":"relative","value":"last_3_days"},
  "sort":"modified_desc" }
```

**3. 找下载目录中大于 100MB 的视频**
```json
{ "schema_version":"1.0","intent":"file_search","language":"zh",
  "file_type":"video",
  "location":{"hint":"下载"},
  "size":{"type":"greater_than","value":100,"unit":"MB"},
  "sort":"size_desc" }
```

**4. 找名字里有"预算"的文件**
```json
{ "schema_version":"1.0","intent":"file_search","language":"zh",
  "keywords":["预算"], "sort":"modified_desc" }
```

**5. 找桌面上的 word 文档**
```json
{ "schema_version":"1.0","intent":"file_search","language":"zh",
  "extensions":["doc","docx"], "file_type":"document",
  "location":{"hint":"桌面"},
  "sort":"modified_desc" }
```

**6. 找文稿目录里 2025 年的 ppt**
```json
{ "schema_version":"1.0","intent":"file_search","language":"zh",
  "extensions":["ppt","pptx"], "file_type":"presentation",
  "location":{"hint":"文稿"},
  "modified_time":{"type":"absolute","from":"2025-01-01","to":"2025-12-31"},
  "sort":"modified_desc" }
```

**7. 找名字以"会议纪要"开头的文档**
```json
{ "schema_version":"1.0","intent":"file_search","language":"zh",
  "keywords":["会议纪要"], "file_type":"document",
  "sort":"modified_desc" }
```

> **v1.0 已知有损表达**（Codex 审阅 should-have #7）：v1.0 的 `keywords` 是无序列表，**只承诺包含匹配**（substring），不承诺前缀匹配。"以…开头"的语义被降级为包含匹配。`keyword_scope` / `keyword_match_mode`（前缀 / 精确 / 模糊）在 v1.1 加入，详见 §8.2。

**8. 找过去一个月里大于 1GB 的视频**
```json
{ "schema_version":"1.0","intent":"file_search","language":"zh",
  "file_type":"video",
  "modified_time":{"type":"relative","value":"last_30_days"},
  "size":{"type":"greater_than","value":1,"unit":"GB"},
  "sort":"size_desc" }
```

**9. 找上周收到的 pdf（"收到"按 created_time 处理）**
```json
{ "schema_version":"1.0","intent":"file_search","language":"zh",
  "extensions":["pdf"], "file_type":"document",
  "created_time":{"type":"relative","value":"last_week"},
  "sort":"created_desc" }
```

**10. 找最近一周访问过的 markdown**
```json
{ "schema_version":"1.0","intent":"file_search","language":"zh",
  "extensions":["md"],
  "accessed_time":{"type":"relative","value":"last_7_days"},
  "sort":"accessed_desc" }
```

**11. 找 2026 年 5 月 1 日之前修改的 zip**
```json
{ "schema_version":"1.0","intent":"file_search","language":"zh",
  "extensions":["zip"], "file_type":"archive",
  "modified_time":{"type":"before","value":"2026-05-01"},
  "sort":"modified_desc" }
```

### 7.2 中文 — 媒体搜索（5 条）

**12. 找一首周华健的歌**
```json
{ "schema_version":"1.0","intent":"media_search","language":"zh",
  "media_type":"audio",
  "artist":"周华健",
  "extensions":["mp3","flac","wav","m4a","ape","ogg","aac","wma","aiff"],
  "sort":"relevance_desc" }
```

**13. 找周华健的朋友**
```json
{ "schema_version":"1.0","intent":"media_search","language":"zh",
  "media_type":"audio",
  "artist":"周华健", "title":"朋友",
  "sort":"relevance_desc" }
```

**14. 找上个月下载的周华健无损音乐**
```json
{ "schema_version":"1.0","intent":"media_search","language":"zh",
  "media_type":"audio",
  "artist":"周华健", "quality":"lossless",
  "created_time":{"type":"relative","value":"last_month"},
  "sort":"created_desc" }
```

**15. 找我昨天截的付款二维码**
```json
{ "schema_version":"1.0","intent":"media_search","language":"zh",
  "media_type":"screenshot",
  "keywords":["付款","二维码"],
  "created_time":{"type":"relative","value":"yesterday"},
  "sort":"created_desc" }
```

> 注：keywords 在 screenshot 上既匹配文件名又（Beta 起）匹配 OCR 文本。

**16. 找最近一个月超过 10 分钟的视频**
```json
{ "schema_version":"1.0","intent":"media_search","language":"zh",
  "media_type":"video",
  "duration":{"type":"greater_than","value":10,"unit":"m"},
  "modified_time":{"type":"relative","value":"last_30_days"},
  "sort":"modified_desc" }
```

### 7.3 英文 — 文件搜索（8 条）

**17. find ppt yesterday edited**
```json
{ "schema_version":"1.0","intent":"file_search","language":"en",
  "extensions":["ppt","pptx"], "file_type":"presentation",
  "modified_time":{"type":"relative","value":"yesterday"},
  "sort":"modified_desc" }
```

**18. find pdf modified last week**
```json
{ "schema_version":"1.0","intent":"file_search","language":"en",
  "extensions":["pdf"], "file_type":"document",
  "modified_time":{"type":"relative","value":"last_week"},
  "sort":"modified_desc" }
```

**19. find files over 100MB in downloads**
```json
{ "schema_version":"1.0","intent":"file_search","language":"en",
  "location":{"hint":"downloads"},
  "size":{"type":"greater_than","value":100,"unit":"MB"},
  "sort":"size_desc" }
```

**20. find screenshots from last month**
```json
{ "schema_version":"1.0","intent":"media_search","language":"en",
  "media_type":"screenshot",
  "created_time":{"type":"relative","value":"last_month"},
  "sort":"created_desc" }
```

**21. find images on desktop**
```json
{ "schema_version":"1.0","intent":"file_search","language":"en",
  "file_type":"image",
  "location":{"hint":"desktop"},
  "sort":"modified_desc" }
```

**22. find Excel modified in the past 7 days**
```json
{ "schema_version":"1.0","intent":"file_search","language":"en",
  "extensions":["xls","xlsx"], "file_type":"spreadsheet",
  "modified_time":{"type":"relative","value":"last_7_days"},
  "sort":"modified_desc" }
```

**23. find audio files by Eric Clapton**
```json
{ "schema_version":"1.0","intent":"media_search","language":"en",
  "media_type":"audio",
  "artist":"Eric Clapton",
  "sort":"relevance_desc" }
```

**24. find videos larger than 1 GB**
```json
{ "schema_version":"1.0","intent":"file_search","language":"en",
  "file_type":"video",
  "size":{"type":"greater_than","value":1,"unit":"GB"},
  "sort":"size_desc" }
```

### 7.4 中英混合（6 条）

**25. 找我 yesterday 改过的 ppt**
```json
{ "schema_version":"1.0","intent":"file_search","language":"mixed",
  "extensions":["ppt","pptx"], "file_type":"presentation",
  "modified_time":{"type":"relative","value":"yesterday"},
  "sort":"modified_desc" }
```

**26. 找 downloads 里的 mp4**
```json
{ "schema_version":"1.0","intent":"file_search","language":"mixed",
  "extensions":["mp4"], "file_type":"video",
  "location":{"hint":"downloads"},
  "sort":"modified_desc" }
```

**27. 找最近的 budget pptx**
```json
{ "schema_version":"1.0","intent":"file_search","language":"mixed",
  "keywords":["budget"], "extensions":["pptx"], "file_type":"presentation",
  "sort":"modified_desc" }
```

> 按 §3.5 Clarify 触发规则：有 `keywords` + `extensions` + `file_type` 三个强约束，"最近的"作为排序修饰 → **不 clarify**，使用 `modified_desc`。对比用例 #41（"最近的"为唯一约束 → clarify）。

**28. find 周华健 的歌**
```json
{ "schema_version":"1.0","intent":"media_search","language":"mixed",
  "media_type":"audio", "artist":"周华健",
  "sort":"relevance_desc" }
```

**29. find 下载目录 里的大文件**
```json
{ "schema_version":"1.0","intent":"file_search","language":"mixed",
  "location":{"hint":"下载"},
  "size":{"type":"greater_than","value":100,"unit":"MB"},
  "sort":"size_desc" }
```

> 注："大文件"由模型映射为 `>100MB`（合理默认）。规则解析器可配置阈值。

**30. show me 上周的 PDF**
```json
{ "schema_version":"1.0","intent":"file_search","language":"mixed",
  "extensions":["pdf"], "file_type":"document",
  "modified_time":{"type":"relative","value":"last_week"},
  "sort":"modified_desc" }
```

### 7.5 多轮上下文 — refine（5 条）

> 前置：上一轮已执行 `#1 查找昨天编辑过的 ppt`，返回了若干结果。

**31. 只看下载目录里的**
```json
{ "schema_version":"1.0","intent":"refine","language":"zh",
  "base_ref":"last_intent",
  "delta":{ "location":{"hint":"下载"} } }
```

**32. 只看 pdf**
```json
{ "schema_version":"1.0","intent":"refine","language":"zh",
  "base_ref":"last_intent",
  "delta":{ "extensions":["pdf"], "file_type":"document" } }
```

**33. 排除视频**
```json
{ "schema_version":"1.0","intent":"refine","language":"zh",
  "base_ref":"last_intent",
  "delta":{ "exclude_file_type":["video"] } }
```

**34. show only the pdf ones**
```json
{ "schema_version":"1.0","intent":"refine","language":"en",
  "base_ref":"last_intent",
  "delta":{ "extensions":["pdf"], "file_type":"document" } }
```

**35. 按大小倒序**
```json
{ "schema_version":"1.0","intent":"refine","language":"zh",
  "base_ref":"last_intent",
  "delta":{ "sort":"size_desc" } }
```

### 7.6 文件操作（5 条）

> 前置：上一轮已展示若干结果。

**36. 打开第三个**
```json
{ "schema_version":"1.0","intent":"file_action","language":"zh",
  "action":"open",
  "target_ref":{"source":"last_results","selector":{"type":"index","value":3}},
  "requires_confirmation":false }
```

**37. open the third one**
```json
{ "schema_version":"1.0","intent":"file_action","language":"en",
  "action":"open",
  "target_ref":{"source":"last_results","selector":{"type":"index","value":3}},
  "requires_confirmation":false }
```

**38. 在访达里显示第一个 / show the first one in Finder**
```json
{ "schema_version":"1.0","intent":"file_action","language":"zh",
  "action":"locate",
  "target_ref":{"source":"last_results","selector":{"type":"index","value":1}},
  "requires_confirmation":false }
```

**39. 把这些 pdf 复制到桌面**（v1.0 两阶段流程，由 Codex must-fix #3 落地）

第一阶段 — parser 把"这些 pdf"翻译为 refine，先缩小结果集：

```json
{ "schema_version":"1.0","intent":"refine","language":"zh",
  "base_ref":"last_intent",
  "delta":{ "extensions":["pdf"], "file_type":"document" } }
```

UI 展示筛选后结果，由用户显式确认目标列表后，再发出第二阶段：

```json
{ "schema_version":"1.0","intent":"file_action","language":"zh",
  "action":"copy",
  "target_ref":{
    "source":"last_results",
    "selector":{"type":"all"}
  },
  "destination":"~/Desktop",
  "requires_confirmation":true }
```

> v1.0 **禁止** `target_ref.selector.type = "filter"` 直接用于写操作（见 §4.6）。`filter` selector 移至 v1.1。

**40. 把第三个改名为 final**
```json
{ "schema_version":"1.0","intent":"file_action","language":"zh",
  "action":"rename",
  "target_ref":{"source":"last_results","selector":{"type":"index","value":3}},
  "new_name":"final",
  "requires_confirmation":true }
```

### 7.7 边界 / 澄清（2 条）

**41. 找最近的（模糊）**
```json
{ "schema_version":"1.0","intent":"clarify","language":"zh",
  "reason":"ambiguous_time",
  "question":"你说的「最近」是指最近几天？",
  "options":["今天","过去 3 天","过去一周","过去一个月"] }
```

> 按 §3.5 Clarify 触发规则：无 keywords / extensions / file_type / location / media_type 任一强约束，"最近的"为唯一约束 → 触发 `ambiguous_time`。

**42. 全部删掉（高风险）**
```json
{ "schema_version":"1.0","intent":"clarify","language":"zh",
  "reason":"unsafe_action",
  "question":"删除操作会移到回收站，且 MVP 暂不支持。是否改为在访达 / 资源管理器中显示，由你手动操作？",
  "options":["在访达/资源管理器中显示","取消"] }
```

### 7.8 Corner cases — Codex 审阅补充（43-47）

**43. 清空上一轮位置约束**（由 Codex 提出，验证 `refine.clear` 设计）

前置：上一轮 `location.hint = "下载"`。

用户：`不限制下载目录了`

```json
{ "schema_version":"1.0","intent":"refine","language":"zh",
  "base_ref":"last_intent",
  "delta":{},
  "clear":["location"] }
```

> 由 must-fix #2 引入的 `clear` 字段表达"移除约束"，避免 `null` 与"未设置"难区分的歧义。

**44. 英文复数扩展名 + 大小写混合**

用户：`find JPG and PNG screenshots from yesterday`

```json
{ "schema_version":"1.0","intent":"media_search","language":"en",
  "media_type":"screenshot",
  "extensions":["jpg","png"],
  "created_time":{"type":"relative","value":"yesterday"},
  "sort":"created_desc" }
```

> Parser 必须把 `JPG` / `PNG` 标准化为小写。Backend 收到的扩展名一律 lowercase。

**45. 排除某类文件**（验证 `exclude_*` 作为通用字段而非仅 refine 临时层）

前置：上一轮查找下载目录的大文件。

用户：`排除压缩包`

```json
{ "schema_version":"1.0","intent":"refine","language":"zh",
  "base_ref":"last_intent",
  "delta":{ "exclude_file_type":["archive"] } }
```

合并后的 file_search intent（由 Harness Context Memory 生成）：

```json
{ "schema_version":"1.0","intent":"file_search","language":"zh",
  "location":{"hint":"下载"},
  "size":{"type":"greater_than","value":100,"unit":"MB"},
  "exclude_file_type":["archive"],
  "sort":"size_desc" }
```

> Backend 必须能处理 `exclude_extensions` / `exclude_file_type`，不仅在 refine 临时层。

**46. 路径 hint 未识别**（验证 §4.3 fallback 与 §3.5 clarify 边界）

用户：`找项目归档里的 budget pdf`

模型输出（有其他强约束，hint 未知）：

```json
{ "schema_version":"1.0","intent":"file_search","language":"zh",
  "keywords":["budget"],
  "extensions":["pdf"],
  "file_type":"document",
  "location":{"hint":"项目归档"},
  "sort":"modified_desc" }
```

> Resolver 找不到"项目归档"对应的目录时，按 §4.3 规则：因为有 `keywords` + `extensions` + `file_type` 强约束 → **不 clarify**，把"项目归档"作为 keywords 兜底匹配（合并到 `keywords` 数组或单独的 hint 关键词通道，由 backend 决定）。

对比情境：若用户只说 `找项目归档里的文件`（无其他强约束）：

```json
{ "schema_version":"1.0","intent":"clarify","language":"zh",
  "reason":"ambiguous_location",
  "question":"没找到名为「项目归档」的目录。要不要在哪个范围内搜索？",
  "options":["全盘搜索","下载","文稿","桌面","取消"] }
```

**47. 高风险批量操作（target_ref 不明确）**

用户：`把这些都移动到桌面`

```json
{ "schema_version":"1.0","intent":"clarify","language":"zh",
  "reason":"ambiguous_action",
  "question":"要移动上一轮的全部结果吗？请先确认目标文件列表。",
  "options":["确认全部","只选择部分","取消"] }
```

> `move` 是 L4 写操作。即使 schema 允许 `selector.type = "all"`，parser 也不应在没有展示目标列表前直接生成 `file_action`。这条规则在 §3.5 Clarify 触发规则中已编码（"selector.type = all 且结果数超阈值 → clarify"）。

---

## 8. 验收结论与开放问题

### 8.1 覆盖性自检

47 条用例（42 首版 + 5 Codex 补充 corner case）**全部**可由本版 schema 表达（含本次根据 Codex must-fix 加入的 `clear` / `exclude_*` / `clarify` 触发规则）。未发现致命缺口。

### 8.2 已识别的 v1.1 候选改进

随后续迭代可能加入（已采纳 Codex 审阅 should-have / nice-to-have 中的建议）：

- **`keyword_scope`**（替代原 `keyword_match_mode`）：`filename | content | metadata | ocr | any`。比 `keyword_match_mode` 更可扩展，可与每个 keyword 关联作用域。用例 #7 与 #46 触发；详见 Codex 审阅 should-have #7 / nice-to-have #13。
- **`relative_duration`**（替代原 `relative_recent`）：`{ type: "relative_duration", value: 10, unit: "day" | "hour" }`，覆盖"最近 10 天"、"过去 2 小时"。`relative_recent` 已被 Codex 否决（让 parser 在产出前做判断，不引入新模糊字段）。
- **`target_ref.selector.type = "filter"`**：v1.0 已禁止用于写操作；v1.1 重新引入，但附加：字段白名单、最大命中数量限制、UI 强制展示目标列表。
- **配置化"大文件"阈值**：让规则解析器把"大文件"映射为可配置阈值（用例 #29）。
- **媒体 `quality` 启发式 → 真实识别**：v1.0 仅由扩展名启发（flac/wav/aiff/ape → lossless）。MVP 后由音频 metadata reader / 索引器升级到真实质量识别。Codex nice-to-have #15。

**已否决（不进入 v1.0/v1.1）**

- ~~`relative_recent` + `confidence`~~：让 parser 在 §3.5 Clarify 触发规则中做硬抉择，不引入 backend 需要处理的模糊时间字段。
- ~~`person` 字段~~：超出"本地文件自然语言搜索"原型范围，等到数据源覆盖联系人 / 邮件 / 聊天记录再设计。

### 8.3 验证 schema 是否够用的下一步

进入实施阶段时：

1. 把本 schema 落地为 `docs/schema/search-intent.v1.json`，纳入 packages 引用。
2. 在 [packages/intent-parser](../packages/intent-parser/) 实现规则解析器，覆盖以上 47 条用例（首版可仅覆盖 §7.1–§7.4 共 30 条静态查询；refine / file_action / clarify / corner cases 留到 multi-turn 上下文具备后）。
3. 在 [packages/evals](../packages/evals/) 把本节用例落库为 v0.1 评测集。
4. 模型 prompt 调试时以本 schema 为唯一输出契约，prompt 中包含简化版 schema 与 5-10 条 few-shot。
5. **§3.5 Clarify 触发规则必须作为 parser 单元测试**（特别是 #27 vs #41 vs #46 三种情境的边界）。
6. **Refine 合并语义必须作为 Harness Context Memory 的契约测试**（覆盖 / clear / 排除 三类）。

---

## 9. v1.0 Codex 审阅修订摘要

### must-fix（已全部修订）

| # | 修订点 | 落地位置 |
|---|---|---|
| 1 | JSON Schema 收紧：所有对象显式 `type: "object"`、默认 `additionalProperties: false`、`file_action` 条件校验、`delete` 运行时拒绝 | §5 整段重写，并追加运行时附加约束 |
| 2 | `refine.delta` 列表语义保守化：默认覆盖、`clear` 字段表达清空、`exclude_*` 提升为通用 file_search/media_search 字段 | §3.1 字段表追加 `exclude_*`；§3.4 重写合并语义；§5 RefineDelta 与 clear 白名单 |
| 3 | `target_ref.selector.type = "filter"` 移至 v1.1；v1.0 写操作仅支持 index/indices/all | §4.6 移除 filter；§7.6 #39 改两阶段示例；§8.2 v1.1 候选 |
| 4 | `clarify` 触发边界写成可执行规则 | §3.5 新增"Clarify 触发规则"表 |
| 5 | Spotlight 翻译规则的 keywords / cd 修饰符 / 扩展名谓词 / shell 注入防护 | 落到 [search-backend-trait.md §4](./search-backend-trait.md) §9 修订摘要 |
| 6 | Stub backend 命名 / 编译控制 / 测试断言 | 落到 [search-backend-trait.md §5](./search-backend-trait.md) §9 修订摘要 |

### should-have（已全部修订）

| # | 修订点 | 落地位置 |
|---|---|---|
| 7 | 用例 #7 注释改硬：v1.0 只保证包含匹配 | §7.1 #7 |
| 8 | `relative_duration` 设计预览，否决 `relative_recent` | §8.2 |
| 9 | Location hint 解析补 Known Folders API / macOS 截屏配置 / resolver 失败处理 | §4.3 |
| 10 | 错误码加 `UnsupportedIntent` | 落到 [search-backend-trait.md §2.3](./search-backend-trait.md) |
| 11 | 同步 trait 超时策略：超时即 `Err(Timeout)`，不返回部分结果 | 落到 [search-backend-trait.md §3](./search-backend-trait.md) |
| 12 | `SearchResult.id` 稳定性级别说明 | 落到 [search-backend-trait.md §2.2](./search-backend-trait.md) |

### nice-to-have（明确推迟）

- #13 `keyword_scope`：§8.2 已记入 v1.1 候选。
- #14 `person` 字段：§8.2 已明确否决。
- #15 媒体 `quality` 启发式 → 真实识别：§8.2 已记入 MVP 后改进。

### out-of-scope（明确不做）

- #16 v1.0 不做 async / streaming trait：保留同步 + Vec。
- #17 v1.0 不做完整 Capability Discovery：原型期静态后端顺序 + `is_available()`。

### Corner cases（已全部纳入 §7.8 用例 43-47）

| 原 ID | 主题 | 新用例编号 |
|---|---|---|
| A | 清空上一轮位置约束 | #43 |
| B | 英文复数扩展名 + 大小写混合 | #44 |
| C | 排除某类文件（验证 exclude_* 通用化） | #45 |
| D | 路径 hint 未识别 | #46 |
| E | 高风险批量操作 target_ref 不明确 | #47 |
