#!/usr/bin/env bash
set -uo pipefail
failed=0
probe() {
    local name=$1 hint=$2
    shift 2
    if "$@" >/dev/null 2>&1; then
        printf '  ok       %s\n' "$name"
    else
        printf '  MISSING  %s — %s\n' "$name" "$hint"
        failed=1
    fi
}
probe cargo 'install Rust: https://rustup.rs' cargo --version
probe rustc 'install Rust: https://rustup.rs' rustc --version
probe rustfmt 'run: rustup component add rustfmt' cargo fmt --version
probe clippy 'run: rustup component add clippy' cargo clippy --version
probe 'Python 3.9+' 'install Python 3.9 or newer' python3 -c 'import sys; sys.exit(sys.version_info < (3, 9))'
probe 'Node 22.12+' 'install Node 24 LTS for the adapter checks' node -e 'const [major, minor] = process.versions.node.split(".").map(Number); process.exit(major > 22 || major === 22 && minor >= 12 ? 0 : 1)'
probe npm 'install npm with Node.js' npm --version
probe lefthook 'run: brew install lefthook' lefthook version
probe 'Git hooks' 'run: just setup' lefthook check-install
exit "$failed"
