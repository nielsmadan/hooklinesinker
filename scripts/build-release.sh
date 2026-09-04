#!/usr/bin/env bash
# Build the release artifacts consumers vendor: one universal macOS binary, one binary
# per Linux architecture, and the SHA-256 manifest everything downstream verifies against.
#
#   scripts/build-release.sh [target...]
#
# With no arguments it builds all four supported targets, which needs both Apple targets
# and both Linux targets installed (`rustup target add`) plus a cross linker for Linux.
# Pass a subset to build only what this host can produce.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dist="$repo_root/dist"

ALL_TARGETS=(
    aarch64-apple-darwin
    x86_64-apple-darwin
    aarch64-unknown-linux-gnu
    x86_64-unknown-linux-gnu
)

targets=("${ALL_TARGETS[@]}")
if [ "$#" -gt 0 ]; then
    targets=("$@")
fi

if command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$@"; }
else
    sha256() { sha256sum "$@"; }
fi

built_path() {
    echo "$repo_root/target/$1/release/hooklinesinker"
}

rm -rf "$dist"
mkdir -p "$dist"

installed="$(rustup target list --installed)"
for target in "${targets[@]}"; do
    if ! grep -qx "$target" <<<"$installed"; then
        echo "target $target is not installed — run: rustup target add $target" >&2
        exit 1
    fi
done

for target in "${targets[@]}"; do
    echo "Building $target..."
    cargo build --release --target "$target"
done

has() {
    [[ " ${targets[*]} " == *" $1 "* ]]
}

if has aarch64-apple-darwin && has x86_64-apple-darwin; then
    echo "Combining the macOS targets into a universal binary..."
    lipo -create -output "$dist/hooklinesinker-macos-universal" \
        "$(built_path aarch64-apple-darwin)" \
        "$(built_path x86_64-apple-darwin)"
    lipo -info "$dist/hooklinesinker-macos-universal"
fi

if has aarch64-unknown-linux-gnu; then
    cp "$(built_path aarch64-unknown-linux-gnu)" "$dist/hooklinesinker-linux-aarch64"
fi

if has x86_64-unknown-linux-gnu; then
    cp "$(built_path x86_64-unknown-linux-gnu)" "$dist/hooklinesinker-linux-x86_64"
fi

shopt -s nullglob
produced=("$dist"/hooklinesinker-*)
if [ "${#produced[@]}" -eq 0 ]; then
    echo "no artifacts produced from: ${targets[*]}" >&2
    echo "a macOS artifact needs both apple-darwin targets (they are lipo'd together)" >&2
    exit 1
fi

chmod +x "${produced[@]}"

cd "$dist"
sha256 hooklinesinker-* >SHA256SUMS
echo ""
echo "=== $dist ==="
cat SHA256SUMS
