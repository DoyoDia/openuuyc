#!/usr/bin/env bash
set -euo pipefail
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$project_dir"
if [[ ! -x bin/OpenUUYC ]]; then
    echo "请先运行 ./build-linux.sh；构建日志写入 logs/build-linux.log。" >&2
    exit 1
fi
mkdir -p logs
echo "启动 OpenUUYC，控制台日志：$project_dir/logs/gui-console.log"
exec "$project_dir/bin/OpenUUYC" "$@" > logs/gui-console.log 2>&1
