# scout-cli

`scout-cli` 是 PROTO-07 的顶层原型 CLI，用于跑通：

```text
自然语言查询 → SearchIntent → SpotlightBackend → 搜索结果
```

## 用法

自然语言搜索：

```bash
cargo run -p scout-cli -- "查找昨天编辑过的 ppt"
```

每行输出一个结果，格式为：

```text
路径<TAB>修改时间
```

JSON 输出模式：

```bash
cargo run -p scout-cli -- --json "查找昨天编辑过的 ppt"
```

stdout 先输出 `SearchIntent` JSON，再输出 `SearchResult[]` JSON。stderr 输出 backend trace。

只解析 intent，不执行搜索：

```bash
cargo run -p scout-cli -- --intent-only "查找昨天编辑过的 ppt"
```

限制搜索范围：

```bash
cargo run -p scout-cli -- --onlyin tests/fixtures/files "查找昨天编辑过的 ppt"
```

`--onlyin` 可重复使用；相对路径会按当前工作目录转成绝对路径，并追加到 `Location.include`。

帮助：

```bash
cargo run -p scout-cli -- --help
```

## 退出码

| 退出码 | 含义 |
|---:|---|
| 0 | 成功且有结果 |
| 1 | 成功但无结果 |
| 2 | intent 为 `clarify`，需要用户澄清 |
| 3 | intent 为 `refine` 或 `file_action`，v0.1 CLI 暂不支持端到端执行 |
| 4 | backend / 系统错误 |
