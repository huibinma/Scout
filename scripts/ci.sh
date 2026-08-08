#!/usr/bin/env bash
# Scout 本地 CI 脚本。
# PROTO-01 落地；任一工具收工前可执行，确保不引入 lint / build / test 回归。
#
# 用法：
#   bash scripts/ci.sh              # 全套
#   bash scripts/ci.sh fmt          # 只跑 fmt
#   bash scripts/ci.sh clippy       # 只跑 clippy
#   bash scripts/ci.sh build        # 只跑 build
#   bash scripts/ci.sh test         # 只跑 test
#   bash scripts/ci.sh synonym-recall  # BETA-15A 同义词召回报告

set -euo pipefail

# cargo 不在默认 PATH 时（如新 shell 未 source rc）自动 source
if ! command -v cargo >/dev/null 2>&1; then
  if [[ -f "$HOME/.cargo/env" ]]; then
    # shellcheck source=/dev/null
    . "$HOME/.cargo/env"
  else
    echo "[ci] cargo not found and $HOME/.cargo/env 不存在；先装 rustup。" >&2
    exit 127
  fi
fi

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

step() {
  echo
  echo "==> $*"
}

run_fmt() {
  step "cargo fmt --all -- --check"
  cargo fmt --all -- --check
}

run_clippy() {
  step "cargo clippy --workspace --all-targets -- -D warnings"
  cargo clippy --workspace --all-targets -- -D warnings
}

run_build() {
  step "cargo build --workspace --all-targets"
  cargo build --workspace --all-targets
}

run_test() {
  step "cargo test --workspace --all-targets"
  cargo test --workspace --all-targets
}

run_synonym_recall() {
  step "cargo run -p scout-evals --bin synonym_recall"
  cargo run -p scout-evals --bin synonym_recall
}

case "${1:-all}" in
  fmt)    run_fmt ;;
  clippy) run_clippy ;;
  build)  run_build ;;
  test)   run_test ;;
  synonym-recall) run_synonym_recall ;;
  all)
    run_fmt
    run_clippy
    run_build
    run_test
    run_synonym_recall
    ;;
  *)
    echo "未知子命令：$1" >&2
    echo "用法见脚本顶部注释。" >&2
    exit 2
    ;;
esac

step "所有检查通过 ✓"
