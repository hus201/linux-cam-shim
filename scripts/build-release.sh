#!/usr/bin/env bash
# Build release artifacts for GitHub Releases (binary tarball + .deb).
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

version="$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')"
arch="$(uname -m)"
dist="$root/dist/v${version}"
target="${CARGO_TARGET_DIR:-$root/target}"

mkdir -p "$dist"

echo "Building cam-shim v${version} (${arch})..."

cargo build --release

if ! command -v cargo-deb >/dev/null 2>&1; then
    echo "Installing cargo-deb..."
    cargo install cargo-deb --locked
fi

CARGO_TARGET_DIR="$target" cargo deb --no-build

deb="$(ls -1 "$target/debian/cam-shim_"*.deb | tail -n1)"
cp "$deb" "$dist/"

bundle="cam-shim-${version}-linux-amd64"
tarball="$dist/${bundle}.tar.gz"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/$bundle"
cp "$target/release/cam-shim" "$stage/$bundle/"
cp LICENSE Readme.md "$stage/$bundle/"
tar -C "$stage" -czf "$tarball" "$bundle"

(cd "$dist" && sha256sum *.deb *.tar.gz > SHA256SUMS)

echo ""
echo "Release artifacts:"
ls -lh "$dist"
echo ""
echo "Release notes: docs/releases/v${version}.md"
echo ""
echo "Publish with:"
echo "  gh release create v${version} dist/v${version}/* \\"
echo "    --title \"cam-shim v${version}\" \\"
echo "    --notes-file docs/releases/v${version}.md"
