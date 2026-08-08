# SearchBackend Trait 最简形态（v0.1）

> 状态：**v0.1（已采纳 Codex 审阅意见修订）**，未实施。本文件给出原型期 SearchBackend 的最小契约，与 [docs/search-intent-schema.md](./search-intent-schema.md) 配套。
> 实施位置：[packages/search-backends/common](../packages/search-backends/common/)。
> 本版变更摘要见本文件最后一节。

## 1. 设计目标

让所有搜索后端（Spotlight / Windows Search / Everything / 未来自建索引）通过同一个 trait 暴露能力。
原型期的 trait **只覆盖核心搜索路径**：输入 `SearchIntent`，输出归一化结果流。

不放进 v0.1 的东西（留到 MVP 阶段补）：

- Capability Discovery 接口（先用静态检测）
- Streaming 取消信号（先用阻塞式带超时）
- 后端配置 / preferences
- 自检 / 健康检查接口
- 错误码完整体系（先 5-6 个核心错误）

## 2. 核心类型（语言无关描述）

### 2.1 `SearchIntent`

见 [search-intent-schema.md](./search-intent-schema.md)。后端只接受已通过 schema 校验的 intent。

### 2.2 `SearchResult`

```text
SearchResult {
  id:         string            // 见下方"id 稳定性级别"
  path:       string            // 绝对路径（OS native）
  name:       string            // 文件名（含扩展名）
  source:     enum BackendKind  // 哪个后端返回的
  match_type: enum MatchType    // filename | content | metadata | ocr
  score:      float | null      // 0.0–1.0；后端无相关性分数时为 null
  metadata: {
    modified_time:  ISO-8601 string | null
    created_time:   ISO-8601 string | null
    accessed_time:  ISO-8601 string | null
    size_bytes:     integer     | null
    // 媒体专属（按需填充）
    artist:   string | null
    title:    string | null
    album:    string | null
    duration_seconds: number | null
  }
}
```

#### `id` 稳定性级别（由 Codex 审阅 should-have #12 落地）

`id` 的稳定性分阶段提高，调用方不应假设跨阶段一致：

- **v0.1（本版，原型期）**：仅保证**本次查询内稳定**。生成方式：规范化绝对路径的 SHA-256 前 16 字符。文件被 rename / move 后 id 会变化。
- **MVP**：在跨多轮上下文内稳定。生成方式：
  - macOS：基于 file system id + inode（`fstat`）或 NSURL bookmark data。
  - Windows：基于 NTFS file reference number 或 Windows Search index id。
- **Beta 起**：跨会话稳定（持久化到本地索引）。

调用方使用 `id` 仅用于本次会话内的 target_ref 指代；持久化引用必须用 `path` + 修改时间组合校验，而不是直接信任 `id`。

### 2.3 `SearchError`

v0.1 错误码（最小集 + Codex 审阅 should-have #10 追加的 `UnsupportedIntent`）：

```text
enum SearchError {
  BackendUnavailable    { reason: string }       // 后端整体不可用（服务未运行、可执行缺失等）
  PermissionDenied      { path: string | null }  // 系统权限不足
  InvalidIntent         { detail: string }       // intent 本身不合法（schema 应已拦截，这里是兜底）
  UnsupportedIntent     { detail: string }       // intent 合法但当前后端不支持
  Timeout               { elapsed_ms: integer }  // 超过 deadline
  IO                    { detail: string }       // 其他 IO 错误
}
```

**`InvalidIntent` vs `UnsupportedIntent` 的边界**：

- `InvalidIntent`：intent 违反 schema 校验或基本完整性（理论上 Harness 已拦截，backend 拿到的不应该再有这种；但保留兜底）。
- `UnsupportedIntent`：intent 合法、但当前 backend 没能力满足。例如：
  - SpotlightBackend 原型期收到 `media_search.artist`，但还未接入媒体 metadata 索引 → `UnsupportedIntent { detail: "media metadata not yet supported in prototype" }`
  - SpotlightBackend 收到 `exclude_file_type` 但暂未实现 → `UnsupportedIntent { detail: "exclude_file_type not yet implemented" }`

这样 Harness 可以分辨"parser 出 bug"（InvalidIntent）与"backend 能力差距"（UnsupportedIntent），后者触发 Fallback Chain 或返回 clarify，而前者要进 tracing 告警。

#### MVP 期补充的错误码（占位，本版不实现）

`BackendUnavailable.reason` 在 v0.1 承载以下情况的可诊断文本；MVP 拆分为独立 variant：

- `SpotlightDisabled` / `SpotlightDirectoryExcluded` / `FullDiskAccessRequired`（macOS）
- `WindowsSearchDisabled`（Windows）
- `EverythingNotInstalled` / `EverythingNotRunning` / `EverythingVersionTooOld`（Windows + Everything）

### 2.4 `BackendKind`

```text
enum BackendKind { Spotlight, WindowsSearch, Everything, NativeIndex }
```

## 3. Trait 定义（Rust 草稿）

```rust
use std::time::Duration;

/// 所有搜索后端实现此 trait。原型期同步阻塞返回完整结果集；
/// MVP 阶段切换为 async + 结果流。
pub trait SearchBackend: Send + Sync {
    /// 后端身份。
    fn kind(&self) -> BackendKind;

    /// 当前环境下是否可用。原型期可简单检测可执行文件 / API 是否存在。
    /// 返回 false 时 Harness 应跳到下一个后端。
    fn is_available(&self) -> bool;

    /// 执行一次搜索。
    ///
    /// `intent` 已通过 Schema 校验；后端只需关心如何翻译。
    /// `timeout` 是本次调用的最大允许时长。
    ///
    /// **超时语义（v0.1，由 Codex 审阅 should-have #11 确定）**：
    ///   - 超过 `timeout` 时返回 `Err(SearchError::Timeout { elapsed_ms })`。
    ///   - v0.1 **不**返回部分结果；要么完整 `Ok(Vec<_>)`，要么明确的错误。
    ///   - 后端应尽可能协作式响应超时（kill 子进程、释放游标等），避免泄漏。
    fn search(
        &self,
        intent: &SearchIntent,
        timeout: Duration,
    ) -> Result<Vec<SearchResult>, SearchError>;
}
```

### 3.1 设计取舍

- **同步而非 async**：原型期减少异步生态依赖（tokio 等），最快跑通闭环。MVP 切 async + `Stream<Item = SearchResult>`。
- **`Vec` 而非流**：同上。Spotlight `mdfind` 子进程的输出本就是一次性收集后返回。
- **不返回查询字符串**：调用者不需要知道后端实际生成了什么查询。Tracing 在后端内部完成。
- **`timeout: Duration`**：v0.1 采用**简单方案 — 超时即错误，不返回部分结果**（Codex 审阅 should-have #11）。若 MVP 需要部分结果，再引入 `SearchOutcome { results, partial, warnings }` 结构，而不是混淆当前 `Result<Vec<_>, _>` 的语义。
- **`&SearchIntent` 而非 `SearchIntent`**：避免拷贝；后端不应持有 intent 引用超过本次调用。

## 4. 原型期实现路径（仅 macOS / Spotlight）

```rust
pub struct SpotlightBackend {
    // 原型期无配置；MVP 加入 mdfind 路径 override、并发限制等
}

impl SearchBackend for SpotlightBackend {
    fn kind(&self) -> BackendKind { BackendKind::Spotlight }

    fn is_available(&self) -> bool {
        // 检查 mdfind 可执行存在 + Spotlight 服务运行
        which::which("mdfind").is_ok()
    }

    fn search(
        &self,
        intent: &SearchIntent,
        deadline: Duration,
    ) -> Result<Vec<SearchResult>, SearchError> {
        // 1. 把 intent 翻译成 mdfind 谓词字符串
        // 2. spawn `mdfind` 子进程（带 -onlyin / -name 等参数）
        // 3. 收集 stdout 路径列表
        // 4. 对每个路径调用 mdls 或 std::fs::metadata 补 metadata
        // 5. 归一化为 Vec<SearchResult>
        todo!()
    }
}
```

### 4.1 mdfind 翻译规则（首版，已采纳 Codex 审阅 must-fix #5）

| Intent 字段 | mdfind 谓词片段 | 备注 |
|---|---|---|
| `keywords: ["X"]` | 通过 `-name` / 谓词组合，**不**作为裸第一参数 | 见下方 "keywords 语义" |
| `extensions: ["ppt","pptx"]` | `(kMDItemFSName == "*.ppt"cd \|\| kMDItemFSName == "*.pptx"cd)` | `"*.ppt"` 用双引号确保子进程参数化传递，**严格禁止** shell 展开 |
| `file_type=presentation`（无 extensions 时） | 展开为扩展名集合后同上；should-have 验证项：是否改用 `kMDItemContentTypeTree == 'public.presentation'` 更稳 | 见下方 "扩展名 vs UTI" |
| `modified_time.relative=yesterday` | `kMDItemContentModificationDate >= $time.today(-1) && kMDItemContentModificationDate < $time.today(0)` | 程序按本地时区计算边界 |
| `modified_time.absolute={from,to}` | `kMDItemContentModificationDate >= $time.iso("<from>") && kMDItemContentModificationDate <= $time.iso("<to>T23:59:59")` | from/to 是 ISO 日期 |
| `created_time.*` | 同 modified，用 `kMDItemContentCreationDate` | |
| `accessed_time.*` | 同 modified，用 `kMDItemLastUsedDate` | |
| `size.greater_than=100MB` | `kMDItemFSSize > 104857600` | 单位：1 MB = 1,000,000 字节（与 schema §4.2 注释一致） |
| `size.between={min,max}` | `kMDItemFSSize >= <min> && kMDItemFSSize <= <max>` | |
| `location.include=["~/Downloads"]` | 命令行 `-onlyin ~/Downloads` | 多个 include → 多个 `-onlyin`；mdfind 取并集 |
| `location.exclude=[...]` | mdfind **不直接支持**，结果端过滤 | 已知能力差距；列入 §8 开放问题 |
| `exclude_extensions: ["mp4"]` | `kMDItemFSName != "*.mp4"cd` 与其他条件 AND | 取反谓词 |
| `exclude_file_type: ["archive"]` | 展开为 `exclude_extensions` 后同上 | |

#### keywords 语义（v1.0，必读）

- v1.0 `keywords` 默认作用域是"**文件名 + 内容 + metadata 宽匹配**"（与 schema §3.1 字段注释一致）。
- SpotlightBackend 的实现：把每个 keyword 作为独立的谓词条件 `(kMDItemDisplayName CONTAINS[cd] "X" || kMDItemTextContent CONTAINS[cd] "X" || kMDItemFSName CONTAINS[cd] "X")`，多个 keyword 之间 AND。
- **不**用 `mdfind` 的裸关键词形式（裸关键词的语义在不同 macOS 版本上不稳定，特别是中文 token 切分）。
- 中文文件名 / 内容匹配必须用 `cd` 修饰符（c = case-insensitive, d = diacritic-insensitive），裸关键词无法保证覆盖中文。
- "名字以…开头"等前缀语义在 v1.0 schema 中已声明为有损（schema §7.1 #7），SpotlightBackend 只承诺包含匹配。

#### 扩展名 vs UTI（kMDItemContentTypeTree）

- 优先 `kMDItemFSName == "*.ext"cd`（路径稳定、行为可预期）。
- `kMDItemContentTypeTree == "public.presentation"` 作为 **should-have 验证项**：某些应用导出的文件 UTI 与扩展名不一致（如 Keynote `.key` 实际是 bundle），UTI 路径可能更稳，但跨版本一致性需实测。Beta 阶段决定是否切换。

#### 安全：shell 注入与参数传递

- SpotlightBackend 通过 `Command::new("mdfind").arg("-onlyin").arg(path).arg(predicate)` 等**结构化方式**传递参数，**绝不**拼接 shell 命令。
- 用户输入的关键词在进入谓词字符串前必须转义双引号与反斜杠：`X` → `X`（无特殊字符）/ `say "hi"` → `say \"hi\"`。
- `location.include` 路径必须先 `canonicalize`，拒绝包含 null byte 或换行符的输入。

### 4.2 实测验证清单（首版开发时必跑）

由 Codex 审阅 must-fix #5 落地，原型期 SpotlightBackend 实现完成后必须验证：

- [ ] 中文文件名包含匹配：`cd` 修饰符是否正确覆盖中文，与 `c`-only / 裸关键词的差异
- [ ] 中文路径下的 `-onlyin` 行为
- [ ] `kMDItemFSName == "*.ppt"cd` 对 macOS 14 / 15 / 26 上的命中一致性
- [ ] `kMDItemContentTypeTree == "public.presentation"` 对 Keynote `.key` bundle、Office `.pptx`、LibreOffice `.odp` 的覆盖差异
- [ ] `$time.today(-1)` 的时区行为（夏令时切换日是否会偏移）
- [ ] `exclude_extensions` 端过滤的性能（大结果集下的内存消耗）
- [ ] Spotlight 未索引目录的报错形态（区分 `BackendUnavailable` vs `UnsupportedIntent`）
- [ ] `mdfind` 子进程被超时中断时的资源回收

## 5. Stub Backends（原型期占位，由 Codex 审阅 must-fix #6 加固）

为了让 Harness 能在 macOS 开发时枚举三个后端，需要 Windows/Everything 的占位实现。但占位**不能被误当成真实后端**，否则集成测试和后续 fallback 链可能产生假阳性。

#### 加固规则

1. **类型名显式带 `Stub` 后缀**：`WindowsSearchStubBackend` / `EverythingStubBackend`。**未来的真实实现使用不带 Stub 的名字**（`WindowsSearchBackend` / `EverythingBackend`），保证类型层面无冲突。
2. **编译特性门控**：stub 实现位于 `packages/search-backends/{windows-search,everything}/src/stub.rs`，且只在 `cfg(feature = "stub")` 或 `cfg(test)` 下编译。生产构建（`--release`）默认不包含 stub。
3. **`kind()` 返回真实 `BackendKind`，但暴露状态**：增加 `implementation_status()` 方法，返回 `enum ImplementationStatus { Real, Stub }`；Harness 注册后端时按状态决定是否进入生产 fallback 链。
4. **集成测试断言**：在 `tests/` 中加测试 — 生产构建下 `BackendRegistry::production_backends()` 不能包含 `ImplementationStatus::Stub` 的实例。

#### 示例代码

```rust
// packages/search-backends/common/src/lib.rs
pub enum ImplementationStatus { Real, Stub }

pub trait SearchBackend: Send + Sync {
    fn kind(&self) -> BackendKind;
    fn implementation_status(&self) -> ImplementationStatus { ImplementationStatus::Real }
    fn is_available(&self) -> bool;
    fn search(&self, intent: &SearchIntent, timeout: Duration) -> Result<Vec<SearchResult>, SearchError>;
}

// packages/search-backends/windows-search/src/stub.rs
#[cfg(any(feature = "stub", test))]
pub struct WindowsSearchStubBackend;

#[cfg(any(feature = "stub", test))]
impl SearchBackend for WindowsSearchStubBackend {
    fn kind(&self) -> BackendKind { BackendKind::WindowsSearch }
    fn implementation_status(&self) -> ImplementationStatus { ImplementationStatus::Stub }
    fn is_available(&self) -> bool { false }
    fn search(&self, _: &SearchIntent, _: Duration) -> Result<Vec<SearchResult>, SearchError> {
        Err(SearchError::BackendUnavailable {
            reason: "WindowsSearchStubBackend: not implemented; this is a development stub".into(),
        })
    }
}
```

#### Harness 注册保护

```rust
pub struct BackendRegistry { backends: Vec<Box<dyn SearchBackend>> }

impl BackendRegistry {
    /// 生产 fallback 链 — 自动剔除 stub。
    pub fn production_backends(&self) -> Vec<&dyn SearchBackend> {
        self.backends.iter()
            .filter(|b| matches!(b.implementation_status(), ImplementationStatus::Real))
            .map(|b| b.as_ref())
            .collect()
    }
}

#[test]
fn production_chain_excludes_stubs() {
    let registry = build_default_registry();
    for b in registry.production_backends() {
        assert!(matches!(b.implementation_status(), ImplementationStatus::Real));
    }
}
```

## 6. Harness 如何选择后端（原型期简化）

```rust
pub fn select_backend<'a>(
    backends: &'a [Box<dyn SearchBackend>],
) -> Option<&'a dyn SearchBackend> {
    backends.iter()
        .filter(|b| matches!(b.implementation_status(), ImplementationStatus::Real))
        .find(|b| b.is_available())
        .map(|b| b.as_ref())
}
```

后端顺序由 Harness 启动时按平台填充：

- macOS：`[SpotlightBackend]`（原型期；NativeIndexBackend 未来加入）
- Windows：`[EverythingBackend, WindowsSearchBackend]`（原型期均未实现，仅在 `feature = "stub"` 下注册占位以供 Harness 单元测试）

MVP 阶段升级为带 Capability Discovery 与用户偏好覆盖的选择器。

## 7. 与 v0.1 schema 的对齐

42 条 schema 用例（[search-intent-schema.md §7](./search-intent-schema.md)）中：

- **§7.1–§7.4（30 条 file_search / media_search / 混合）**：原型期 SpotlightBackend 必须全部能翻译并返回结果（媒体 metadata 字段在原型期允许返回 null，Beta 加 metadata 索引）。
- **§7.5（5 条 refine）**：由 Harness Context Memory 合并 delta 后再交给 backend，backend 本身只看到合并后的 file_search/media_search intent。
- **§7.6（5 条 file_action）**：不经过 SearchBackend，走 FileActionTool（另写）。
- **§7.7（2 条 clarify）**：不经过 SearchBackend，由 UI 直接渲染问题。

## 8. 开放问题

- **结果流 vs 完整集**：Spotlight `mdfind` 默认一次性输出；要做 streaming 需要切到 `NSMetadataQuery`。原型期接受一次性，但 trait 是否提前为 streaming 留口子（如返回 `Iterator`）？倾向"不提前，MVP 切 async stream 时一并改"。
- **跨后端结果去重**：同一文件被多个 backend 命中时由 Result Normalizer 处理，不在 SearchBackend trait 层。
- **错误降级**：`BackendUnavailable` 时由谁决定 fallback？倾向 Harness 层的 Fallback Chain，不在 trait 上加 fallback 字段。
- **`location.exclude` 在 Spotlight 上的支持**：`mdfind` 不支持原生 exclude；当前方案是结果端过滤。需评估在大结果集（10K+）下的内存与性能影响。

---

## 9. v0.1 Codex 审阅修订摘要

### must-fix（已全部修订）

| # | 修订点 | 落地位置 |
|---|---|---|
| 5 | mdfind 翻译规则：keywords 语义、`cd` 修饰符、扩展名谓词形式、shell 注入防护、UTI 验证项 | §4.1 整段重写；新增 §4.2 实测验证清单 |
| 6 | Stub backend 命名 / 编译特性门控 / `implementation_status()` / 集成测试断言 | §5 整段重写；§6 select_backend 加 stub 排除 |

### should-have（已全部修订）

| # | 修订点 | 落地位置 |
|---|---|---|
| 10 | 错误码加 `UnsupportedIntent`，与 `InvalidIntent` 边界说明；MVP 错误码占位 | §2.3 |
| 11 | 超时策略：v0.1 简单方案，超时即 `Err(Timeout)`，不返回部分结果 | §3 trait 注释 + §3.1 取舍 |
| 12 | `SearchResult.id` 稳定性级别（v0.1 / MVP / Beta） | §2.2 |

### out-of-scope（明确不做）

- #16 v0.1 不做 async / streaming：保留同步 + `Vec<SearchResult>`。
- #17 v0.1 不做完整 Capability Discovery：只有 `is_available()`。

### Schema 侧的对应修订

- must-fix #1 / #2 / #3 / #4，nice-to-have #13–#15，corner cases A–E 全部落到 [search-intent-schema.md](./search-intent-schema.md) §9。
