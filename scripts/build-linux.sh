#!/usr/bin/env sh
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
PROJECT_DIR="$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)"
OUTPUT_DIR="${1:-$PROJECT_DIR/dist}"
OUTPUT_BIN="$OUTPUT_DIR/rex-hud-linux-x86_64"

if [ -n "${CONTAINER_ENGINE:-}" ]; then
  ENGINE="$CONTAINER_ENGINE"
elif command -v docker >/dev/null 2>&1; then
  ENGINE="docker"
elif command -v podman >/dev/null 2>&1; then
  ENGINE="podman"
else
  echo "Error: neither docker nor podman was found in PATH." >&2
  exit 1
fi

mkdir -p "$OUTPUT_DIR"

ENGINE_EXTRA_ARGS=""
if [ "$(basename -- "$ENGINE")" = "podman" ]; then
  # Avoid SELinux relabeling issues on Fedora/Bazzite atomic hosts.
  ENGINE_EXTRA_ARGS="--security-opt label=disable"
fi

# shellcheck disable=SC2086
"$ENGINE" run --rm --platform linux/amd64 \
  $ENGINE_EXTRA_ARGS \
  -v "$PROJECT_DIR:/src" \
  -v "$OUTPUT_DIR:/out" \
  -v rex-hud-linux-cargo-cache:/usr/local/cargo/registry \
  -v rex-hud-linux-target-cache:/target \
  -e CARGO_TARGET_DIR=/target \
  -w /src \
  docker.io/library/rust:1.92-alpine \
  sh -eu -c '
    apk add --no-cache musl-dev >/dev/null
    cargo build --release --target x86_64-unknown-linux-musl
    cp /target/x86_64-unknown-linux-musl/release/rex-hud /out/rex-hud-linux-x86_64
  '

chmod +x "$OUTPUT_BIN"
echo "Built static Linux x86_64 binary ($ENGINE): $OUTPUT_BIN"
