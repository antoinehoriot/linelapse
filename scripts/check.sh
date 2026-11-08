#!/usr/bin/env bash
# Gate for every commit: formatting, lints as errors, full test suite.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt -- --check
cargo clippy --release --all-targets -- -D warnings
cargo nextest run --release
