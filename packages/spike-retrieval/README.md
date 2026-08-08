# spike-retrieval — 本地现实校准锤

起源于 BETA-26 的语义检索质量探针，**BETA-15B-6 起转正为常驻工具**，不再是一次性丢弃 crate。

## 定位

用**真实 home 数据**周期性核对 `packages/evals` 的**合成评测集是否同向**：

- `packages/evals` 是可入库、可复现的合成评测集，是 CI 的主门。
- spike-retrieval 在本机真实文档语料上跑同一套检索指标（Recall@10 / nDCG@10）。
- 若真实集与合成集**趋势背离**（合成集涨、真实集不动，或相反），说明合成集已失真，需回到 evals 修正——这是本工具的核心价值：给合成评测集一个现实锚点。

## 数据都是本地、gitignored

真实 `corpus.jsonl` / `vectors.bin` / 评测集均由本机生成，已在 `.gitignore`（BETA-26 段）排除，**不入库**。仓库里只有代码与 `tests/` 的完整性校验。

## 构建与依赖

默认构建**不编 llama-cpp**（无需 cmake，workspace 构建更快）：

- `build-corpus` 不调模型，默认 feature 即可编、可跑。
- `embed-corpus` / `run-retrieval` 调本地嵌入模型，需开 `llama-cpp`（mac 上一般用 `metal`，它会自动带上 `llama-cpp`）。

## 跑法

```bash
# 1) 冻结语料（遍历 $HOME，默认无需任何 feature）
cargo run -p spike-retrieval --bin build-corpus            # 可传 root 参数；MAX_DOCS 环境变量调上限

# 2) 全语料 embedding（需模型 → 开 metal/llama-cpp）
cargo run -p spike-retrieval --bin embed-corpus --features metal

# 3) 三路检索对比（FTS5-only / vector-only / hybrid），输出分桶 Recall@10 / nDCG@10
cargo run -p spike-retrieval --bin run-retrieval --features metal
```

mac 以外（无 Metal）用 `--features llama-cpp` 替代 `--features metal`。
