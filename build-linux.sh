#!/usr/bin/env bash
set -euo pipefail
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$project_dir"
mkdir -p logs bin
if ! command -v nasm >/dev/null 2>&1; then
    export PATH="$project_dir/tools/nasm/bin:$PATH"
fi
if ! command -v nasm >/dev/null 2>&1; then
    echo "缺少 NASM。Arch/EndeavourOS 可用 sudo pacman -S --needed nasm 安装。" >&2
    exit 1
fi
profile=debug
for arg in "$@"; do
    if [[ "$arg" == --release ]]; then profile=release; fi
done
echo "正在构建；完整输出：$project_dir/logs/build-linux.log"
if ! cargo build --locked --bin OpenUUYC -j "${CARGO_BUILD_JOBS:-1}" "$@" > logs/build-linux.log 2>&1; then
    tail -n 20 logs/build-linux.log >&2
    exit 1
fi
# Drop debug information from the launch copy while leaving Cargo artifacts
# intact for subsequent builds and tests.
launch_copy=$(mktemp bin/.OpenUUYC.XXXXXX)
trap 'rm -f -- "$launch_copy"' EXIT
objcopy --strip-debug "target/$profile/OpenUUYC" "$launch_copy"
strip --strip-unneeded "$launch_copy"
chmod +x "$launch_copy"
mv -f -- "$launch_copy" bin/OpenUUYC
echo "构建完成：./run-linux.sh"
