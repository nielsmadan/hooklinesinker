# Hooks and adapters

Agent configuration invokes the shared binary's `ingest` command.
[`HookManager`](../src/hooks.rs) reconciles and inspects configuration;
[`normalize.rs`](../src/normalize.rs) assigns event meaning. See [installation](installation.md)
for activation, paths, and consumer lifetime.

## Configuration ownership

Claude, Codex, and Qwen store event groups under a JSON `hooks` key. Droid's JSON
root is the event map. Their installers preserve foreign groups and unrelated
values. Reconciliation removes recognized command handlers, including legacy Juggler notify
handlers, and installation adds fresh owned groups. Foreign handlers inside mixed groups
retain their group metadata, including prompt, agent and unknown handler shapes.
Ownership uses a command handler's canonical command, including agent and event; JSON also
recognizes a standalone hooklinesinker invocation at an old binary path as drift.
Drift and legacy matching require plain command words; shell wrappers, substitutions,
redirections and compound commands are preserved as foreign. Status exposes only groups
containing a single recognized command handler as owned.

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

Hook status validates the complete canonical shape of each owned JSON/TOML entry,
including matcher, timeout, command, and duplicate ownership. Foreign entries remain
outside that comparison. TypeScript status compares the complete generated content.

## Runtime bounds and event semantics

Hook failures must let the agent continue. JSON/TOML registrations set host
timeouts; Qwen expresses them in milliseconds, while the other command-hook
hosts use seconds. Codex `Interrupt` and `SessionEnd` use three seconds, including
in trust computation. [`hooks.rs`](../src/hooks.rs) stores event timeouts as `Duration` and
converts them to the host's units when rendering configuration.

Both TypeScript adapters spawn with argument arrays, discard child output, and
resolve failures. Each invocation runs in its own process group and escalates from
TERM to KILL within two seconds. Queued work uses the same deadline from enqueue time,
so stale calls are dropped rather than starting late. The host
timeouts and adapter timers bound waiting outside ingest; ingest's handled
errors exit zero. See [status lifecycle](status-lifecycle.md) for sink deadlines.

Registration and normalization must move together. `events.rs` owns native command-hook
registration and actions; `normalize.rs` adds Pi and OpenCode adapter mappings and
metadata-dependent refinements. For example, Codex `Interrupt` keeps the binding live and sets
it idle; `request_user_input` also maps to idle for its own binding, though child tracking (see
[child agents](status-lifecycle.md#child-agents)) treats it as that child waiting on the user.
Codex's `SubagentStart`/`SubagentStop` map to working/idle like any other event, but only ever
have a visible effect through a root's tracked children — a subagent event with no known root
starts a parallel binding that tracks that one child, so it still reports working, then idle. Claude's `Stop` stays `working` instead of
settling `idle` when `background_tasks[]` still names a live subagent, workflow, teammate, MCP
task, or cloud session; Claude's `SubagentStop` is ignored so a child finishing still cannot
mark its still-working parent idle on its own. Qwen's `SessionDelete` is ignored because it
names a different conversation.

[`agents.rs`](../src/agents.rs) is the inventory for adding an agent. Each profile ties the
wire agent to its event source, hook backend, status-event table, executable names, shared-host
markers, lifecycle flags, and timeout unit. Backend-specific parsing still lives in the modules
named above, but a new agent cannot silently omit one of these cross-cutting choices.

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
suffix before invoking ingest. It forwards only ID and directory metadata, and serializes
invocations to preserve arrival order. `tui.session.select` forwards explicit selection
requests without retiring other conversations; ordinary TUI navigation is not visible to
this backend plugin. Instance disposal emits deletion for every known session ID that was
not already deleted.

A child session — one whose `info.parentID` is set — is tracked under its root rather than
reported under its own ID. The adapter follows `parentID` to the end of the chain to resolve a
root (so grandchildren resolve the same way), and computes a tree phase over every session that
resolves to it: `permission` if any has an open `permission.asked`, else `working` if any is
busy or retrying, else `idle`. A child's `session.created` and `session.compacted` never
forward their native event; they emit the root's current tree phase instead, and only when it
differs from the phase last actually sent for that root — a dropped or superseded hook does not
count as sent. A child learned late, whose `parentID` arrives after its own row was already
forwarded as a root, has that stray row deleted — after its root's tree phase is re-sent, so
the root reclaims a terminal row the stray had taken over before the stray's removal arrives. `permission.replied` resolves the pending ask
named by its `requestID` and, like `session.updated`, re-evaluates and (if changed) re-emits the
root's tree phase; an idle-meaning event for the root while the tree is still working or waiting
on permission re-emits the tree phase instead of forwarding idle. When background subagents are
enabled in OpenCode's process (`OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS` set to `true`, `yes`,
`on`, `1` or `y`, read once at plugin load), a child event that would take the root's queued
phase from `working` to `idle` sends nothing and holds the root: OpenCode injects the finished
child's result into the root as a new prompt right after, so idle would only flash. The default
foreground mode never holds, because an aborted root's cancelled child can report idle after the
root's own idle with no wake to follow. While held, the root's own `session.updated` and
`permission.replied` cannot release idle either, and a late `parentID` that links a stray under
the root reports it `working`; the root's own status, `session.idle`, `session.error` and
`permission.asked` clear the hold and report as usual, so the injected prompt's busy keeps it
working and its next idle settles it. A child's permission still surfaces on a held root. Deleting a child session
re-resolves and re-emits its root's phase (a deletion is not a completion, so it releases an
unheld root to idle); deleting a root forgets every descendant and emits one `session.deleted`.

Pi queues hooks serially and suppresses sessions whose `hasUI` is false.
Session starts forward their native `reason` so resumes can reactivate retired bindings;
new and fork starts replace the previous foreground binding. `agent_settled` marks idle;
manual compaction returns to idle, other compaction to working. Permission decisions clear
only the pending prompt with the same request ID;
settlement clears pending prompts. Only a shutdown with reason `quit` emits
removal. Every shutdown drains hooks and clears pending prompts; permission listeners remain
registered so new, resumed, forked and reloaded sessions continue reporting permission state.

Pi also tracks this session's live `pi-subagents` async runs (run ID → PID) from
`subagent:async-started`/`subagent:async-complete`, independent of the permission listeners.
When `agent_settled` or a manual compaction would otherwise report idle, it first drops any run
whose PID is no longer alive; if a run remains, it reports `working` instead. A run completing
while held only releases that hold when its payload says `triggerTurn === false` — Pi normally
wakes the parent to consume the result right away, so releasing on every completion would flash
idle before that wake arrives. On `session_start`, live runs are rebuilt from `status.json`
files on disk without blocking startup: a session epoch, and a short-lived set of runs that
complete during the scan, keep that race from resurrecting a run that already finished. A run
whose `completionOwnerId` names another Pi process (for example one started before a quit and
`pi --resume`) is not rebuilt: pi-subagents announces its completion only in that process.
`asyncRunsDir()` reads `PI_SUBAGENTS_TEMP_ROOT` when set, mirroring `pi-subagents`' own
temp-root layout, and otherwise falls back to its default per-UID temp directory.
`session_shutdown` forgets every live run.

## Validation

[`tsconfig.json`](../tsconfig.json) checks strict, erasable TypeScript without
emitting JavaScript and rejects unchecked indexed access. [`package.json`](../package.json)
runs type-aware correctness, suspicious, and performance rules and rejects explicit `any`.
Host interfaces describe only the methods used locally; they do not prove
compatibility against installed host SDK types. Dynamic objects still require
runtime validation before property access.

[`tests/ts_adapters.rs`](../tests/ts_adapters.rs) runs the actual templates through
[`adapters_harness.mjs`](../tests/adapters_harness.mjs) with stub hosts and a fake
ingest binary, covering routing, malformed payloads, permission state, and hangs.
Without Node 22.6+, these behavior tests print `SKIP` and pass. Development needs
Node 22.12+; see [development and releases](development-and-releases.md) for checks.
