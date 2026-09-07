# AGENTS.md

Rust CLI that owns the agent status hooks for Claude Code, Codex, OpenCode, Pi, Factory Droid,
Qwen Code and Kimi Code CLI, keeps a ledger of live sessions, and serves it to registered
consumers (Juggler, ringleader).
`README.md` documents the surface; this file is the working brief.

## Commands

```sh
just setup      # fetch dependencies, install Git hooks, and verify the checkout
just doctor     # check Rust tools, Python, Node, and Git hooks
just check      # formatting, Clippy, Rust tests, and development/release-tool tests
just test       # cargo test: unit + cli + integration + the node-backed adapter tests
just lint       # cargo clippy --all-targets --all-features -- -D warnings
just format
just build-release  # dist/ artifacts for all four targets + SHA256SUMS
just release        # propose/confirm a version, then create a draft GitHub release
just release --dry-run  # preview without checks, edits, or publication
```

`cargo test --test ts_adapters` needs node 22.6+ for type stripping; without it those tests
print a SKIP line and pass, so a green run on a node-less machine proves less than it looks.
`just doctor` checks this requirement. Development tooling also requires Python 3.9+.
The pre-push hook runs `just check`; CI additionally builds all four platform targets.

`just release minor`, `just release patch`, and exact versions use the same confirmation flow.
Release from a clean, current `main` checkout with complete history and matching origin tags.
After checks and confirmation, preparation updates `Cargo.toml` and `Cargo.lock`, commits them,
and atomically pushes the branch and tag. The command waits for the existing workflow's draft
release and reports its URL. Publishing the draft is manual; never replace a public tag.
The README documents prerequisites and failure recovery.

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
- **Only `ingest` deletes from the ledger.** `running()`, `sessions --json` and `doctor`
  filter dead bindings out of their results but leave the files alone; the sweep inside
  `ingest` removes them and fans out the `running:false` event push consumers depend on. A
  read that deleted would swallow that event, and Juggler has no re-hydration timer.
- **An empty `session.id` is never recorded.** Such an event is normalized and fanned out to
  sinks (consumers key rows on terminal identity) but kept out of the ledger: its binding is
  hashed over an empty id and would outlive every real session in the same live process.
- **State is private.** `$XDG_STATE_HOME/hooklinesinker` is read through the CLI's JSON
  commands, never by a consumer opening files. Keep the JSON envelopes stable and complete —
  in particular, `problems` must reach the caller rather than being swallowed.
- **Uninstall is scoped.** `uninstall --consumer X` removes only X; hooks and the active
  binary go only when the last consumer leaves.
- **The active binary is shared.** `install` reuses a newer protocol-compatible active version
  rather than downgrading it, so install order between consumers never matters.
- **Kimi's `config.toml` bricks entirely on one malformed `[[hooks]]` entry.** Its installer
  uses `toml_edit`, writes only the strict `{event, command, timeout}` shape, verifies its own
  rendering re-parses before ever touching disk, and removes only exact canonical `command`
  matches — never the fuzzy near-match the JSON installers use.

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
