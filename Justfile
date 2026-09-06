[private]
default:
    @just --list

test:
    @cargo test

lint:
    @cargo clippy --all-targets --all-features -- -D warnings

fmt:
    @cargo fmt

# Everything CI runs.
check:
    @cargo fmt --check
    @cargo clippy --all-targets --all-features -- -D warnings
    @cargo test

# Install git hooks (lefthook).
hooks:
    @lefthook install

# Release artifacts + SHA256SUMS in dist/. Pass targets to build a subset.
release *TARGETS:
    @bash scripts/build-release.sh {{TARGETS}}

# Build and drop a fresh binary straight onto the active install, bypassing version
# promotion (which reuses an equal version and would ignore a rebuilt one). For local
# iteration only — a real change ships as a version bump + release.
dev-install:
    #!/usr/bin/env bash
    set -euo pipefail
    data="${XDG_DATA_HOME:-$HOME/.local/share}/hooklinesinker"
    link="$data/bin/hooklinesinker"
    if [ ! -L "$link" ]; then
        echo "no active hooklinesinker install at $link" >&2
        echo "activate one first: hooklinesinker install --consumer <name>" >&2
        exit 1
    fi
    target="$(python3 -c 'import os,sys; print(os.path.realpath(sys.argv[1]))' "$link")"
    cargo build --release
    tmp="$(mktemp "$(dirname "$target")/hooklinesinker.XXXXXX")"
    cp target/release/hooklinesinker "$tmp"
    chmod +x "$tmp"
    mv -f "$tmp" "$target"
    echo "refreshed $target"
    "$link" version --json

clean:
    @cargo clean
    @rm -rf dist
