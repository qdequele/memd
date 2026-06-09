#!/usr/bin/env bash
# memd installer: build from source, then run the one-command setup.
# Usage: ./install.sh
set -euo pipefail

cd "$(dirname "$0")"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo (Rust toolchain) not found. Install Rust from https://rustup.rs and re-run." >&2
  exit 1
fi

echo "==> Building memd (release)…"
cargo build --release

echo "==> Running memd setup…"
exec ./target/release/memd setup "$@"
