#!/bin/bash
# 强制 Spotlight 重新索引 fixture 目录

set -e

FIXTURE_DIR="$(cd "$(dirname "$0")/files" && pwd)"

if [ ! -d "$FIXTURE_DIR" ]; then
    echo "错误: fixture 目录不存在，请先运行 generate.sh"
    exit 1
fi

echo "正在强制 Spotlight 重新索引: $FIXTURE_DIR"
mdimport "$FIXTURE_DIR"

echo "完成。你可以使用 mdfind -onlyin \"$FIXTURE_DIR\" <query> 来验证。"
