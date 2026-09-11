[private]
default:
    @just --list

setup:
    @cargo fetch --locked
    @npm ci
    @lefthook install
    @just doctor

doctor:
    @bash scripts/doctor.sh

install:
    @cargo install --path . --locked --force

uninstall:
    @cargo uninstall hooklinesinker

test:
    @cargo test

lint:
    @cargo clippy --all-targets --all-features -- -D warnings
    @npm run lint

check-adapters:
    @npm run check

format:
    @cargo fmt

# Formatting, lint, adapter type checks, Rust tests, and release-tool tests.
check:
    @just check-adapters
    @python3 -B -m unittest discover -s scripts -p 'test_*.py'
    @cargo fmt --check
    @cargo clippy --all-targets --all-features -- -D warnings
    @cargo test

# Release artifacts + SHA256SUMS in dist/. Pass targets to build a subset.
build-release *TARGETS:
    @bash scripts/build-release.sh {{TARGETS}}

[positional-arguments]
release *args:
    python3 scripts/release.py "$@"

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
