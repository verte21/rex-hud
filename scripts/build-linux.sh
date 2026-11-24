#!/usr/bin/env sh
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
PROJECT_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)"
OUTPUT_DIR="${1:-$PROJECT_DIR/dist}"
OUTPUT_BIN="$OUTPUT_DIR/rex-hud-linux-x86_64"

mkdir -p "$OUTPUT_DIR"

docker run --rm --platform linux/amd64 \
  -v "$PROJECT_DIR:/src" \
  -v "$OUTPUT_DIR:/out" \
  -v rex-hud-linux-cargo-cache:/usr/local/cargo/registry \
  -v rex-hud-linux-target-cache:/target \
  -e CARGO_TARGET_DIR=/target \
  -w /src \
  rust:1.92-alpine \
  sh -eu -c '
    apk add --no-cache musl-dev >/dev/null
    cargo build --release --target x86_64-unknown-linux-musl
    cp /target/x86_64-unknown-linux-musl/release/rex-hud /out/rex-hud-linux-x86_64
  '

chmod +x "$OUTPUT_BIN"
echo "Built static Linux x86_64 binary: $OUTPUT_BIN"
