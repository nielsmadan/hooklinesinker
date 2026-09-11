# Hooks and adapters

Agent configuration invokes the shared binary's `ingest` command.
[`HookManager`](../src/hooks.rs) reconciles and inspects configuration;
[`normalize.rs`](../src/normalize.rs) assigns event meaning. See [installation](installation.md)
for activation, paths, and consumer lifetime.

## Configuration ownership

Claude, Codex, and Qwen store event groups under a JSON `hooks` key. Droid's JSON
root is the event map. Their installers preserve foreign groups and unrelated
values, replace recognized owned groups, and remove legacy Juggler notify groups.
Ownership uses the canonical command, including agent and event; JSON also
recognizes a matching hooklinesinker command at an old binary path as drift.

An unsupported hooks shape, including a non-array value under a managed event,
produces `unsupported` without rewriting the file. Invalid JSON/TOML produces an
error. Preservation of foreign JSON values does not promise original whitespace:
successful reconciliation renders the document again and replaces it atomically.

Kimi uses TOML `[[hooks]]` entries. Its installer uses `toml_edit`, writes only
`event`, `command`, and `timeout`, and removes only exact canonical-command matches.
It re-parses the rendered TOML before writing and the disk contents afterward.
This protects a host where one malformed hook entry can invalidate the config;
an unfamiliar `hooks` shape is left untouched. Foreign TOML content is retained.

Codex ownership ends at `hooks.json`. The host application owns trust approval
and `trusted_hash` entries in Codex's `config.toml`; hooklinesinker never edits
that file. `hooks status --agent codex --json` exposes each owned entry's event,
group index, and command so the host can compute trust for the installed group.
Installing a group does not approve it.

Hook status is an inspection aid, not a full host schema validator. JSON/TOML
`installed` checks command coverage; it does not compare every matcher, timeout,
or foreign field. TypeScript status compares the complete generated content.

## Runtime bounds and event semantics

Hook failures must let the agent continue. JSON/TOML registrations set host
timeouts; Qwen expresses them in milliseconds, while the other command-hook
hosts use seconds. Codex `Interrupt` and `SessionEnd` use three seconds, including
in trust computation. Event-specific values live in [`hooks.rs`](../src/hooks.rs).

Both TypeScript adapters spawn with argument arrays, discard child output, and
resolve failures. Each invocation schedules a kill after two seconds. The host
timeouts and adapter timers bound waiting outside ingest; ingest's handled
errors exit zero. See [status lifecycle](status-lifecycle.md) for sink deadlines.

Registration and normalization must move together. For example, Codex
`Interrupt` keeps the binding live and sets it idle; `request_user_input` also
maps to idle. Claude's `SubagentStop` is ignored so a child finishing cannot
mark its still-working parent idle. Qwen's `SessionDelete` is ignored because
it names a different conversation. The full mapping stays in `normalize.rs`.

## Embedded TypeScript

[`opencode-hooklinesinker.ts`](../adapters/opencode-hooklinesinker.ts) and
[`pi-hooklinesinker.ts`](../adapters/pi-hooklinesinker.ts) are source assets
embedded with Rust `include_str!`. Installation substitutes
`__HOOKLINESINKER_BIN__` and prepends a protocol ownership marker. Adapter edits
reach installed files after rebuilding the binary and reinstalling the hook.

An unmarked file at the generated path is foreign and remains untouched.
Marked content that differs from the current template is drifted and can be
reconciled; uninstall deletes it only when its marker matches this protocol.
The named legacy Juggler adapter files are also removed during reconciliation.

OpenCode emits a synthetic `session.created` on plugin load with directory but
no session ID, including when resuming. Its listener validates dynamic objects,
selects the first valid session ID, and expands `session.status` with its status
suffix before invoking ingest. It forwards only ID and directory metadata.

Pi queues hooks serially and suppresses sessions whose `hasUI` is false.
`agent_settled` marks idle; manual compaction returns to idle, other compaction to
working. Permission prompts stay pending until eligible decisions clear them all;
settlement clears pending prompts. Only a shutdown with reason `quit` emits
removal. Every shutdown drains hooks and unsubscribes permission listeners.

## Validation

[`tsconfig.json`](../tsconfig.json) checks strict, erasable TypeScript without
emitting JavaScript. [`package.json`](../package.json) also rejects explicit
`any`. Host interfaces describe only the methods used locally; they do not prove
compatibility against installed host SDK types. Dynamic objects still require
runtime validation before property access.

[`tests/ts_adapters.rs`](../tests/ts_adapters.rs) runs the actual templates through
[`adapters_harness.mjs`](../tests/adapters_harness.mjs) with stub hosts and a fake
ingest binary, covering routing, malformed payloads, permission state, and hangs.
Without Node 22.6+, these behavior tests print `SKIP` and pass. Development needs
Node 22.12+; see [development and releases](development-and-releases.md) for checks.
