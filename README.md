# hooklinesinker

I am working on an app ([Juggler](https://github.com/nielsmadan/juggler)) and a CLI
([ringleader](https://github.com/nielsmadan/ringleader)) that both consume agent status hook
events for multiple agents. I did not want to register both the app and the CLI in every hook
config of every agent. Every problem can be solved with another layer of abstraction, so I built
this little plumbing CLI tool. Hook events from any supported agent (Claude Code, Codex,
OpenCode, Pi, Factory Droid, Qwen Code, Kimi Code CLI) get sent to hooklinesinker, which keeps
its own queryable state on session status and forwards normalized events. Other apps can register
with hooklinesinker to receive the forwarded events.

This is just a little piece of plumbing, but if you're building anything that needs agent status
hook events, it might save you some time. It can be installed standalone or integrated into your
own app; see [Integrating](#integrating).

In the future it might support more than status events
([why only status events for now](docs/decisions/0001-status-events-only.md)). Let me know if you have a
use case and which events you would be interested in.

Today's consumers:

- **[Juggler](https://github.com/nielsmadan/juggler)** bundles it and subscribes to a sink.
- **[ringleader](https://github.com/nielsmadan/ringleader)** bundles it and polls `sessions --json`.
- You can also install it on its own — see *Standalone install*.

[Install](#install) · [Commands](#commands) · [Protocol](#protocol-1) ·
[Integrating](#integrating) · [Paths](#paths) · [Diagnostics](#diagnostics) ·
[Development](#development) · [Releasing](#releasing)

For implementation flows, design decisions, and manual check procedures, start at the
[documentation index](docs/overview.md).

## Install

You normally do not install this yourself. A consumer ships the binary and activates it on
request (`rl data presence install`, or Juggler's integration hub). The first activation copies
the binary into `~/.local/share/hooklinesinker/versions/<version>/`, points
`~/.local/share/hooklinesinker/bin/hooklinesinker` at it, and registers the consumer.

### Standalone install

```sh
just install                              # build and install the current checkout via Cargo
hooklinesinker install --consumer me       # activate + register
hooklinesinker hooks install --agent claude
hooklinesinker sessions --json
```

Re-running `just install` replaces the Cargo-installed command with the current source build.
`just uninstall` removes that Cargo installation. Consumer registration and the shared active
binary have their own lifecycle: `hooklinesinker uninstall --consumer me` removes your consumer,
and the hooks and active binary are removed when the last consumer leaves.

`hooklinesinker install --consumer NAME` is idempotent and cooperative: a newer protocol-compatible
binary that is already active is reused rather than downgraded. Hook configuration is updated
separately with `hooks install --agent AGENT`, which reconciles owned entries in place.
See [installation](docs/installation.md) for the three installation lifecycles.

## Commands

```sh
hooklinesinker version [--json]                  # version + protocol major
hooklinesinker sessions --json                   # every live session (the point of all this)
hooklinesinker consumers --json                  # who is registered
hooklinesinker doctor [--json]                   # every self-check; nonzero on a real fault
hooklinesinker install --consumer NAME [--sink URL]
hooklinesinker uninstall --consumer NAME
hooklinesinker hooks install|status|uninstall --agent claude|codex|opencode|pi|droid|qwen|kimi
hooklinesinker ingest --agent AGENT --event EVENT   # what the installed hooks call
```

## Protocol 1

Everything a consumer reads is versioned by a `protocol` major. Consumers must refuse an
envelope whose `protocol` is not one they speak, and skip individual records whose own
`protocol` they do not speak, rather than guessing.

`sessions --json` emits:

```json
{
  "protocol": 1,
  "sessions": [
    {
      "protocol": 1,
      "bindingId": "…",
      "agent": "claude",
      "event": "UserPromptSubmit",
      "phase": "working",
      "running": true,
      "observedAt": "2026-09-04T00:00:00Z",
      "session": { "id": "…", "cwd": "/path", "transcriptPath": null },
      "process": { "pid": 42, "startedAt": "…", "host": "…" },
      "terminal": { "sessionId": "…", "terminalType": "iterm", "kittyListenOn": null, "kittyPid": null },
      "tmux": { "pane": "%1", "sessionName": "main" },
      "git": { "branch": "main", "repo": "hooklinesinker" },
      "remoteHost": null
    }
  ],
  "problems": [{ "observedAt": "…", "message": "…" }]
}
```

Keys are camelCase. `agent` is kebab-case (`claude`, `codex`, `opencode`, `pi`, `droid`, `qwen`,
`kimi`). `phase` is snake_case (`idle`, `working`, `permission`, `compacting`, `unknown`).
Consumers should map unfamiliar phase values to `unknown` when the rest of the record can be
decoded; the Rust enum itself does not provide that fallback.

**A session id is only unique within one agent.** Key on `(agent, session.id)`, or on
`bindingId`, which also separates two terminals driving the same native session.

An event whose `session.id` is empty (an agent announcing itself before its session id exists)
is delivered to sinks but never stored, so it can never appear in `sessions --json`.

`sessions --json` and `doctor` never mutate the ledger. A record whose process is gone is
filtered out of both, and removed by the next `ingest`, which fans out one `running:false`
event as it goes, so a poll landing between the kill and the next hook event cannot swallow
that notification.

`problems` is never dropped on the floor: a consumer that cannot answer must say so rather than
report an empty, healthy-looking result.

**Sinks.** A consumer that registers with `--sink URL` gets each event POSTed as JSON to that URL
as it happens, instead of polling. Sink failures are recorded as problems and are not retried.
Network timeouts bound each delivery phase; handled delivery failures do not fail the agent hook.

## Integrating

Two models, matching the two consumers above. Runnable, integration-tested examples live in
[`examples/`](examples/).

### Pull — CLIs, scripts, anything short-lived

Do not register a sink; ask when you care.

1. Locate or ship the binary (see *Bundling* below) and run
   `hooklinesinker install --consumer yourname` once (`yourname` must start with a lowercase
   letter or digit, and contain only lowercase letters, digits, `_` or `-`). It is idempotent
   and cooperative, so running it on every startup is safe.
2. Run `hooklinesinker hooks install --agent <agent>` for the agents your users approve. It
   edits their agent config, so ask first.
3. When you need state, run `hooklinesinker sessions --json` and parse the envelope: refuse a
   `protocol` you do not speak, skip records you do not understand, key on `(agent, session.id)`
   or `bindingId`, and surface `problems` instead of treating them as an empty result.

This is ringleader's model (`ringleader/presence.py`).

### Push — long-running apps (menu bar, Electron, Tauri, native)

Register a sink and receive every event as it happens.

1. Start a localhost HTTP server. Each event arrives as one `POST` with a `StatusEvent` JSON
   body. Answer 2xx fast: the sender has separate 200 ms connection, response and body timeouts
   and never retries. Do your real work after responding.
2. Register: `hooklinesinker install --consumer yourname --sink http://127.0.0.1:PORT/hook`.
3. Hydrate: after your server is listening, run `sessions --json` once, applying the same
   protocol refusal, per-record skip, and `problems` handling as the Pull model, and feed the
   records through the same code path as live events, deduplicating by `bindingId` — a live
   event racing your hydration must not create two rows.
4. A `running:false` event removes the binding it names. Missed events are recoverable by
   re-running `sessions --json`; sink delivery is best-effort by design.

This is Juggler's model (`juggler/Services/HooklinesinkerClient.swift`, `HookServer.swift`).

### Bundling

- **Node / Electron**: ship per-platform binaries (e.g. `extraResources`), spawn with
  `child_process.execFile` (argument arrays, never a shell string); the sink server is a plain
  `http.createServer` in the main process. `examples/sink-server/` is exactly this pattern as a
  single dependency-free script.
- **Tauri / Rust**: ship it as a sidecar (`externalBin`) or spawn via `std::process::Command`;
  any small HTTP listener works for the sink.
- **Native macOS (Swift)**: bundle as an auxiliary executable under `Contents/MacOS`, spawn via
  `Process` with argument arrays, and mind code signing/notarization if you distribute: the
  nested binary is signed as part of your app. Juggler is the worked example.
- **Everywhere**: talk to it only through its CLI JSON (never read the state files directly),
  and verify a downloaded release artifact against `SHA256SUMS` before executing it.

## Paths

| Path | Holds |
|---|---|
| `$XDG_DATA_HOME/hooklinesinker/versions/<v>/hooklinesinker` | each installed version |
| `$XDG_DATA_HOME/hooklinesinker/bin/hooklinesinker` | symlink to the active version |
| `$XDG_STATE_HOME/hooklinesinker/status/` | latest recorded status per binding; dead records await ingest sweeping |
| `$XDG_STATE_HOME/hooklinesinker/consumers/` | one JSON record per consumer |
| `$XDG_STATE_HOME/hooklinesinker/health.json` | recent problems |
| `~/.factory/hooks.json` | Factory Droid hooks (top-level event map, no `hooks` wrapper) |
| `$QWEN_HOME/settings.json` (default `~/.qwen`) | Qwen Code hooks |
| `$KIMI_CODE_HOME/config.toml` (default `~/.kimi-code`) | Kimi Code CLI hooks (`[[hooks]]`) |

Without `XDG_*` set these fall back to `~/.local/share` and `~/.local/state`. Both roots are
created `0700`.

Hooks are written into each agent's own configuration, and nowhere else:
`~/.claude/settings.json`, `~/.codex/hooks.json`,
`$OPENCODE_CONFIG_DIR/plugins/hooklinesinker-opencode.ts`,
`$PI_CODING_AGENT_DIR/extensions/hooklinesinker-pi.ts`, `~/.factory/hooks.json`,
`$QWEN_HOME/settings.json`, and `$KIMI_CODE_HOME/config.toml`. Entries this tool wrote are
identified by their exact canonical `command` (and the generated OpenCode/Pi plugin files
also carry an ownership marker), so an install reconciles its own entries and leaves
everyone else's alone.

Kimi's hook events (`TurnStarted`, `SessionHeartbeat`, etc.) require Kimi Code CLI **0.32.0** or
later; `hooks install --agent kimi` writes hooks regardless, but they only fire on a new-enough
CLI.

Droid reads its hooks at startup, so a `droid` session that was already running when the hooks
were installed will not report anything until it is restarted (it warns about the change in its
`/hooks` UI).

Codex's `Interrupt` hook returns an interrupted turn to idle while keeping its session live.
Existing installations need `hooks install --agent codex` again to register this event, then
approval in Codex's `/hooks` or the consumer's trust UI. Codex caps `Interrupt` and `SessionEnd`
at three seconds, including the timeout used in their trust hashes.
Codex's `config.toml` (including its `trusted_hash` machinery) is **not** touched. Trusting a
hook is the host application's business.

## Consumer ownership

A consumer is a name plus a protocol, a capability list, and an optional sink. Registration is
additive and removal is scoped:

- `install --consumer X` adds X and activates a compatible binary. Running it again from a second
  tool adds only that tool's registration.
- `uninstall --consumer X` removes X. Hooks and the active binary stay as long as **any** other
  consumer is registered.
- Removing the last consumer uninstalls the hooks for every agent and drops the active
  binary symlink.

Removing one consumer preserves the installation used by the others. After the last consumer
leaves, old version directories and stored state remain; see [removal](docs/installation.md#remove-a-consumer).

## The privacy boundary

The state above is private to this binary. Consumers read it **only** through the CLI's JSON
commands, never by opening the files, so the protocol is the whole contract and the on-disk
shape stays free to change. Nothing is sent anywhere except to a sink URL a consumer explicitly
registered.

## Diagnostics

```sh
hooklinesinker doctor           # version, permissions, active version, hook drift, parse faults
hooklinesinker doctor --json    # the same, machine-readable, with an exitCode field
hooklinesinker hooks status --agent codex --json
hooklinesinker consumers --json
```

`doctor` exits nonzero on a real fault (unparseable records, drifted or unsupported hooks, a
state root with the wrong permissions) and zero otherwise. Its `dead_records` check counts
records whose process is gone; it does not remove them.

## Development

Install Rust with rustfmt and Clippy, Just, Lefthook, Python 3.9+, and Node 22.12+ with npm
(Node 24 recommended), then run:

```sh
just setup
just check
```

`just setup` installs the pinned adapter tools and Git hooks. `just check` runs TypeScript,
explicit-`any` lint, Python tooling tests, Rust formatting, Clippy, and Cargo tests.
For focused checks, adapter development, and local shared-binary iteration, see
[development and releases](docs/development-and-releases.md).

## Releasing

```sh
just release --dry-run
just release
```

Run from a clean, current `main` checkout. The helper proposes a version from Git tags, runs
checks, and asks for confirmation before preparing and pushing the release commit and tag.
It needs Git credentials; GitHub CLI authentication is used only inside Actions.

The tag workflow builds and automatically publishes the release with macOS/Linux binaries
and `SHA256SUMS`. Wait for both CI and the release workflow to succeed. See
[the release procedure](docs/development-and-releases.md#prepare-and-publish-a-release)
for overrides, artifact verification, consumer updates, and failure recovery.
