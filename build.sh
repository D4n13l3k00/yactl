#!/bin/sh
set -eu
cd "$(dirname "$0")"
cargo build --locked --release --target aarch64-unknown-linux-musl
mkdir -p dist
cp target/aarch64-unknown-linux-musl/release/yactl dist/yactl
"${UPX_BIN:-upx}" --best --lzma dist/yactl
"${UPX_BIN:-upx}" -t dist/yactl
