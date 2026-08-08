# Scout 合成测试 Fixtures

本目录包含用于测试和评测的合成文件集。

## 结构

- `generate.sh`: 幂等生成脚本（调用 `packages/evals/src/bin/fixtures.rs`）。
- `reindex.sh`: 调用 `mdimport` 强制 Spotlight 重新索引生成的文件。
- `files/`: 生成的文件存放处（**不入库**）。

## 用法

### 1. 生成文件

```bash
bash tests/fixtures/generate.sh
```

### 2. 重新索引 (macOS)

生成文件后，需要让 Spotlight 知道这些文件的存在：

```bash
bash tests/fixtures/reindex.sh
```

### 3. 验证

```bash
mdfind -onlyin "$(pwd)/tests/fixtures/files" "kMDItemFSName == '*.pptx'cd"
```

### 4. 清理

```bash
bash tests/fixtures/generate.sh clean
```

## 覆盖说明

生成的合成文件覆盖了：
- **文件类型**: pptx, xlsx, docx, pdf, md, zip, mp3, flac, png, mp4.
- **时间分布**: 今天、昨天、最近三天、上周、上月、2025年。
- **大小分布**: 小文件、>100MB 视频、>1GB 视频。
- **命名**: 中文 (如 `合成-预算-2026.docx`)、英文、中英混合。
- **目录结构**: 模拟 Desktop, Downloads, Documents, Music, Pictures/Screenshots.

## 安全性

所有文件均为程序合成，**不包含任何真实用户路径或敏感信息**。
