#!/bin/bash
# 生成合成测试 fixture 的包装脚本

set -e

# cargo 不在默认 PATH 时自动 source
if ! command -v cargo >/dev/null 2>&1; then
  if [[ -f "$HOME/.cargo/env" ]]; then
    . "$HOME/.cargo/env"
  fi
fi

# 编译并运行 Rust 生成器
cargo run -p scout-evals --bin fixtures -- generate "$@"
