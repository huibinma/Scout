# 本地个人搜索 Agent 项目计划书

## 1. 项目概述

本项目目标是开发一个跨平台（macOS 与 Windows）的本地个人搜索 Agent。它将系统自带搜索（macOS Spotlight、Windows Search）、可选的 Everything 加速后端、本地小模型的自然语言理解能力、以及可控的 Agent Harness 工程底座结合起来，让用户无需理解 Spotlight 操作符、Everything 通配符或其他高级搜索语法，也能用自然语言快速查找本机文件、音乐、文档、截图、图片和其他个人资料。

产品初始定位：

> Deep Local Search：一个本地、轻量、跨平台、可扩展的个人搜索 Agent。

第一阶段不重做完整搜索引擎，而是在系统搜索（Spotlight / Windows Search）与可选的 Everything 加速后端前面封装一层”人话翻译层”和 Agent Harness。系统搜索是默认后端，零安装、跨平台覆盖；Everything 作为 Windows 上可选的高速后端，在用户已安装时自动接入。后续逐步扩展音乐 metadata、Office/PDF 内容索引、OCR、向量检索、浏览器历史、邮件、聊天记录、本地活动洞察等本地个人信息源。

核心原则：

- 本地优先：默认不上传用户文件名、路径、内容、搜索词和索引数据。
- 轻量可用：普通 16GB 内存 Mac 或 Windows 电脑可以运行。
- 跨平台一致：macOS 与 Windows 共享同一份 Agent Harness、Search Intent JSON、UI 和模型。
- 后端可插拔：系统搜索是默认后端，Everything 是 Windows 上的加速后端，未来可扩展更多。
- 可解释可控：Agent 每一步工具调用、权限判断、错误状态可追踪。
- 渐进扩展：先做好系统搜索的自然语言前端，再发展为完整本地个人搜索 Agent。

## 2. 目标用户与痛点

目标用户：

- macOS 普通用户（Spotlight 用户）
- Windows 普通用户（Windows Search 用户）
- Everything 用户
- 文件较多但不擅长高级搜索语法的办公用户
- 本地资料、音乐、图片、文档较多的个人用户
- 重视隐私、不希望把个人文件上传给云端 AI 的用户

核心痛点：

- macOS Spotlight 对自然语言、复杂条件支持有限，第三方文件搜索工具生态相对薄弱。
- Windows 自带搜索速度和可控性不足，索引覆盖范围有限。
- Everything 很快，但只在 Windows 上可用，且高级语法、通配符、日期条件、组合条件不够普通用户友好。
- 云端 AI 搜索涉及隐私、成本和网络依赖。
- 本地文件、音乐、截图、PDF、Office 文档分散在多个目录，缺少统一自然语言入口。
- 现有 AI 搜索产品多为重索引方案，资源占用和产品复杂度较高，且很少同时覆盖 macOS 与 Windows。

典型用户输入：

```text
查找昨天编辑过的 ppt
find ppt yesterday edited
找下载目录里最近一周的大文件
找一首周华健的歌
找上周客户会议里提到续约的 PDF
找我昨天截的付款二维码
刚才那些结果里只看 PPT
打开第三个
```

## 3. 产品范围

### 3.1 MVP 范围

MVP 聚焦跨平台系统搜索的自然语言前端和基础 Harness：

- 中文、英文、中英混合自然语言输入
- 文件类型、时间、大小、路径、关键词解析
- 规则解析 + 本地小模型兜底
- 输出统一 Search Intent JSON
- 通过统一的 SearchBackend 抽象将 Search Intent 转成各后端查询：
  - macOS：`mdfind` / Spotlight `kMDItem*` 查询
  - Windows：Windows Search `SystemIndex` SQL（OLE DB / Microsoft.Search）
  - Windows + Everything 已安装：自动切换到 Everything 后端（ES CLI 或 SDK）以获得毫秒级响应
- 结果列表展示
- 多轮上下文，例如”只看昨天的””排除视频””打开第三个”
- Tool registry
- Schema 校验
- 权限检查
- 最大步数和超时控制
- 基础错误分类
- 基础 tracing
- Golden evals（macOS 与 Windows 两个平台分别校验）

MVP 示例能力：

```text
找昨天编辑过的 ppt
查找最近三天修改的 Excel
find pdf modified last week
找下载目录中大于 100MB 的视频
找名字里有预算的文件
找一首周华健的歌
```

其中“找一首周华健的歌”在 MVP 可先基于文件名和基础音频扩展名搜索，后续加入音频 metadata 索引。

### 3.2 Beta 范围

Beta 加入多源本地索引：

- 音乐 metadata 索引：artist、title、album、duration、format
- Office/PDF 内容索引
- 图片和截图 OCR
- 结果归一化与排序
- 多来源结果合并
- Streaming 搜索和索引进度
- Audit log
- 后台索引调度
- 模型 LoRA 微调和量化部署
- 500-1000 条固定评测集
- Windows 安装包和自动更新

### 3.3 产品级 1.0 范围

1.0 面向普通用户稳定发布：

- 插件式连接器
- Everything / metadata / full-text / OCR / vector hybrid retrieval
- 本地活动洞察：最近打开文档类型、常用文件、工作时间分布、工作主题摘要
- 本地模型管理和升级
- 权限策略 UI
- 隐私边界和索引管理 UI
- 大文件库性能优化
- 崩溃恢复
- 多语言扩展
- 企业/个人权限模式
- 完整安装、卸载、升级流程

## 4. 核心架构

推荐整体架构：

```text
User Input
  ↓
Agent Harness
  ├─ Context Memory
  ├─ Intent Router
  ├─ Tool Loop Controller
  ├─ Step Limit / Timeout
  ├─ Policy Engine
  ├─ Schema Validator
  ├─ Hooks / Tracing
  └─ Evals Recorder
  ↓
Planner / Local Small Model
  ↓
Search Intent JSON
  ↓
Tool Registry
  ├─ Search Backend Adapter
  │   ├─ Spotlight Backend (mdfind / NSMetadataQuery)         [macOS 默认]
  │   ├─ Windows Search Backend (SystemIndex / OLE DB)        [Windows 默认]
  │   ├─ Everything Backend (ES / SDK)                        [Windows 可选加速]
  │   └─ Native Index Backend                                 [未来]
  ├─ File Metadata Tool
  ├─ Music Metadata Tool
  ├─ OCR Tool
  ├─ Office/PDF Text Index Tool
  ├─ Vector Search Tool
  ├─ Local Activity Insights Tool
  └─ File Action Tool
  ↓
Result Normalizer + Ranker
  ↓
Streaming Results UI
```

关键设计：

- 模型不直接生成任何后端的查询语法。
- 模型只输出统一 Search Intent JSON。
- 程序通过 SearchBackend 抽象，将 Search Intent JSON 转成对应后端的查询：Spotlight `kMDItem*` 表达式、Windows Search SQL、Everything 查询、全文检索查询或向量检索查询。
- SearchBackend 在启动时由 Capability Discovery 决定优先级，运行时可由用户在设置中覆盖。
- 所有工具调用必须经过 Tool registry、Schema 校验和权限检查。

## 5. Search Intent JSON 设计

第一版 Search Intent JSON 示例：

```json
{
  "intent": "file_search",
  "domain": "general",
  "keywords": ["预算"],
  "extensions": ["ppt", "pptx"],
  "file_type": "presentation",
  "path_hint": null,
  "modified_time": {
    "type": "relative",
    "value": "yesterday"
  },
  "created_time": null,
  "accessed_time": null,
  "size": null,
  "sort": "modified_desc",
  "limit": 50
}
```

音乐搜索示例：

```json
{
  "intent": "media_search",
  "domain": "music",
  "media_type": "audio",
  "artist": "周华健",
  "title": null,
  "album": null,
  "keywords": [],
  "extensions": ["mp3", "flac", "wav", "m4a", "ape", "ogg"],
  "modified_time": null,
  "quality": null,
  "sort": "relevance_desc",
  "limit": 50
}
```

文件操作示例：

```json
{
  "intent": "file_action",
  "action": "open",
  "target_ref": {
    "source": "last_results",
    "index": 3
  },
  "requires_confirmation": true
}
```

日期解析原则：

- 模型输出语义，例如 `yesterday`、`last_7_days`、`last_week`。
- 本地程序根据系统时区和 locale 计算具体日期范围。
- 避免让模型直接生成具体时间边界，减少错误。

## 6. 搜索后端策略

### 6.1 总体原则

Scout 不绑死任何单一搜索后端。SearchBackend 是一个抽象适配层，由 Capability Discovery 在启动时探测平台和环境，按以下默认优先级选择：

```text
macOS:
  Spotlight (mdfind) → Native Index (future)

Windows:
  Everything (如已安装) → Windows Search (SystemIndex) → Native Index (future)
```

设计要点：

- 系统搜索（Spotlight / Windows Search）是兜底默认值，保证零安装、跨平台覆盖。
- Everything 仅在 Windows 已检测到时作为加速后端启用。
- 用户可在设置中强制指定后端或禁用某个后端（例如企业环境禁用 Everything）。
- 后端不可用、超时、权限不足时按 Fallback Chain 自动降级，并返回明确的错误码（见 §8.1）。
- 后端层只负责"接收 Search Intent + 返回归一化结果"，模型和上层 UI 不感知具体后端。

### 6.2 macOS：Spotlight 集成

调用方式优先级：

1. `mdfind` 命令行：进程调用简单、无需特殊权限、Apple 长期支持。
2. `NSMetadataQuery`（Swift / Objective-C）：流式结果、原生集成、可订阅更新。
3. Core Spotlight（仅在自建索引时使用，MVP 不需要）。

MVP 选用 `mdfind`，Beta 评估切换到 `NSMetadataQuery` 以获得流式结果。

Spotlight 查询语法基础：

```text
mdfind -onlyin <dir> "<query>"
```

`<query>` 可以是裸关键词，也可以是 `kMDItem*` 谓词表达式。

Search Intent → Spotlight 查询示例：

用户输入：

```text
查找昨天编辑过的 ppt
```

Search Intent：

```json
{
  "extensions": ["ppt", "pptx"],
  "modified_time": {
    "type": "relative",
    "value": "yesterday"
  }
}
```

Spotlight 查询（由程序生成）：

```text
mdfind "(kMDItemFSName == '*.ppt'cd || kMDItemFSName == '*.pptx'cd) && \
        kMDItemContentModificationDate >= $time.today(-1) && \
        kMDItemContentModificationDate <  $time.today(0)"
```

注意事项：

- Spotlight 不索引用户在"系统设置 → Spotlight → 隐私"中排除的目录，需要在 UI 中告知用户。
- macOS 14+ 对完整磁盘访问（Full Disk Access）有更严格的限制，应用打开时若需访问受保护目录，需要引导用户授权。
- `mdfind` 默认搜索整机，可用 `-onlyin` 限制路径。
- Spotlight 对中文文件名搜索良好，但对中文正文 token 切分有时不够稳。

### 6.3 Windows：Windows Search（SystemIndex）集成

调用方式优先级：

1. OLE DB Provider for Microsoft Windows Search：通过 SQL 查询 `SystemIndex`。
2. `Windows.Storage.Search`（WinRT）：现代 API，适合 WinUI/UWP。
3. `Search-MgFile` / PowerShell 包装：原型阶段可用，正式版应走 OLE DB 或 WinRT。

MVP 选用 OLE DB + Search SQL（语法稳定、跨语言、可在 Rust/C#/Node 通过 ADO 调用）。

Search Intent → Windows Search SQL 示例：

```sql
SELECT System.ItemPathDisplay, System.ItemName, System.DateModified, System.Size
  FROM SystemIndex
 WHERE (System.FileExtension = '.ppt' OR System.FileExtension = '.pptx')
   AND System.DateModified >= 'yesterday'
   AND System.DateModified <  'today'
 ORDER BY System.DateModified DESC
```

注意事项：

- Windows Search 默认只索引"已建立索引的位置"（用户配置文件、邮件等），其他目录需要用户加入索引。Scout 应在首次启动时提示并提供一键加入入口。
- 索引服务（Windows Search Service）可能被企业策略禁用，此时直接降级到 Everything 或提示用户。
- SystemIndex SQL 不支持 `LIMIT`，需要在结果端截断。
- 对文件名前缀模糊匹配性能尚可，对全盘文件名扫描不如 Everything。

### 6.4 Windows 加速：Everything 集成

Everything 集成优先级：

1. 启动时检测用户是否已安装 Everything。
2. 检测 Everything 服务是否运行，IPC/SDK 是否可用。
3. 已安装且可用：自动将 Everything 设为 Windows 默认后端。
4. 未安装：保留 Windows Search 默认后端，并在设置页提供一句话引导，链接到 voidtools 官方下载（不强制、不内嵌广告）。
5. 如需随产品分发 ES 或 SDK，必须包含第三方许可声明（见 IP 计划书 §7.2）。

Search Intent → Everything 查询示例：

```text
ext:ppt;pptx dm:yesterday
```

不论用户输入 `查找昨天编辑过的 ppt` 还是 `find ppt yesterday edited`，模型生成的 Search Intent 相同，最终 Everything 查询也相同。

注意事项：

- Everything 是兼容能力，不是产品主体（见风险清单 §3.1）。
- 默认不分发 Everything 二进制；如分发 ES portable，必须列出 voidtools 与 PCRE 许可。
- Everything 服务可能未启动、未授权、版本过旧，必须有完整错误分类（`EVERYTHING_NOT_INSTALLED` / `EVERYTHING_NOT_RUNNING` / `EVERYTHING_VERSION_TOO_OLD`）。

### 6.5 后端能力对比

| 维度 | Spotlight (mac) | Windows Search | Everything (Win) |
|---|---|---|---|
| 安装要求 | 系统自带 | 系统自带 | 需用户安装 |
| 文件名搜索速度 | 中 | 中 | 极快（毫秒级） |
| 正文搜索 | 支持 | 支持 | 不支持 |
| 索引范围 | 默认全盘（除排除目录） | 默认部分目录 | 全盘 MFT |
| metadata 字段 | 丰富（kMDItem*） | 丰富（System.*） | 仅文件名/路径/时间/大小 |
| 中文支持 | 良好 | 一般 | 良好 |
| 调用方式 | mdfind / NSMetadataQuery | OLE DB / WinRT | ES CLI / SDK |
| 流式结果 | 支持（NSMetadataQuery） | 有限 | 支持 |
| 取消正在执行的查询 | 支持 | 有限 | 支持 |
| 适合场景 | macOS 默认 | Windows 默认（无 Everything 时） | Windows 文件名快速定位 |

## 7. 本地模型方案

### 7.1 模型选择

推荐第一版基座：

```text
Qwen2.5-1.5B-Instruct
```

选择原因：

- 体积小，适合本地部署。
- 中文和英文能力均较好。
- JSON/结构化输出能力较成熟。
- Apache 2.0 授权，适合商业产品进一步评估。
- 4-bit 量化后适合普通 16GB Windows 电脑运行。

备选：

```text
Qwen3-1.7B
```

适合后续对比复杂中文表达、Agent 能力和多语言理解能力。

### 7.2 训练策略

推荐训练方式：

```text
规则解析器 + 小模型 + LoRA 微调 + JSON Schema 校验
```

不要让模型直接输出 Everything 查询语法。模型输出 Search Intent JSON，再由程序生成查询。

训练阶段：

1. 提示词基线测试，不训练先验证原始模型能力。
2. 生成 3,000-5,000 条第一版训练样本。
3. 生成 500 条第一版评测集。
4. 在 MacBook M5 Pro 64GB 上使用 MLX / mlx-lm 做 LoRA 微调。
5. 根据评测错误补样本。
6. 迭代 2-4 轮。
7. 合并或挂载 adapter。
8. 导出量化部署格式，例如 GGUF Q4_K_M。
9. Windows 使用 llama.cpp / llama-server / Ollama 或自带 C++ runtime 推理。

### 7.3 训练数据类型

训练样本覆盖：

- 中文
- 英文
- 中英混合
- 常见错别字
- 口语表达
- 文件类型同义词
- 时间表达
- 路径表达
- 大小表达
- 排序表达
- 多轮上下文

样本示例：

```json
{
  "input": "找我昨天改过的 ppt",
  "output": {
    "intent": "file_search",
    "extensions": ["ppt", "pptx"],
    "modified_time": {
      "type": "relative",
      "value": "yesterday"
    },
    "sort": "modified_desc"
  }
}
```

```json
{
  "input": "find ppt yesterday edited",
  "output": {
    "intent": "file_search",
    "extensions": ["ppt", "pptx"],
    "modified_time": {
      "type": "relative",
      "value": "yesterday"
    },
    "sort": "modified_desc"
  }
}
```

```json
{
  "input": "找一首周华健的歌",
  "output": {
    "intent": "media_search",
    "domain": "music",
    "media_type": "audio",
    "artist": "周华健",
    "extensions": ["mp3", "flac", "wav", "m4a", "ape", "ogg"]
  }
}
```

### 7.4 本地推理目标

普通 16GB Mac 或 Windows 电脑目标体验：

- 简单查询：规则解析直接返回，几十毫秒级。
- 复杂查询：调用本地模型，目标 1-3 秒返回 Search Intent。
- 首次加载模型：2-10 秒，可通过后台常驻模型进程优化。
- 后端搜索耗时（典型值）：
  - Everything：毫秒级到亚秒级。
  - Windows Search / Spotlight：亚秒级到 1-2 秒，取决于索引覆盖范围与查询复杂度。

模型推理后端：

- macOS：llama.cpp（Metal）/ MLX 直接推理，Apple Silicon 上小模型表现良好。
- Windows：llama.cpp（CPU/Vulkan/CUDA）/ Ollama / 自带 C++ runtime。
- 模型本身与平台无关，量化后 GGUF 在两个平台上通用。

模型大小目标：

- 1.5B/1.7B 4-bit：约 0.9-1.4GB。
- 不建议默认使用 7B。

## 8. Harness 工程能力

项目必须将 Harness 作为核心工程底座，而不是后期补丁。

### 8.1 必备 Harness 能力

#### 多轮 tool loop

支持用户连续交互：

```text
找昨天编辑过的 ppt
只看下载目录里的
打开第三个
```

Agent 需要保留上一轮结果和筛选上下文。

#### 最大步数

防止 Agent 无限调用工具。MVP 可默认每轮最多 4-6 步。

#### Tool registry

所有工具必须注册：

- 工具名称
- 描述
- 输入 schema
- 输出 schema
- 权限级别
- 超时配置
- 是否可并发

#### Schema 校验

所有模型输出和工具输入输出都必须校验：

- Search Intent JSON
- Everything 查询参数
- 文件操作参数
- 索引器参数
- 结果格式

#### 权限检查

建议权限分级：

- Level 0：只读搜索，默认允许。
- Level 1：读取文件 metadata，默认允许但可关闭。
- Level 2：读取文件正文/OCR，需要用户授权目录。
- Level 3：打开文件，轻确认或用户点击触发。
- Level 4：复制/移动/重命名，明确确认。
- Level 5：删除/批量修改，默认禁用或强确认。

#### Sandbox

限制工具只能访问用户授权目录。外部命令调用必须白名单化。

#### Streaming 抽象

支持流式展示：

- 模型解析状态
- 正在搜索 Everything
- 正在读取 metadata
- 正在 OCR
- 已找到前 N 个结果
- 索引进度

#### 错误分类

至少包含：

- `NO_RESULTS`
- `BACKEND_UNAVAILABLE`
- `EVERYTHING_NOT_INSTALLED`
- `EVERYTHING_NOT_RUNNING`
- `WINDOWS_SEARCH_DISABLED`
- `SPOTLIGHT_DISABLED`
- `SPOTLIGHT_DIRECTORY_EXCLUDED`
- `FULL_DISK_ACCESS_REQUIRED`（macOS）
- `PERMISSION_DENIED`
- `MODEL_INVALID_JSON`
- `SCHEMA_VALIDATION_FAILED`
- `TOOL_TIMEOUT`
- `INDEX_NOT_READY`
- `FILE_UNREADABLE`
- `UNSUPPORTED_FILE_TYPE`
- `ACTION_REQUIRES_CONFIRMATION`

#### Hooks / tracing

记录每轮：

- 用户原始输入
- 解析出的 Search Intent
- 选用工具
- 工具入参
- 工具耗时
- 结果数量
- 错误信息
- 是否触发权限确认

默认本地保存，可提供用户清除日志入口。

#### Evals

建立固定评测集：

- 自然语言转 JSON 准确率
- JSON 合法率
- 文件类型识别准确率
- 时间解析准确率
- 查询生成准确率
- 工具路由准确率
- 多轮上下文准确率
- 安全策略准确率

### 8.2 补充 Harness 能力

#### Intent Router

判断用户意图：

- 文件名搜索
- 文档内容搜索
- 音乐搜索
- 图片/OCR 搜索
- 文件操作
- 澄清问题

#### Policy Engine

统一判断某个动作是否允许、是否需要确认。

#### Result Normalizer

将不同来源结果统一：

```json
{
  "id": "result_001",
  "path": "D:/Docs/demo.pptx",
  "name": "demo.pptx",
  "source": "everything",
  "match_type": "filename",
  "score": 0.93,
  "metadata": {
    "modified_time": "2026-05-13T10:30:00",
    "size_bytes": 12345678
  }
}
```

#### Ranking / Reranking

合并多来源结果后按相关性、时间、路径、文件类型、打开频率排序。

#### Context Memory

保存短期会话上下文：

- 上一轮查询条件
- 上一轮结果
- 用户筛选条件
- 指代关系，如“第三个”“刚才那些”

#### Confirmation Gate

文件打开、复制、移动、删除、批量操作必须通过确认门。

#### Audit Log

记录 Agent 做过什么，让用户可追溯。

#### Tool Timeout / Cancellation

长任务必须可取消。

#### Fallback Chain

推荐降级顺序：

```text
规则解析成功 → 直接搜索
规则不足 → 本地模型解析
模型输出非法 → 修复/重试
仍失败 → 澄清问题
语义搜索无结果 → 系统搜索（Spotlight / Windows Search / Everything）回退到文件名搜索
首选后端不可用：
  macOS：Spotlight 被禁用或目录被排除 → 自建轻量索引 或 提示用户调整 Spotlight 隐私设置 / 授予完整磁盘访问
  Windows：Everything 不可用 → Windows Search → 自建轻量索引或提示安装 Everything
```

#### Privacy Boundary

明确哪些数据只在本机，哪些永不上传，哪些可选上报。

#### Index Scheduler

后台低优先级索引，避免影响电脑性能。

#### Capability Discovery

启动时检测：

- 操作系统与版本（macOS / Windows，及其主次版本）
- macOS：Spotlight 是否启用、是否拥有完整磁盘访问（Full Disk Access）、用户排除目录列表
- Windows：Windows Search 服务是否运行、`SystemIndex` 是否可查询、当前索引位置
- Windows：Everything 是否安装、Everything 服务是否运行、ES 是否可用、SDK 是否可用
- OCR 是否可用
- 本地模型是否已下载
- 当前机器 CPU / GPU / NPU / Apple Silicon 能力
- 模型推理后端可用性（Metal / MLX / CUDA / Vulkan / CPU）

#### Plugin System

后续扩展：

- 音乐
- 照片
- 本地活动洞察
- 邮件
- 浏览器历史
- 聊天记录
- 云盘同步目录

#### Versioned Schemas

Search Intent JSON 和工具 schema 必须版本化。

## 9. 多语言能力

第一版支持：

- 中文
- 英文
- 中英混合

设计原则：

```text
多语言输入
  ↓
统一 Search Intent JSON
  ↓
统一查询生成器
```

不要为每种语言写独立后端逻辑。

同义词示例：

- `昨天` / `yesterday`
- `编辑过` / `modified` / `edited` / `changed`
- `ppt` / `powerpoint` / `演示文稿` / `presentation`
- `表格` / `excel` / `spreadsheet`
- `图片` / `image` / `photo`
- `视频` / `video`

日期表达必须由程序根据本地时区和 locale 计算。

## 10. 本地索引扩展

### 10.1 音乐 metadata

目标查询：

```text
找一首周华健的歌
找周华健的朋友
找我上个月下载的周华健无损音乐
```

索引字段：

- path
- file_name
- artist
- title
- album
- duration
- format
- bitrate
- modified_time

可选库：

- Python: mutagen
- C#: TagLib#
- Node.js: music-metadata

### 10.2 Office/PDF 内容索引

支持：

- doc/docx
- xls/xlsx
- ppt/pptx
- pdf
- txt/md/html

索引：

- 标题
- 正文片段
- 作者
- 修改时间
- 页码/幻灯片编号
- 摘要

### 10.3 OCR

支持：

- 图片
- 截图
- 扫描 PDF

查询示例：

```text
找我昨天截的付款二维码
找包含发票号码的截图
```

### 10.4 向量检索

Beta 或 1.0 加入：

- 本地 embedding 模型
- SQLite/duckdb + vector extension 或独立向量库
- BM25 + vector hybrid ranking

### 10.5 本地活动洞察

本地活动洞察（Local Activity Insights）用于回答“我最近在忙什么”和“哪些文件/文档类型占用了最多工作时间”这类问题。它不是 MVP 搜索闭环的前置条件，建议在 Beta 后期或 1.0 作为高价值增强模块加入。

目标问题：

```text
统计我最近打开最多的文档类型
我这周最常打开哪些文件
分析我最近的工作时间分布
我最近主要在忙哪些项目
过去 30 天我处理最多的是文档、表格还是演示稿
```

核心统计：

- 最近打开最多的文档类型：PDF、Word、Excel、PowerPoint、Markdown、图片、音频等。
- 最近高频文件：文件名、类型、最近打开时间、打开次数、所在项目目录。
- 工作时间分布：按小时、日期、工作日/周末聚合用户的本地文件活动。
- 工作主题摘要：基于目录名、文件名、文档标题、可选内容摘要推断近期工作重心。

可用数据来源：

- macOS：Spotlight 元数据、最近项目、应用打开记录（在权限允许范围内）。
- Windows：Windows Search、Recent Items、Jump Lists、文件访问时间（在权限允许范围内）。
- Scout 自有活动索引：用户通过 Scout 打开、预览、定位的文件事件。

本地索引字段建议：

- file_id / path_hash
- file_name（可由用户选择是否保存）
- extension / file_type
- parent_dir_hash / project_hint
- opened_at
- source（system_recent / scout_action / backend_metadata）
- event_type（open / preview / reveal / search_click）
- optional_title / optional_summary（默认关闭）

隐私边界：

- 活动洞察必须有独立开关，首次启用时明确说明会记录哪些本地活动。
- 默认优先记录类型、时间、hash 与统计值；完整路径、文件名、内容摘要应可关闭。
- 分析报告必须本地生成，不上传活动日志、文件名、路径或内容。
- 提供一键清除活动历史，并允许用户排除目录或文件类型。
- 日志与 tracing 默认不记录完整路径和正文内容。

与搜索的关系：

- Search Intent 仍负责“找什么”，活动洞察负责“最近怎样使用过本地资料”。
- 活动洞察可作为 Ranker 的可选信号，例如优先展示近期高频项目中的文件。
- 模型只能读取经过统计聚合和权限过滤后的活动摘要，不能直接扫描用户完整活动日志。

## 11. UI/UX 设计方向

产品应像本地工具，而不是营销网页。

核心界面：

- 全局快捷键呼出搜索框（macOS 默认 `⌘ Space` 类位（避免与 Spotlight 冲突，推荐 `⌥ Space`）；Windows 默认 `Ctrl + Space` 或 `Alt + Space`，可自定义）
- 单行自然语言输入
- 结果列表
- 筛选条
- 结果详情预览
- 权限确认弹窗
- 索引状态
- 搜索后端状态指示（当前后端是 Spotlight / Windows Search / Everything，是否降级）
- 本地活动洞察页（最近文件类型、常用文件、工作时间分布、近期工作主题）
- 设置页
- 隐私和数据管理页

结果展示字段：

- 文件名
- 路径
- 类型
- 修改时间
- 大小
- 命中原因
- 来源：Spotlight / Windows Search / Everything / metadata / OCR / content / vector

交互示例：

```text
用户：找昨天编辑过的 ppt
Agent：展示 12 个结果
用户：只看下载目录里的
Agent：在上一轮结果基础上筛选
用户：打开第三个
Agent：确认并打开
```

## 12. 安全与隐私

隐私承诺：

- 默认本地处理。
- 默认不上传文件、文件名、路径、索引、搜索词。
- 用户可删除所有本地索引和日志。
- 索引按操作系统用户账户隔离（Windows 用户配置文件 / macOS 用户主目录）。
- 敏感目录默认需要授权。
- macOS：默认不主动请求完整磁盘访问，只在用户需要搜索受保护目录时引导授权。

安全策略：

- 只读搜索默认允许。
- 文件正文索引需要目录授权。
- 文件操作需要确认。
- 删除和批量修改默认禁用或强确认。
- 外部工具白名单。
- 记录本地审计日志。

## 13. Everything 版权和分发策略

Everything 官方 License 是 MIT-like 宽松授权，允许使用、复制、修改、发布、分发、再授权、销售副本，但必须保留版权声明和许可声明。

商业化时建议：

- 产品不要命名为 Everything AI、Everything Agent、Smart Everything 等。
- 使用自有品牌。
- 文案使用“支持 Everything by voidtools”或“Works with Everything by voidtools”。
- 默认检测用户已安装的 Everything。
- 未安装时引导用户从 voidtools 官方下载安装。
- 若随产品分发 ES/SDK/Everything portable，必须在 Third-party Notices 中包含 voidtools 和 PCRE 等许可声明。
- 不暗示 voidtools 官方背书。
- 企业场景避免默认使用 Everything Server，涉及 Everything Server 时单独核对 site license。

相关官方资料：

- Everything License: https://www.voidtools.com/License.txt
- Everything SDK: https://www.voidtools.com/support/everything/sdk/
- Everything downloads: https://www.voidtools.com/downloads/
- ES CLI: https://github.com/voidtools/ES
- Enterprise: https://www.voidtools.com/en-us/enterprise/

法律备注：

正式商业发布前需要律师审查 EULA、第三方许可、安装包和官网宣传文案。

## 14. 技术选型建议

### 14.1 跨平台桌面客户端

候选：

- Tauri 2 + React/TypeScript（Rust 后端，跨平台原生 webview）
- Electron + React/TypeScript（成熟生态，资源占用较高）
- Flutter Desktop（Dart，跨平台一致，但桌面生态较弱）

建议：

- 首选 **Tauri 2**：
  - 同时支持 macOS、Windows、Linux。
  - 二进制小、内存占用低，适合本地常驻搜索工具。
  - Rust 后端方便实现 SearchBackend 适配器与系统调用（Spotlight、OLE DB、Everything SDK）。
  - 原生支持全局快捷键、托盘、文件系统权限请求。
- 不再推荐 .NET/WinUI 3（Windows 专用，不利于跨平台）。
- Electron 作为风险预案：若 Tauri 在某些 OS 版本上 webview 兼容性差，可临时切换。

### 14.2 本地服务

可选：

- **Rust 本地服务**（推荐）：与 Tauri 同语言，跨平台编译，性能好，调用系统 API 方便。
- C++ 服务：仅在需要直接复用 llama.cpp 或 Everything SDK 时考虑。
- Node.js 服务：原型期可用，正式版资源占用偏重。

平台特定适配模块：

- macOS：通过 Rust FFI 调用 `mdfind`、`NSMetadataQuery`，或直接 `std::process::Command` 启动 `mdfind`。
- Windows：通过 Rust `windows` crate 调用 OLE DB / WinRT，Everything 通过 SDK 的 C ABI 接入。

### 14.3 模型推理

跨平台统一：

- **llama.cpp**：macOS（Metal）、Windows（CPU / Vulkan / CUDA）通用，量化 GGUF 格式跨平台。
- Ollama：作为可选用户安装的本地服务，跨平台。
- 自带 C++ runtime：MVP 之后评估。

平台优化：

- macOS（Apple Silicon）：Metal 后端，MLX 仅在训练侧使用。
- Windows：默认 CPU 推理，检测到独立 GPU 时启用 CUDA 或 Vulkan。

训练侧（仅 Mac）：

- MLX / mlx-lm 做 LoRA 微调。
- 训练产物（adapter / 合并模型 / 量化 GGUF）跨平台通用。

### 14.4 索引存储

推荐（跨平台一致）：

- SQLite：metadata、全文轻量索引、审计日志。
- SQLite FTS5：全文搜索。
- DuckDB：后续分析型索引可选。
- 向量检索：后续再决定 sqlite-vec、Qdrant local、LanceDB 等。

存储位置：

- macOS：`~/Library/Application Support/Scout/`
- Windows：`%APPDATA%\Scout\`
- 索引按操作系统用户账户隔离，多用户机器互不可见。

## 15. 开发里程碑

### 15.1 技术原型：1-2 周（macOS 优先）

目标：

- 输入自然语言。
- 输出 Search Intent JSON。
- 通过 SearchBackend 抽象生成 `mdfind` 查询并在 Mac 上执行。
- CLI 或最简 UI 展示结果。

选 macOS 作为原型平台的原因：

- `mdfind` 调用最简单，几小时即可跑通端到端闭环。
- 开发者本机即开发即测试。
- 跨平台抽象在最简单的后端上先打磨，更不容易过度设计。

交付物：

- Search Intent schema
- 规则解析器
- Prompt 基线
- SearchBackend trait + SpotlightBackend 实现
- 基础 evals 100 条

### 15.2 MVP：3-5 周（macOS + Windows 双平台）

目标：

- 同一份 Tauri 应用在 macOS 与 Windows 上均可运行。
- macOS：Spotlight 后端默认启用。
- Windows：Windows Search 后端默认启用；检测到 Everything 时自动切换。
- 本地小模型可选（GGUF 在两个平台上共用）。
- 基础 Harness 完成。

交付物：

- 桌面 UI（Tauri，跨平台）
- SearchBackend：Spotlight + Windows Search + Everything 三套实现
- Capability Discovery
- Tool registry
- Tool loop
- Schema 校验
- 权限检查（含 macOS Full Disk Access 引导）
- 错误分类（含跨平台错误码）
- Tracing
- 训练数据生成器
- 500 条 evals（每个平台跑一遍）
- 第一版 LoRA 或 prompt-only 模型

### 15.3 Beta：8-12 周

目标：

- 音乐 metadata、文档内容、OCR 初步可用。
- 多源结果合并。
- 用户可长期试用。
- macOS 与 Windows 安装包均可签名分发。

交付物：

- Music Metadata Tool
- Office/PDF Index Tool
- OCR Tool（macOS Vision framework / Windows OCR API / Tesseract 跨平台兜底）
- Result Normalizer
- Ranking
- Audit Log
- Index Scheduler
- 量化模型
- macOS 安装包（DMG + notarization）
- Windows 安装包（MSI / MSIX，含代码签名）
- 1000 条 evals

### 15.4 产品级 1.0：4-6 个月

目标：

- 普通用户可稳定发布。
- 完整隐私、权限、索引、模型管理。

交付物：

- 完整客户端
- 插件系统
- 模型升级机制
- 本地活动洞察
- 自动更新
- 崩溃恢复
- 完整测试矩阵
- 法务和许可文档
- 官网和用户文档

## 16. 项目风险评估

### 16.1 产品风险

风险：

- 市面已有 Searchibald、Pronto、FileScope、Linkly AI、remio、Microsoft Recall 等相近产品。

应对：

- 避开“重型 AI 搜索引擎”定位。
- 先做轻量 Everything 自然语言前端。
- 强调本地、轻量、低资源占用、无需 API Key。
- 用 Harness 安全可控作为差异化。

### 16.2 技术风险

风险：

- 本地模型在低配 Windows 机器上响应慢。
- Everything 未安装或服务不可用。
- Windows Search 服务被企业策略禁用，或索引覆盖不足。
- macOS Spotlight 被排除目录、Full Disk Access 未授权导致结果缺失。
- 跨平台开发复杂度：UI、文件路径、权限模型、安装包、签名各有差异。
- 文件内容索引/OCR 耗时。
- 多源排序效果不稳定。

应对：

- 规则优先，模型兜底。
- 1.5B/1.7B 4-bit 模型。
- 后台常驻模型进程。
- SearchBackend 抽象，所有后端均可插拔、可降级。
- 平台特定代码集中在 `backend/` 与 `platform/` 两个模块，UI 与业务逻辑跨平台共享。
- Tauri 处理大部分跨平台 UI / 打包 / 签名差异。
- 大任务后台低优先级执行。

### 16.3 法律和授权风险

风险：

- Everything 商标使用不当。
- Apple / Microsoft 商标和品牌指引使用不当（如 "Spotlight Search by Scout" 等暗示官方背书的措辞）。
- 第三方许可声明遗漏。
- 企业使用 Everything Server 授权不清。
- macOS App Store / Microsoft Store 上架审查（沙箱、隐私权限说明、应用类别）。

应对：

- 使用自有品牌。
- 不暗示 voidtools / Apple / Microsoft 官方背书。
- Spotlight、Windows Search 仅以"支持"/"兼容"形式描述。
- Third-party Notices 完整（含 Everything、PCRE、llama.cpp、模型 license 等）。
- 企业版单独审查授权。
- 提前阅读 App Store / Microsoft Store 审核指南，尤其是与文件访问、AI、网络相关的条款。

### 16.4 隐私风险

风险：

- 索引文件名、路径、正文、OCR 内容涉及高度敏感信息。

应对：

- 默认本地处理。
- 本地索引可删除。
- 目录授权。
- 审计日志。
- 明确隐私边界。
- 可选索引加密。

### 16.5 Agent 安全风险

风险：

- Agent 误执行打开、移动、删除、批量操作。

应对：

- 文件操作分级权限。
- 删除默认禁用。
- Confirmation Gate。
- 最大步数和超时。
- Tool schema 和 Policy Engine。

## 17. 开发任务拆分建议

建议新项目初始目录：

```text
local-search-agent/
  docs/
    project-plan.md
    search-intent-schema.md
    harness-design.md
    privacy-security.md
    third-party-licenses.md
  apps/
    desktop/                 # Tauri 跨平台桌面应用
  packages/
    harness/
    intent-parser/
    search-backends/
      spotlight/             # macOS
      windows-search/        # Windows
      everything/            # Windows 可选加速
      common/                # SearchBackend trait、查询 IR、结果归一化
    result-normalizer/
    ranker/
    indexer/
    model-runtime/
    evals/
  platform/
    macos/                   # FFI、权限请求、安装/签名
    windows/                 # WinRT、OLE DB、安装/签名
  training/
    datasets/
    generators/
    mlx-lora/
    evals/
  scripts/
  tests/
```

第一批开发任务（macOS 优先打通闭环）：

1. 创建 Search Intent JSON schema。
2. 实现规则解析器。
3. 定义 SearchBackend trait（输入：SearchIntent；输出：归一化结果流）。
4. 实现 SpotlightBackend（封装 `mdfind`）。
5. 实现 Tool registry。
6. 实现 Schema Validator。
7. 实现基础 CLI 测试入口（先跑 macOS）。
8. 建立 100 条 evals。
9. 加入本地小模型 prompt-only 推理（llama.cpp Metal）。
10. 构建最简 Tauri 桌面 UI（macOS 先跑通）。

第二批开发任务（Windows 平台 + Harness 增强）：

1. 实现 WindowsSearchBackend（OLE DB / SystemIndex SQL）。
2. 实现 EverythingBackend（ES CLI 优先，SDK 备选）。
3. 实现 Capability Discovery，自动选择默认后端。
4. Tauri 应用在 Windows 上跑通同一份 UI。
5. 加入多轮上下文。
6. 加入权限系统（含 macOS Full Disk Access 引导）。
7. 加入错误分类（跨平台错误码）。
8. 加入 tracing。
9. 扩展 evals 到 500 条（macOS / Windows 各跑一遍）。
10. 生成训练数据。
11. 在 Mac 上做 LoRA 微调。
12. 导出量化模型。
13. Windows 本地推理集成（llama.cpp CPU / Vulkan / CUDA）。

第三批开发任务：

1. 音乐 metadata 索引。
2. Office/PDF 内容索引。
3. OCR（macOS Vision / Windows OCR API / Tesseract 兜底）。
4. Result Normalizer。
5. Ranking。
6. 后台索引调度。
7. Audit Log。
8. 安装包（macOS DMG + notarization；Windows MSI/MSIX + 代码签名）。

## 18. 成功指标

MVP 成功指标：

- 90% 以上简单文件搜索输入可生成合法 Search Intent JSON。
- 85% 以上中文/英文/中英混合常见查询解析正确。
- 简单查询响应小于 500ms（Spotlight / Everything）。
- 复杂模型解析查询响应小于 3 秒。
- SearchBackend 工具调用成功率大于 95%（覆盖 Spotlight、Windows Search、Everything 三个后端）。
- macOS 与 Windows 同一份 evals 通过率差距小于 5 个百分点。
- 模型输出 JSON 合法率大于 98%。
- 文件操作权限策略 100% 通过安全 evals。

Beta 成功指标：

- 1000 条 golden evals 总体通过率大于 90%。
- 音乐 artist/title metadata 搜索准确率大于 85%。
- Office/PDF 内容搜索 Top 5 命中率大于 80%。
- OCR 查询 Top 5 命中率大于 75%。
- 普通 16GB Windows 机器可流畅运行。
- 用户可一键删除本地索引和日志。

## 19. 推荐下一步

立即启动的最小工作包（macOS 上跑通）：

1. 创建独立项目仓库 `local-search-agent`。
2. 将本计划书复制到 `docs/project-plan.md`。
3. 定义 `SearchIntent` JSON schema。
4. 实现自然语言规则解析器。
5. 定义 `SearchBackend` trait 与归一化结果格式。
6. 实现 `SpotlightBackend`：将 SearchIntent 转成 `mdfind` 调用。
7. 用 30-50 条手工用例在 Mac 上验证闭环。

第一条闭环必须做到（macOS）：

```text
输入：查找昨天编辑过的 ppt
输出 JSON：extensions = ["ppt", "pptx"], modified_time = yesterday
Spotlight 查询（mdfind）：
  (kMDItemFSName == '*.ppt'cd || kMDItemFSName == '*.pptx'cd) &&
  kMDItemContentModificationDate >= $time.today(-1) &&
  kMDItemContentModificationDate <  $time.today(0)
搜索结果：展示本机匹配文件
```

第二条闭环（macOS）：

```text
输入：find ppt yesterday edited
输出与中文输入一致
```

第三条闭环（macOS）：

```text
输入：找一首周华健的歌
输出 domain = music, artist = 周华健, extensions = audio formats
先用 mdfind 按文件名 + kMDItemAuthors / kMDItemMusicalGenre 等 metadata 搜索
后续接入独立 music metadata index
```

完成 macOS 闭环后立即做的 Windows 平移工作包：

1. 实现 `WindowsSearchBackend`，把同样三条用例在 Windows 上跑通（SystemIndex SQL）。
2. 实现 `EverythingBackend`，在检测到 Everything 时跑出同样结果。
3. 跑同一份 evals，比较三个后端结果差异。

做到 macOS 三条闭环 + Windows 平移验证，项目就从想法进入了可迭代工程阶段。
