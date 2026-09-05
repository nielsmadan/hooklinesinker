# Examples

Runnable counterparts to the [Integrating](../README.md#integrating) section. Both are
exercised end-to-end by `../tests/examples.rs`.

## cli-poll

The Pull model as a dependency-free POSIX shell script: registers a consumer, polls
`hooklinesinker sessions --json` once, and prints a table of live sessions (agent, phase,
session id, cwd). Refuses an envelope whose `protocol` is not `1`, surfaces `problems`, and
falls back to printing the raw envelope if `jq` is not installed.

```sh
HOOKLINESINKER_BIN=hooklinesinker examples/cli-poll/poll.sh
```

Run it again whenever you want a fresh snapshot — `hooklinesinker sessions --json` never
blocks, so this is safe to wrap in a loop (`watch examples/cli-poll/poll.sh`) or call from a
cron job.

Environment variables:

- `HOOKLINESINKER_BIN` — path to the binary (default: `hooklinesinker` on `PATH`).
- `HOOKLINESINKER_CONSUMER` — consumer name to register (default: `cli-poll-example`).

## sink-server

The Push model as a single dependency-free Node script — also the pattern an Electron main
process would use, per the README's Bundling section. Starts a plain `http.createServer`,
registers itself as a sink, hydrates from `sessions --json`, and then applies live POSTs
through the same code path, keyed by `bindingId` so a live event racing hydration never
creates two rows. A `running:false` event removes its row.

```sh
HOOKLINESINKER_BIN=hooklinesinker node examples/sink-server/sink-server.mjs
```

Watch its stdout — every hydrate, add, update and remove is logged. `Ctrl+C` stops the
server; the consumer registration stays (uninstalling is a separate, explicit step, same as
in Juggler/ringleader).

Environment variables:

- `HOOKLINESINKER_BIN` — path to the binary (default: `hooklinesinker` on `PATH`).
- `HOOKLINESINKER_CONSUMER` — consumer name to register (default: `sink-server-example`).
- `HOOKLINESINKER_HOST` — host to bind (default: `127.0.0.1`).
- `PORT` — port to bind, `0` for an OS-assigned ephemeral port (default: `4870`).
