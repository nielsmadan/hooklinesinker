# Isolated native-agent smoke check

Use this when changing a native hook contract, updating a host pin, or closing a
blocked native-delivery check. It runs one minimal turn with the generated installed
artifact, records normalized loopback sink events and final polling output, and removes
its isolated consumer afterward. It does not approve permission requests or activate
the user's shared installation.

## Prerequisites

Build the checkout with `cargo build`. Use Python 3.9+ and a verified native executable
for the selected host. Download/verify executables through the host's official release
instructions; do not execute an installer against the real home. The
[validation matrix](../../reference/agent-validation.md) records the checked versions.

The [driver](../../../scripts/smoke-agent.py) uses a fresh directory under `/tmp`, a
neutral project directory, and a newly constructed environment. HOME, all XDG roots,
Goose/Copilot/Kilo overrides and temporary files point inside that directory. Inherited
host roots, terminal/tmux identity and project instructions are absent. Keep the root
outside a repository or any directory with ancestor agent configuration; `--root-parent`
can select another neutral temporary parent. This procedure explicitly uses process-local
HOME isolation; it never replaces or edits the actual home.

Only already-injected host credentials and `SSL_CERT_FILE` pass through. Values are never
printed by the driver or written into its environment inventory. It does not read/copy
credential stores, log in, disable TLS checks, or change permission policies. Goose also
accepts already-set `GOOSE_PROVIDER` and `GOOSE_MODEL`; configure these deliberately for
the isolated check. Devin has no credential-file fallback, so unavailable isolated auth
remains a blocker. Antigravity may automatically open its authentication flow; do not
interact with it during this check. The driver bounds every command to 45 seconds and
terminates its process group on timeout or Ctrl-C. Cleanup allows three seconds after SIGTERM,
then three after SIGKILL, and at most three more for reaping if capture pipes remain open.
Detached descendants are not signaled through unrelated process groups.

## Commands

Run from the checkout. Replace the absolute executable paths with your verified copies:

```sh
python3 scripts/smoke-agent.py devin /absolute/path/to/devin
python3 scripts/smoke-agent.py goose /absolute/path/to/goose
python3 scripts/smoke-agent.py copilot /absolute/path/to/copilot
python3 scripts/smoke-agent.py antigravity /absolute/path/to/antigravity
python3 scripts/smoke-agent.py kilo /absolute/path/to/kilo --model openrouter/openai/gpt-4.1-mini
python3 scripts/smoke-agent.py cursor /absolute/path/to/cursor-agent
```

Run only the host being investigated. Add `--prepare-only` to install/inspect its artifact
and read the native version without a model turn. `--hls /absolute/path/to/hooklinesinker`
selects another candidate. The prompt asks for `OK` without tools; a successful minimal
turn does not validate tool, permission, child, compaction, resume, or concurrent-session
behavior.

The driver executes these host commands after isolated HLS installation:

| Host | Native command |
|---|---|
| Devin | `devin --respect-workspace-trust false --print 'Reply with OK. Do not use tools.'` |
| Goose | `goose run --text 'Reply with OK. Do not use tools.'` |
| Copilot | `copilot --no-auto-update --disable-builtin-mcps --no-custom-instructions -p 'Reply with OK. Do not use tools.'` |
| Antigravity | `antigravity --log-file <isolated-root>/cli.log --print-timeout 15s -p 'Reply with OK. Do not use tools.'` |
| Kilo | `kilo run --format json --model <selected-model> 'Reply with OK. Do not use tools.'` |
| Cursor | `cursor-agent --trust --print --output-format json 'Reply with OK. Do not use tools.'` |

Cursor passes through only an already-injected `CURSOR_API_KEY` for authentication.
`--trust` applies to the disposable project. Its user hooks are installed under the
isolated HOME, because `CURSOR_CONFIG_DIR` does not redirect the hook loader.
For broader validation, use the same isolated configuration to exercise a successful
file read, a missing-file failure, a foreign `preToolUse` hook returning
`{"permission":"deny"}`, resume with `--resume <conversation_id>`, and interactive
`/compact` and exit. Capture normalized events while the process is alive to compare
polling with the sink. A deny must remain denied alongside HLS's `{}` response.
Check child callbacks separately; synthetic payload replay establishes HLS behavior,
not that the host emits the expected payload or ordering.

## Evaluate and clean up

The printed `result.json` contains executable hashes, exact arguments, exit codes,
timeouts, interruptions, launch/cleanup errors, native stdout/stderr, normalized sink events,
final `sessions --json`, and consumer removal. Invalid UTF-8 output is replaced when decoded.
An interrupted run skips final polling, removes its consumer, prints the report path, and exits
130. Inspect the entire result. Exit zero means commands completed without timeout or cleanup
error; a host that exits zero after termination still fails the smoke check. It
does not itself prove discovery or correct lifecycle. Native logs can contain response
headers or private content: review locally before sharing.

Successful delivery needs events with the selected agent and native session ID, the
actual owning CLI PID, working then idle behavior when the host exposes them, and the
documented removal behavior. Final polling may already be empty after native shutdown;
use the sink sequence to distinguish correct removal from hooks that never ran. A
discovery log alone establishes only discovery. Compare the remaining native checks in
the validation matrix before making broader support claims.

The server stops and the isolated consumer is removed before the driver returns. Inspect
timeout logs for detached backend/updater processes and stop only PIDs created by this
probe if any survived process-group termination. Save a result only when it changes the
compatibility evidence, then remove the printed temporary root. No active user binary,
agent config, credential file, or repository is part of that cleanup.
