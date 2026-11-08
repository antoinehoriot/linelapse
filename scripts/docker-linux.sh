#!/usr/bin/env bash
# Build and test linelapse on Linux inside Docker, at no cost (no CI minutes).
#
#   scripts/docker-linux.sh            # native arch of the Docker host (arm64 on Apple Silicon)
#   PLATFORM=linux/amd64 scripts/docker-linux.sh   # x86_64, emulated: much slower
#
# Proves that it compiles, that the test suite passes, that indexing is exact on a real
# repository (--check), and that headless video export runs (--record). It cannot
# check the GPUI window itself: there is no display in the container.
# Outputs (binary, sample video) land in target/docker-linux-out/.
set -euo pipefail
cd "$(dirname "$0")/.."
PLATFORM=${PLATFORM:-}
OUT="$PWD/target/docker-linux-out"
mkdir -p "$OUT"
docker run --rm ${PLATFORM:+--platform "$PLATFORM"} \
  -v "$PWD":/src:ro \
  -v "$OUT":/out \
  -v linelapse-cargo-registry:/usr/local/cargo/registry \
  -v linelapse-target-${PLATFORM//\//-}:/target \
  -e CARGO_TARGET_DIR=/target \
  -w /src \
  rust:1-bookworm bash -euo pipefail -c '
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
      libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libxcb1-dev \
      libfontconfig-dev libfreetype-dev libvulkan-dev git ffmpeg >/dev/null
    rustc --version
    cargo build --release --locked 2>&1 | tail -3
    cargo test --release --locked 2>&1 | grep -E "^test result|FAILED|panicked" || true
    cargo test --release --locked >/dev/null 2>&1
    BIN=/target/release/linelapse
    $BIN https://github.com/BurntSushi/ripgrep --check
    $BIN https://github.com/BurntSushi/ripgrep --record /out/ripgrep-linux.mp4 \
      --size 1280x720 --fps 30 --seconds 6
    cp $BIN /out/linelapse-linux-$(uname -m)
    file /out/linelapse-linux-$(uname -m) || true
  '
echo "outputs in $OUT"
