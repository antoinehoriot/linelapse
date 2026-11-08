#!/usr/bin/env bash
# Run the CI lint gate (fmt + clippy -D warnings) with the latest stable Rust, in Docker.
# Catches lints that a newer clippy adds before CI does (the local toolchain may lag).
set -euo pipefail
cd "$(dirname "$0")/.."
docker run --rm \
  -v "$PWD":/src:ro \
  -v linelapse-cargo-registry:/usr/local/cargo/registry \
  -v linelapse-target-:/target \
  -e CARGO_TARGET_DIR=/target \
  -w /src \
  rust:1-bookworm bash -euo pipefail -c '
    rustup component add clippy rustfmt >/dev/null 2>&1
    apt-get update -qq >/dev/null
    apt-get install -y -qq --no-install-recommends \
      libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libxcb1-dev \
      libfontconfig-dev libfreetype-dev libvulkan-dev >/dev/null 2>&1
    rustc --version
    cargo fmt -- --check
    cargo clippy --release --locked --all-targets -- -D warnings
  '
