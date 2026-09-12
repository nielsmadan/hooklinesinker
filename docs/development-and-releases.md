# Development and releases

## Prepare and check the checkout

Install Rust with rustfmt and Clippy, Just, Lefthook, Python 3.9+, and Node 22.12+ with npm.
CI uses Node 24. [`just setup`](../Justfile) runs `cargo fetch --locked`, `npm ci`,
`lefthook install`, then `just doctor`. Setup verifies prerequisites; it does not run tests.
[`just doctor`](../scripts/doctor.sh) checks tools and installed Git hooks without installing
anything. This is distinct from `hooklinesinker doctor`, which diagnoses an installation.

| Command | Coverage |
|---|---|
| `just check` | Adapter checks, Python development/release-tool tests, Rust formatting, Clippy and Cargo tests. |
| `just check-adapters` | TypeScript type checking and adapter lint. |
| `just test` | Cargo unit, CLI, integration, example and adapter tests. |
| `just lint` | Clippy for all targets/features with warnings denied, plus adapter lint. |
| `just format` | Rust formatting. |

The [Lefthook configuration](../lefthook.yml) checks formatting and Clippy for staged Rust changes;
pre-push runs `just check`. The [main/PR CI workflow](../.github/workflows/ci.yml) runs the
same full checks and additionally builds the four release targets.

[`package.json`](../package.json) has three development dependencies: TypeScript, Node types
and Oxlint. Run `npm ci` after their lockfile changes. [`tsconfig.json`](../tsconfig.json)
uses strict checking, `noEmit`, and erasable TypeScript syntax. Oxlint enables
`typescript/no-explicit-any`. These tools add no runtime dependencies to the installed
adapters, whose host interfaces cover the methods they use and validate dynamic payloads.

[`tests/ts_adapters.rs`](../tests/ts_adapters.rs) drives the
[Node harness](../tests/adapters_harness.mjs) against stub hosts and a recording executable.
Those tests emit `SKIP` and pass if Node is missing or older than 22.6. A green Cargo run on
such a machine does not verify adapter behavior; the development-tool requirement is higher.

## Iterate on the active helper

`just install` refreshes Cargo's command; `install --consumer NAME` separately promotes it
into the shared installation. Promotion reuses equal versions. For local iteration,
`just dev-install` builds release mode and atomically replaces the binary targeted by the
existing active symlink. It affects all consumers and requires an existing activation.
It neither advances the version nor refreshes hook configuration or TypeScript adapter files;
run hook installation separately when those change. See [installation](installation.md).

## Build distributable artifacts

[`just build-release`](../scripts/build-release.sh) recreates `dist/`, builds
`aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu` and
`x86_64-unknown-linux-gnu`, then writes `SHA256SUMS`. Both Apple targets become
`hooklinesinker-macos-universal` through `lipo`; Linux binaries are named
`hooklinesinker-linux-aarch64` and `hooklinesinker-linux-x86_64`.

Install the requested Rust targets and appropriate cross linkers first; a complete local
build also needs Apple's tooling. Pass targets to build only what the host supports, e.g.
`just build-release x86_64-unknown-linux-gnu`. A macOS artifact requires both Apple targets.
Verify a staged/downloaded artifact against its `SHA256SUMS` before executing or bundling it.

## Prepare and publish a release

Run from clean `main` with complete Git history, an up-to-date origin history, matching
latest local/origin version tags, and Git credentials able to push. Local commits ahead of
origin are allowed and included in the push. Origin must match the configured repository
and use one matching fetch/push destination. Policy lives in
[`scripts/release.json`](../scripts/release.json); [`release.py`](../scripts/release.py)
enforces it without requiring a local GitHub CLI or GitHub token.

```sh
just release
just release minor
just release 1.2.0
just release --dry-run
just release patch --yes
```

The helper derives its base from the latest numeric `vMAJOR.MINOR.PATCH` tag, not from
`Cargo.toml`. Features propose minor, fixes propose patch, and breaking changes propose
major (minor during `0.x`); maintenance-only changes need an explicit bump. The initial
proposal is `1.0.0`. Editing the Cargo version alone does not advance the release baseline.

The normal command runs `just check`, then shows the proposal for confirmation. Enter `y`
to proceed, a version or `patch`/`minor`/`major` to revise it, or Enter to cancel.
`--dry-run` still checks tools and reads local/origin state, but skips tests, edits and
publication. `--yes` confirms unattended use; other nonterminal invocations fail.

After confirmation, it rechecks the checkout and remote state. Then
[`prepare_release.py`](../scripts/prepare_release.py) updates `Cargo.toml` and uses offline
Cargo metadata to update `Cargo.lock` with the existing resolution. The helper commits
changed release files, creates an annotated tag and atomically pushes `main` and that tag.
It prints a tag-filtered Actions URL and returns after the push.
The [tag workflow](../.github/workflows/release.yml) builds all four targets, assembles the
three artifacts and checksum manifest, verifies the macOS binary's compiled version matches
the tag, then automatically publishes with `gh release create --verify-tag` using CI's GitHub token. This
workflow does not run the full test/lint suite; local release checks and the separate
main/PR workflow provide those checks. Local success confirms the push, not finished artifacts.

Wait for both CI and the release workflow to succeed, then verify the published assets. Consumers
must separately update their helper version/source pins and verify release artifacts in their
own repositories; publishing does not update Juggler or ringleader automatically.

Failed preparation or push leaves local files, commits or tags available for inspection.
A failed workflow leaves the remote tag in place. Inspect the Actions failure and fix or
rerun the workflow; never replace a public tag. The local helper does not automatically resume.
