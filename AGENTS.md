# AGENTS.md

Rust CLI that owns the agent status hooks for Claude Code, Codex, OpenCode and Pi, keeps a
ledger of live sessions, and serves it to registered consumers (Juggler, ringleader).
`README.md` documents the surface; this file is the working brief.

## Commands

```sh
just check      # everything CI runs: cargo fmt --check + clippy -D warnings + cargo test
just test       # cargo test: unit + cli + integration + the node-backed adapter tests
just lint       # cargo clippy --all-targets --all-features -- -D warnings
just fmt
just hooks      # install the git hooks (lefthook)
just release    # dist/ artifacts for all four targets + SHA256SUMS
```

`cargo test --test ts_adapters` needs node 22.6+ for type stripping; without it those tests
print a SKIP line and pass, so a green run on a node-less machine proves less than it looks.

## Protocol

One `protocol` major (currently 1) versions everything a consumer reads. `src/protocol.rs` is
the wire contract: camelCase keys, kebab-case agents, snake_case phases. Consumers refuse an
envelope with an unknown `protocol` and skip individual records with one.

Changing a field in `src/protocol.rs` changes an installed-binary-to-installed-app contract —
Juggler decodes it in `HooklinesinkerStatus.swift`, ringleader in `presence.py`. Additive
optional fields are safe; anything else needs a protocol bump and both consumers updated.

## Constraints

- **Never touch Codex's `config.toml`.** Its `trusted_hash` entries belong to the host app
  (Juggler computes and removes its own). This binary owns `hooks.json` only.
- **Hooks must not block the agent.** Every path out of `ingest` exits 0, bounded by the
  hook timeouts in `src/hooks.rs` and the adapters' own kill timers. A failure is recorded in
  `health.json`, never surfaced as a nonzero exit into someone's editor.
- **Reconcile, never overwrite.** Installers rewrite only the group carrying this tool's
  generated marker, and preserve foreign entries and unknown shapes verbatim
  (`HookState::Unsupported` exists so an unfamiliar config is left alone rather than rewritten).
- **State is private.** `$XDG_STATE_HOME/hooklinesinker` is read through the CLI's JSON
  commands, never by a consumer opening files. Keep the JSON envelopes stable and complete —
  in particular, `problems` must reach the caller rather than being swallowed.
- **Uninstall is scoped.** `uninstall --consumer X` removes only X; hooks and the active
  binary go only when the last consumer leaves.
- **The active binary is shared.** `install` reuses a newer protocol-compatible active version
  rather than downgrading it, so install order between consumers never matters.

## Layout

| File | Holds |
|---|---|
| `src/protocol.rs` | the wire types and `PROTOCOL_VERSION` |
| `src/state.rs` | the status ledger, ingest, sweeping, health problems |
| `src/hooks.rs` | per-agent hook installation, status and drift |
| `src/consumers.rs` | consumer registration and validation |
| `src/install.rs` | versioned self-install, activation, scoped uninstall |
| `src/sinks.rs` | POST-per-event fanout to consumer sinks |
| `assets/*.ts` | the OpenCode plugin and Pi extension, with `__HOOKLINESINKER_BIN__` |
| `tests/assets_harness.mjs` | node harness the adapter tests drive |
