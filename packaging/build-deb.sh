#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target}"

if ! command -v cargo-deb >/dev/null 2>&1; then
    echo "Installing cargo-deb..."
    cargo install cargo-deb --locked
fi

echo "Building release binary..."
cargo build --release

echo "Building .deb package..."
cargo deb --no-build

deb="$(ls -1 "$CARGO_TARGET_DIR/debian/cam-shim_"*.deb | tail -n1)"
echo ""
echo "Built: $deb"
echo "Install: sudo dpkg -i $deb"
