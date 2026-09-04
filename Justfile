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

clean:
    @cargo clean
    @rm -rf dist
