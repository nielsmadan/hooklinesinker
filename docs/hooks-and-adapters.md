# Hooks and adapters

Agent configuration invokes the shared binary's `ingest` command.
[`HookManager`](../src/hooks.rs) reconciles and inspects configuration;
[`normalize.rs`](../src/normalize.rs) assigns event meaning. See [installation](installation.md)
for activation, paths, and consumer lifetime.

## Configuration ownership

Claude, Codex, Qwen, and Devin store event groups under a JSON `hooks` key. Droid's JSON
root is the event map. Their installers preserve foreign groups and unrelated
values. Reconciliation removes recognized command handlers, including legacy Juggler notify
handlers, and installation adds fresh owned groups. Foreign handlers inside mixed groups
retain their group metadata, including prompt, agent and unknown handler shapes.
Ownership uses a command handler's canonical command, including agent and event; JSON also
recognizes a standalone hooklinesinker invocation at an old binary path as drift.
A JSON command whose binary word starts with `~/` or `$HOME/` and expands to the active
binary is installed, not drifted, so a config manager that renders home-relative commands
and hooklinesinker agree. This holds only for hosts that run hook commands through a POSIX
shell (`HookTarget::accepts_home_relative_commands`): Claude and Codex, observed recording
status from `~/` commands; Qwen, whose Unix hook runner spawns `bash -c`; and Droid, whose
hooks documentation describes hooks as shell commands and shows a user hook with a `~/`
redirect. Installation writes the absolute path for groups it adds, and leaves an event's
groups untouched when that event is already installed: its only owned handler is one group
of the exact installed shape. [Decision 0005](decisions/0005-home-relative-ownership.md)
records why.
Drift and legacy matching require plain command words; shell wrappers, substitutions,
redirections and compound commands are preserved as foreign. Status exposes only groups
containing a single recognized command handler as owned.

An unsupported hooks shape, including a non-array value under a managed event,
produces `unsupported` without rewriting the file. Invalid JSON/TOML produces an
error. Install does not write a file whose status is installed, except to remove legacy
Juggler handlers, which status does not count. A reconciliation that changes something
renders the document again, keeping key order and the file's mode but not its original
whitespace, and replaces it atomically.

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

Hook status validates the complete shape of each owned JSON/TOML entry, including matcher,
timeout, command, and duplicate ownership; an owned handler inside a mixed group counts as a
duplicate. JSON compares that shape with the command as written, so an accepted home-relative
spelling stays installed; Kimi requires the canonical command. Status and install share one
per-event predicate. Foreign entries remain outside that comparison. TypeScript status compares
the complete file with the generated content or its home-relative variant.

## Runtime bounds and event semantics

Hook failures must let the agent continue. JSON/TOML registrations set host
timeouts; Qwen expresses them in milliseconds, while the other command-hook
hosts use seconds. Codex `Interrupt` and `SessionEnd` use three seconds, including
in trust computation. [`hooks.rs`](../src/hooks.rs) stores event timeouts as `Duration` and
converts them to the host's units when rendering configuration.

Both TypeScript adapters spawn with argument arrays, discard child output, and
resolve failures. Each invocation runs in its own process group and escalates from
TERM to KILL within two seconds. Queued work uses the same deadline from enqueue time,
so stale calls are dropped rather than starting late. Ingest shares a 1.5-second
cooperative deadline across storage locks, consumer lookup, sink delivery, convergence
and best-effort health recording. Host timeouts and adapter timers remain the outer
bound for stalled stdin or filesystem operations; handled errors exit zero.
See [status lifecycle](status-lifecycle.md) for sink deadlines.

Registration and normalization must move together. `events.rs` owns native command-hook
registration and actions; `normalize.rs` adds Pi and OpenCode adapter mappings and
metadata-dependent refinements. Claude `PreToolUse` and `PermissionRequest` for the exact
`AskUserQuestion` tool report `idle` (waiting for input), with `running: true`.
Other Claude permission requests remain `permission`. `PostToolUse`,
`PostToolUseFailure`, or a new `UserPromptSubmit` returns the session to `working`.
For example, Codex `Interrupt` keeps the binding live and sets
it idle; `request_user_input` also maps to idle for its own binding, though child tracking (see
[child agents](status-lifecycle.md#child-agents)) treats it as that child waiting on the user.
Codex's `SubagentStart`/`SubagentStop` map to working/idle like any other event, but only ever
have a visible effect through a root's tracked children — a subagent event with no known root
starts a parallel binding that tracks that one child, so it still reports working, then idle. Claude's `Stop` stays `working` instead of
settling `idle` when `background_tasks[]` still names a live subagent, workflow, teammate, MCP
task, or cloud session; Claude's `SubagentStop` is ignored so a child finishing still cannot
mark its still-working parent idle on its own. Qwen's `SessionDelete` is ignored because it
names a different conversation.

### Session details

`normalize.rs` reads session details in a lenient second pass over the native JSON: a missing or
wrong-typed field is skipped and never fails the event, and strings pass one sanitizer in
[`details.rs`](../src/details.rs) (trim, drop control and format characters, cap at 120 scalars). Only these native fields are read;
prompt text, tool input and output, and message bodies never are, and `error` fields are read
only on `StopFailure`.

| Agent | Fields |
|---|---|
| Claude | `model` (`SessionStart`), `to_model` (`PostModelSwitch`), `effort` (`{level}` object or bare string), `permission_mode`, `session_title`, `StopFailure.error` as the problem, `tool_name`, live children in `background_tasks[]`, a non-empty `agent_id` as subagent origin |
| Codex | `model`, `permission_mode`, `tool_name`, `Interrupt` as problem `interrupted` |
| Cursor | `model_id` (fallback `model`), `tool_name`, `stop.status` as problem `interrupted` for `aborted` or `error` for `error` |
| Droid | `permission_mode`, `tool_name` |
| Qwen | `model`, `permission_mode`, `tool_name`, `StopFailure.error`, `Stop.context_usage` (a fraction, scaled to a percentage) |
| Kimi | `model`, `session_title`, `tool_name`, `StopFailure.error_type`, `Interrupt` as problem `interrupted` |
| Pi | the adapter's `model`, `effort`, `session_title`, `context_percent` and `problem` keys; a JSON `null` clears every one but `problem` |
| OpenCode | none; the V1 adapter does not load on OpenCode 2.x, and `session.status.busy` repeats on retry and after a permission, so it is no turn start. `activity.turns` stays 0 until the V2 adapter port maps a real prompt event to a turn start |

The tool is set by `PreToolUse` and `PermissionRequest` and cleared by `PostToolUse` and
`PostToolUseFailure`. Each agent's [event table](../src/events.rs) declares these roles beside
the phase. Droid's `PostToolUse` has no phase of its own: it refreshes the tool without changing
the phase. A turn starts on `UserPromptSubmit` (Kimi: `TurnStarted`, Pi: `agent_start`) and
increments `activity.turns`.
Problems are snake_case tokens, so `RateLimitError` becomes `rate_limit_error`.

Claude's `PostModelSwitch` (no matcher, five seconds) and Pi's `session_info_changed`,
`model_select` and `thinking_level_select` annotate the ledger without changing phase and are not
forwarded; the new value rides the next forwarded event. Claude Code ignores unknown hook names
since 2.1.101, so older installs keep working, but `PostModelSwitch` only fires on 2.1.251 and
later. `hooks status` reports an install without it as drifted.

[`agents.rs`](../src/agents.rs) is the inventory for adding an agent. Each profile ties the
wire agent to its event source, hook backend, status-event table, executable names, shared-host
markers, lifecycle flags, and timeout unit. Backend-specific parsing still lives in the modules
named above, but a new agent cannot silently omit one of these cross-cutting choices.

## Cursor CLI

`hooks install --agent cursor` manages `~/.cursor/hooks.json`, using version 1
direct command handlers. Each owned handler has `type: "command"`, a five-second
`timeout`, and `failClosed: false`. The installer preserves foreign handlers and
other settings; duplicate handlers, changed matchers, timeouts and binary paths
are drift. Unsupported versions, grouped handler shapes and ambiguous execution
overrides are left untouched. This path follows the CLI hook loader, independently
of `CURSOR_CONFIG_DIR` and `XDG_CONFIG_HOME`.

| Hook | Status |
|---|---|
| `sessionStart` | Idle for a new binding; preserves an existing live phase |
| `beforeSubmitPrompt` | Working; increments turns |
| `preToolUse` | Working; sets the tool |
| `postToolUse`, `postToolUseFailure` | Working; clears the tool |
| `preCompact` | Compacting |
| `stop` | Idle; `aborted` reports `interrupted`, `error` reports `error` |
| `sessionEnd` | Removes only the named binding |

All eight hooks emit `{}`, including handled telemetry failures. They supply no
permission decision, tool-input rewrite, or follow-up prompt. `preToolUse` means
tool activity; it does not establish that a permission dialog is open. Question
waiting and child counts are unreported. `subagentStart` and `subagentStop` are
not subscribed; tool callbacks carrying `parent_tool_call_id` are ignored.

`conversation_id` is required and becomes `session.id`. A conflicting `session_id`
is rejected; `generation_id` never identifies a session. The effective project
directory is native `cwd`, then `CURSOR_PROJECT_DIR`, then the first absolute
`workspace_roots` entry, then the hook launch directory. Git context uses the same
directory. Prompts, tool arguments/results, model parameters and user email are
not persisted. `transcript_path` is retained as metadata without reading the file.

Only CLI owners executing the installed `cursor-agent/versions/<version>/index.js`
bundle qualify. A generic `agent` process, arbitrary `node index.js`, desktop app,
cloud worker, ACP or persistent-server invocation does not. Ordinary interactive
and `--print` sessions are the intended scope. Recognition covers the pinned build's
chat options, combined short flags such as `-pf`, attached values such as `-HName:value`,
and launcher names retained in `argv[0]` when Linux reports Node as `MainThread`.
Cursor's imported Claude callbacks
are suppressed when Cursor is the nearest recognized agent; a real nested Claude
process keeps its own identity.

Same-process conversations stay independent. Startup is asynchronous, so a late
`sessionStart` preserves working/compacting state and cannot reopen an ended
binding. Resume in a new process creates a new binding on its first native event;
the inspected CLI does not emit `sessionStart` on resume. After sink delivery, HLS
rechecks the stored state and sends a correction within the remaining delivery
budget if concurrent activity overtook startup. See the
[validation record](reference/agent-validation.md) for observed and fixture coverage.

## Devin CLI

`hooks install --agent devin` manages the `hooks` key in `~/.config/devin/config.json`.
It writes eight grouped command hooks with omitted matchers and five-second timeouts,
preserving other configuration and foreign handlers. The documented user path is fixed;
`XDG_CONFIG_HOME` does not redirect this installer. Devin describes command hooks as shell
commands, so home-relative binary spellings are accepted as well as the generated quoted
absolute path. The command returns silent success and never emits a permission decision.

`SessionStart` reports idle; `UserPromptSubmit` reports working and increments turns;
`PreToolUse` reports working and sets the tool; `PostToolUse` clears the tool while preserving
phase. `Stop` reports idle and `SessionEnd` removes only that session. `PostCompaction`
annotates without starting a compacting phase. The [lifecycle reference](https://docs.devin.ai/cli/extensibility/hooks/lifecycle-hooks)
says it fires “after context compaction completes successfully.” It describes
`PermissionRequest` as “Fires when the agent needs a permission decision.” Hooklinesinker
reports that event as permission, conservatively retaining it across tool activity until a
root prompt, stop, or session-start boundary. A policy may resolve the decision immediately;
there is no documented resolution hook or tool-call correlation ID, so this is not proof
that a human dialog remains open. `prompt_id` identifies a turn, not an individual tool.

A nonempty native `session_id` is required. The project root comes from `DEVIN_PROJECT_DIR`
when the payload omits `cwd`; Git context is gathered from that effective directory.
Prompts, tool arguments/results, compaction summaries, and raw payloads are never persisted.
No child count, compaction-in-progress, or hosted Devin state is inferred. Same-process
sessions are retained independently because the public contract does not establish exclusive
ownership across every CLI mode.

Devin imports Claude hook configuration by default. A declared Claude callback is suppressed
when its nearest recognized ancestor is Devin; the native Devin callback supplies the record.
A real Claude process nested under Devin remains Claude, and unknown ancestry keeps explicit
manual ingestion working. Script-runtime detection uses the launcher position rather than
matching arbitrary arguments. The [hook overview](https://docs.devin.ai/cli/extensibility/hooks/overview)
documents the inherited configuration and global path.

The fixture contract was checked against the official documentation on 2026-11-01 and the
verified Devin CLI 3000.11.234 bundle (`92c83011d113`). Automated checks cover installation,
normalization, permission precedence, process provenance, and generated command execution.
An isolated `auth status` reported “Not logged in”; a minimal non-interactive prompt exited
with “Login canceled.” Live hook delivery and inheritance suppression remain unverified.
No minimum host version is promised.

## Embedded TypeScript

[`opencode-hooklinesinker.ts`](../adapters/opencode-hooklinesinker.ts) and
[`pi-hooklinesinker.ts`](../adapters/pi-hooklinesinker.ts) are source assets
embedded with Rust `include_str!`. Installation replaces the
`join(homedir(), "__HOOKLINESINKER_BIN__")` expression with the binary's absolute path as a
string literal, which unlike `homedir()` does not depend on the agent's `$HOME`, drops the
imports only that expression uses, and prepends a protocol ownership marker. A marked file
that keeps the expression, with the binary's path relative to `$HOME`, is also installed. Both
comparisons are byte for byte against the current template, so an external renderer must
reproduce the marker line and template exactly and follow template changes between releases.
An installed file that is not group- or world-writable is not rewritten; any rewrite leaves it
`0600`, because the agent loads it as code. Install refuses a group- or world-writable plugin
directory even when the file is already installed. Adapter edits reach installed files after
rebuilding the binary and reinstalling the hook.

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

Pi queues hooks serially and suppresses sessions whose `hasUI` is false. Session starts
forward their native `reason` so resumes can reactivate retired bindings; new and fork
starts replace the previous foreground binding. `agent_settled` marks idle; manual
compaction returns to idle, other compaction to working. Permission decisions clear only the
pending prompt with the same request ID; settlement clears pending prompts. Handlers with a
context read `model`, `effort`, `context_percent` (rounded; `null` when Pi reports it
unknown) and the session name each in its own `try`, so a stale context drops only that
field; `session_shutdown` and channel-driven sends read none. An aborted `agent_settled`
sends `problem: "interrupted"`. `session_info_changed` (name or `null`), `model_select` and
`thinking_level_select` are sent as their own events. Only a shutdown with reason `quit`
emits removal. Every shutdown drains hooks and clears pending prompts; permission listeners
remain registered so new, resumed, forked and reloaded sessions continue reporting
permission state.

Pi also tracks this session's live `pi-subagents` async runs (run ID → PID) from
`subagent:async-started`/`subagent:async-complete`, independent of the permission listeners.
When `agent_settled` or a manual compaction would otherwise report idle, it first drops any run
whose PID is no longer alive; if a run remains, it reports `working` instead, as
`session_compact_working`. That event is not a turn start, so the hold keeps the tool, the
problem and the turn count; an aborted settle still sends `problem: "interrupted"` with it. A run completing
while held only releases that hold when its payload says `triggerTurn === false` — Pi normally
wakes the parent to consume the result right away, so releasing on every completion would flash
idle before that wake arrives. On `session_start`, live runs are rebuilt from `status.json`
files on disk without blocking startup, and a rebuilt run holds the session with the same
`session_compact_working`. A session epoch, and a short-lived set of runs that
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

## Goose CLI

Goose v1.57.0 source establishes user plugin discovery from `.agents/plugins`, grouped
`hooks/hooks.json` without a manifest, regex matchers, seconds for `timeout`, and `sh -c`
command execution. Both absolute and home-relative hook commands are supported.
Registered events are `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `Stop`,
`PreToolUse`, `PostToolUse`, and `PostToolUseFailure`. The native `working_dir` becomes
`cwd` before lifecycle recording; environment/git discovery also uses that directory.
No prompt, message, tool input/output, or transcript content is stored.

Scope is foreground Goose CLI (`goose session` / `goose run`). Known `goose serve` and
legacy `goosed` callbacks are ignored because they also serve Desktop; a standalone shared
backend cannot identify its CLI consumer. Unknown process provenance retains explicit replay
support. ACP/gateway owners are conservatively shared, preserving separate session IDs;
Desktop/serve support and complete subagent aggregation are unavailable.

Validation: source pinned to v1.57.0 (`30bba95105e59bc3e7df624cf6b4d433690cfe12`);
installer, generated shell commands, lifecycle, and filtering have fixture coverage.
The isolated 1.57.0 live prompt stopped with “No provider configured” before model execution;
real turn/tool/session-end delivery is not live-verified.


## GitHub Copilot CLI

`hooks install --agent copilot` writes `$COPILOT_HOME/hooks/hooklinesinker.json`, defaulting
to `~/.copilot/hooks/hooklinesinker.json`. Its version 1 envelope contains direct command
handlers with `timeoutSec: 5` and no matcher. The shell `command` field is the cross-platform
fallback, so absolute and home-relative commands are recognized. Windows command execution
is outside this macOS/Linux validation. Foreign handlers and other top-level keys survive.
Installed status requires the exact envelope version and handler shape, with one owned entry
per event. A matching command carrying `bash`, `powershell`, `exec`, `args`, `cwd`, or `env`
overrides is unsupported and remains untouched. No-op installation preserves bytes; changed
files use the existing conditional atomic writer and preserve file mode.

The subscribed events are `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`,
`PostToolUse`, `PostToolUseFailure`, `Stop`, `PreCompact`, and lowercase `notification`.
The [official reference](https://docs.github.com/en/copilot/reference/hooks-reference)
states: “Empty output uses default behavior.” Ingest produces no permission decisions and
returns zero on controlled telemetry failures. Copilot pretool hooks fail closed on crashes
or other nonzero exits; timeouts fail open. This means a killed/crashed helper can deny a
tool even though its normal error handling is nonblocking.

Copilot's session sandbox also applies to user command hooks. The active binary, state/data
roots, and configured sinks must be reachable under the user's policy. Hook installation
never widens sandbox access or disables sandboxing. Configuration status does not prove that
the host loaded the file or granted access.

Only native identity, cwd, and tool metadata enter status. Prompts, notification messages,
and tool arguments/results are not persisted. Known child callbacks are ignored; child counts
and complete child lifecycle coverage are unavailable. Copilot reads repository Claude hooks,
so the nearest-producer guard suppresses inherited Claude callbacks without suppressing a real
Claude process nested inside Copilot. Unknown ancestry preserves explicit replay.

Validation uses official documentation and Copilot CLI 1.0.105. Automated coverage includes
schema/ownership, generated commands, canonicalization, waits, child isolation, provenance,
and independent sessions. The isolated live prompt failed token validation on a network request before model execution;
live hook delivery and sandbox access remain unverified. No minimum host version is promised.

## Antigravity CLI — provisional, support blocked

The `antigravity` profile is preparatory integration, **not verified CLI support**. It is
included in all-agent configuration management; `hooks status` and installation output mark
its configuration as provisional even when the four definitions are installed correctly.
Do not interpret `installed` as proof of a usable lifecycle.

The named definition `hooklinesinker` in `~/.gemini/config/hooks.json` subscribes
`SessionStart`, `PreInvocation`, `PostToolUse`, and `Stop`. Tool events use matcher groups;
the other events contain direct handlers. Each command has a five-second timeout. Foreign
names and handlers are preserved. A reserved-name collision without recognized commands, an
unsupported shape, or a disabled definition containing foreign handlers is left unchanged.
A disabled wholly owned definition is drifted and can be enabled. Home-relative shell commands
are recognized; idempotent installation preserves bytes and updates preserve the file mode.

The [hook reference](https://antigravity.google/docs/hooks) describes `command` as “The shell
command to execute.” Its Stop contract says “Any other value allows the stop.” Stop responses
are `{"decision":"allow"}`; the other subscribed hooks return `{}`. Controlled stdin, path,
state, normalization, and sink errors still reach that response. Crashes, termination,
broken stdout, and host timeouts cannot guarantee a response. Storage lock waits share
ingest's 1.5-second deadline, including error reporting. Real host timeout behavior
remains unverified. No permission hook is subscribed and no approval is emitted.

Callbacks require both a positively identified `agy`/`antigravity` CLI ancestor and a matching
absolute `.gemini/antigravity-cli/brain/<conversationId>/.system_generated/logs/transcript.jsonl`
path. App/IDE paths, traversal paths, unknown owners, and detached backends produce no rows or
terminal attribution. No transcript contents are read. `conversationId`, `transcriptPath`,
`modelName`, and `toolCall.name` supply identity/details. The current directory wins when it is
inside a listed absolute workspace; otherwise the first valid workspace wins. Git discovery
uses that effective directory. `PreInvocation` reports working without counting a user turn;
`PostToolUse` refreshes details. Stop requires boolean `fullyIdle`: true reports idle, false
reports working; missing or invalid values record health and leave existing state unchanged.

Validation used official documentation and isolated binaries `agy` 1.1.11 and `antigravity`
1.4.12. The initial minimal prompts failed authentication. A subsequent isolated 1.4.12 run
loaded the generated named hook definition, then failed silent authentication and attempted
OAuth; it was terminated at the 35-second probe deadline without interaction. Automated fixtures cover the schema,
responses and conservative lifecycle, but **actual hook ancestry, closing a CLI while its
backend survives, eventual authoritative settlement after Stop(false), and host timeout/crash
semantics remain blocked**. The public fullyIdle field definition does not establish a later
Stop(true) notification. Multiple conversations remain independent and a verified CLI owner
would drive dead-process sweeping; a live backend is never substituted for that owner. No
permission, compaction, session-end or complete child state is inferred. These gates must be
resolved before advertising support.


## Kilo CLI

Kilo uses the same standalone TypeScript source as OpenCode. The installer renders a closed
host configuration into `HOST_CONFIG`: agent name, native status mapping, background flag and
default, compaction policy, and disposal callback policy. Neither installed plugin imports its
sibling or an SDK at runtime. There is one exported plugin function.

The installed path is `$KILO_CONFIG_DIR/plugins/hooklinesinker-kilo.ts`, otherwise
`$XDG_CONFIG_HOME/kilo/plugins/hooklinesinker-kilo.ts`, otherwise
`~/.config/kilo/plugins/hooklinesinker-kilo.ts`. Kilo also discovers project/home `.kilo` and
`.kilocode` directories; these are not installation targets. It does not discover OpenCode's
configuration roots. Kilo reconciliation has no legacy filename and cannot remove OpenCode's
legacy adapter. `KILO_PURE=true` or `--pure` disables external plugins, including this adapter.

Kilo's background-child hold defaults to enabled. `KILO_EXPERIMENTAL_BACKGROUND_SUBAGENTS=false`
disables it independently of `KILO_EXPERIMENTAL` and OpenCode flags. `offline` maps to working;
`scheduled` maps to idle, subject to live-child and permission precedence. Permission requests
remain keyed by native ask ID and replies by `requestID`. Completed compaction emits `session.compaction.completed` and annotates the
current phase; it does not start a compacting phase. Kilo's awaited `dispose` callback attempts
known-root deletions; instance-disposal events use the same cleanup. Disposal closes event
admission, drains the existing queue within its elapsed two-second budget, then runs up to eight
deletion helpers concurrently within one additional two-second budget (each stage allows 100 ms
for forced termination). Budget exhaustion warns with the number of unattempted deletions.
Concurrent or repeated disposal shares that cleanup, including when the ordinary queue is full.
Cleanup is best effort: helper failures or budget exhaustion can leave stale records while the
backend remains alive. Process death retains the usual PID/start-time sweep. Child activity is aggregated by the plugin, with no invented child
count at the Rust boundary.

The v7.8.25 source contract is pinned in the [event reference](reference/agent-hook-events.md#kilo-cli-source-contract).
Automated tests execute installed absolute and accepted home-relative artifacts, including paths
with spaces and apostrophes. Shared fixtures cover queue bounds/deadlines, root-child concurrency,
permission replies, unknown statuses and disposal. The isolated live turn completed with `openrouter/openai/gpt-4.1-mini`; native plugin discovery,
Kilo process attribution and disposal removal were observed. The default model first failed with
an endpoint/tool-compatibility error. Live background-child settlement remains unverified.
