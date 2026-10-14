# Status lifecycle

Hooklinesinker stores the latest status per agent/process binding and sends it to
registered sinks. It has no daemon, event history, delivery queue, or retry worker.

## Ingest and identity

The flow starts in [`run_ingest`](../src/main.rs), which delegates to
[`handle_ingest`](../src/ingest.rs). That application layer normalizes the event and updates
[`StatusStore`](../src/state.rs):

1. Read at most 1 MiB of native JSON; gather terminal, tmux, Git, and remote-host
   context; discover the owning agent by walking the hook process's ancestors. Terminal
   identity is withheld when the pane's tmux session has no attached client, since its
   terminal variables were inherited from the tab that started the tmux server. A failed
   or unparseable tmux query keeps the terminal identity. `sessions --json` repeats the check
   for every record with a tmux pane and a terminal, with one `list-panes` per stored socket,
   so a session detached since its last hook stops claiming the tab. A record without a socket
   asks the default server, which counts only when it names the stored tmux session. Commands
   run with `$TMUX` removed, and `tmux` is looked up in `/opt/homebrew/bin`, `/usr/local/bin`
   and `/usr/bin` when it is not on `PATH`, since apps started from Finder get launchd's
   `PATH`.
2. Map command-hook events through `events.rs` and adapter-specific events through
   `normalize.rs` to a phase update, removal, or ignored event. Event names outside an
   agent's vocabulary become ingest diagnostics.
3. Write or remove the binding, retire superseded foreground bindings, then
   sweep bindings whose processes have died.
4. Send the normalized event and synthetic removals to registered status sinks.

`session.id` identifies an agent conversation. `bindingId` hashes the agent,
session ID, host, PID, and process start time. Resuming one conversation in two
processes therefore produces two bindings. macOS renames the host as networks change
(`MacBookPro.fritz.box`, then `…local`). When an event's binding has no record but the same
agent, session ID, remote host, PID, and start time already have one, the event takes over that
record's `bindingId` and `process`, so the session keeps its binding, children, and retired
marker, and consumers never see it re-keyed. Other records of that session and process, left
by versions that re-keyed on a rename, are deleted without a removal event: a `running:false`
carrying the live session's terminal and session ID would remove its row in consumers that key
on the terminal. PID reuse does not preserve a binding:
[`SystemProcessLookup`](../src/processes.rs) checks both PID and start time. Liveness checks
reuse the ancestor-discovery snapshot during one ingest transaction, avoiding a second full
process-table scan. `sessions` and `doctor` use `SystemProcessLiveness`, which refreshes only
the requested PID and holds no process table.

Ancestor detection accepts executable names and script paths under Node, Bun,
or Deno. An unrecognized launcher can leave `process` null. A nonempty-session
record with no process identity is retained as unverifiable diagnostic state for
up to 24 hours. It is excluded from running results, then the next ingest sweep
removes it without emitting a synthetic removal event.

An empty session ID is different: `normalize` returns it as forward-only, so the event
reaches sinks but cannot enter the ledger. OpenCode's load-time event uses this path so consumers can
update a terminal row before a conversation ID becomes available.

Foreground bindings replace one another within the same agent, PID, process start
time, terminal, tmux pane, and remote-host context. The host is not compared, for the same
renaming reason. Inside tmux the pane stands in
for the terminal, because attaching or detaching changes the reported terminal within one
process. Tmux session names are navigation metadata: renames or failed name lookups do
not change binding identity.
A different nonempty foreground session ID retires the previous foreground binding
immediately, regardless of its phase. For exclusive CLI sessions, the first status
update can replace a binding even if the new session's start hook was missed.
An empty session ID, missing process identity, or end event cannot replace another
binding. The new record is written before retirement, under the same ledger lock;
successful retirements produce `swept` events for sinks even if another retirement fails.

[`lifecycle.rs`](../src/lifecycle.rs) interprets agent-specific signals for the shared
ledger transaction in `state.rs`:

| Agent | Foreground selection and replacement | Parallel protection |
|---|---|---|
| Claude | Ordinary activity; `SessionStart source: resume` reactivates a retired binding | `source: fork` starts parallel; `agent_id` identifies child activity |
| Codex | Ordinary activity, including a root `source: fork`; `source: resume` reactivates | Any event carrying `agent_id` attaches to the root's tracked children instead of starting its own binding (only `SubagentStart` creates a new entry there); an `agent_id` with no known root starts a parallel binding that tracks that one child; app-server and legacy mcp-server modes stay independent |
| Droid | Ordinary activity; `SessionStart source: resume` reactivates | A child `SessionStart` naming its parent's (or grandparent's) `calling_session_id` attaches to that root instead of starting its own session; distinct owner processes stay independent; ambiguous same-process sessions are retained conservatively |
| Qwen | Ordinary activity, including `source: branch`; `source: resume` reactivates | ACP/serve modes and attributed `source_type`/`source_id` sessions stay independent |
| Kimi | Ordinary activity; `SessionStart source: resume` reactivates | ACP, web and wire launch modes stay independent |
| Pi | UI session activity, including new/fork; `session_start reason: resume` reactivates | Adapter suppresses contexts with `hasUI: false` |
| OpenCode | `tui.session.select` selects/reactivates an independent binding | Selection and activity never retire another conversation |

Recognized child activity cannot replace another binding. When it introduces a separate binding,
that binding is marked parallel; child activity on an existing parent preserves its role.
Parallel markers survive ordinary updates. An explicit foreground resume can select a
previously parallel binding. Resume in a recognized shared server remains independent.
OpenCode selection retains the last known phase rather than resetting a busy or waiting
session to idle; a newly observed selection starts idle.

OpenCode selection marks the selected binding parallel and preserves every other live
conversation. After selecting A then B, A's activity, permission requests, and deletion still
reach the ledger and sinks. Selection is navigation, not an exclusive-session lifecycle
boundary. The backend plugin does not observe ordinary session-picker navigation; accurate
display selection would require a separate TUI integration. See the
[external identity and navigation evidence](reference/agent-hook-events.md#opencode-selection-is-navigation).

**Droid task children:** a child `SessionStart` whose `calling_session_id` differs from its own
`session_id` (verified against a local capture, 2026-10-06, droid 0.218.2) attaches to that
parent's root — or, for a grandchild, to the root whose tracked children already include the
parent's digest — instead of starting its own session; see
[child agents](#child-agents). A `SessionStart` without `calling_session_id` is always a root. A
same-process Droid session with a different ID and no such marker is still retained as parallel,
since exclusivity among those is unverified.
The [cross-agent reference](reference/agent-hook-events.md#session-identity-subagents-and-shared-processes)
distinguishes documented contracts, inspected source, and unresolved behavior. Current
regression tests exercise synthetic metadata; they do not verify real subagents across agents.

Server-mode protection in
[`processes.rs`](../src/processes.rs) checks known launch arguments; in-process mode changes
or custom multiplexing wrappers without those arguments cannot be identified reliably.

Retired and explicitly ended bindings retain a private marker until their owning process
exits. They are excluded from session results and dead-binding counts. Subsequent hooks
for them, including delayed startup, status, and end hooks, are discarded without sink
delivery. Explicit resume/selection reactivates them. Ingest sweeps discard markers after
process exit without sending duplicate removals; reads never delete them.
Parallel records sharing a long-lived host process expire after 24 hours without an update;
the sweep emits one removal and prevents abandoned server sessions from accumulating forever.

Lifecycle decisions follow ingest order. Native hooks provide no sequence number to
distinguish a delayed resume from an intentional return, or an unseen delayed session
from a new one. Parallel protection depends on observed native metadata and recognized
launch mode. Pre-upgrade records without role metadata are treated as foreground except
for OpenCode, whose legacy records remain independent. Existing `claudeParallel` and
`claudeRetired` markers are accepted on upgrade. Lifecycle markers remain private and do
not change the consumer wire protocol.

[`gather_environment`](../src/environment.rs) supplies optional navigation metadata.
Native `cwd` overrides the event's session directory; environment probes use the
hook process's directory. Missing metadata does not prevent a status update.

## Liveness and phase

`running` and `phase` answer separate questions. A live process can be idle,
working, waiting for permission, or compacting. `phase` is an aggregate of the session's own
latest mapped hook event and any child agents it is tracking (see
[Child agents](#child-agents) below); process inspection cannot reconstruct a missed
working-to-idle event. Each input to the aggregate stays at its previous value until another
relevant event arrives.

Only ingest removes ledger records. `running()`, `sessions --json`, and `doctor`
filter or count dead bindings without deleting them. The next ingest sweep
removes each dead binding and emits an event named `swept`, with `running: false`
and a fresh `observedAt`. Sweeping preserves the last phase, so consumers must
honor `running: false` independently of phase.

Keeping deletion inside ingest preserves the removal event for push consumers.
A poll between process exit and the next hook cannot consume that event. If no
further hook arrives, dead files remain while live-session reads exclude them. A sweep
retains successful removals alongside per-record failures and sends those removal events
even if another record could not be read or deleted.

### Child agents

A session can track child agents (Codex subagents, Droid Task children) on its own ledger
record. The aggregate rule: `permission` if the session's own phase or any tracked child is
`permission`; else `working` if the session's own phase is `idle` or `unknown` and any child is
`working` or `compacting` or the session is held for a wake (below); else the session's own
phase. A child that has already finished (`idle`) keeps its entry — so a late event for it is
still recognised — but never holds the session working.

The private record carries `ownPhase` (the session's own latest mapped phase), `children`
(child key → phase, omitted when empty) and `held` (omitted when false; a record without it
reads as not held); none of them reaches the wire. `sessions --json` and sink
bodies only ever see the resolved `phase`. A child key is the first 8 bytes of a SHA-256 digest
of the child's native session or agent ID, hex-encoded (a provisional entry, below, prefixes it
with `prov:`) — raw child IDs are never stored. At
most 64 children are tracked per session; a new child past that cap first evicts an idle entry,
and is not tracked at all if none is idle.

Only a child's start event creates its entry (Codex `SubagentStart`; Droid's child
`SessionStart` with `calling_session_id`). Every other child event can only update an existing
entry — an idle one may be reopened — and otherwise changes nothing, with one exception: a
child's own permission request, seen before its start event (or without one ever arriving),
creates a provisional entry so the approval prompt is not lost. A provisional entry is promoted
to a normal one by the child's start event, is otherwise cleared by that child's next
non-permission event, and never holds the session `working` on its own.

A child's phase follows its own event: its stop event or `running: false` sets it `idle`; a
mapped `permission` sets it `permission` (this includes Codex `PreToolUse` for
`request_user_input`, which the ordinary event mapping marks `idle` but which counts as
`permission` once it is a tracked child waiting on the user); anything else mapped `working` or
`compacting` sets it `working`; everything else sets it `idle`. A child event never changes the
session's own phase and never creates a ledger record for the child itself. The update is
persisted when the session's aggregate phase, terminal, or tmux identity changed, or the child
map itself did; it reaches sinks only when phase, terminal, or tmux changed — a child-map-only
change is written but not forwarded. The forwarded event carries the session's own binding ID,
session, and process, with `event` set to `child:` followed by the child's event name (so a
consumer never sees a bare `UserPromptSubmit` from a child) and `observedAt` from the child
event. When the child event comes from the session's own process, its `terminal`, `tmux`,
`git`, and `remote_host` are refreshed from the child event, which keeps a tmux attach/detach
reported correctly; a git- or remote-host-only change does not by itself make the update
persist or forward.

Fan-out to sinks runs after the ledger lock is released, so two concurrent children can deliver
out of order. After fan-out, hooklinesinker re-reads the session's stored record and, if its
phase or `running` no longer matches what was just sent, sends it once more — reusing the first
fan-out's remaining time budget rather than opening a fresh one, and skipping the re-check
entirely when the first fan-out had no consumers. If the record is gone because a Droid child's
stray record was folded into its parent meanwhile, the parent's status is sent first and the
stray's removal (`swept`) second, so the parent keeps a row the stray had taken over. Either
re-send carries the stored record's own `event` prefixed with `resend:` (e.g.
`resend:UserPromptSubmit`) rather than repeating it verbatim, so a consumer matching on exact
event names does not treat the correction as a second instance of that event. There is no
timer and no retry loop beyond that single re-check.

A session's own `SessionStart` clears its `children`, except `source: compact`, which Codex runs
mid-turn after compaction while the children keep running. Retirement — a superseded binding, or
`SessionEnd` with a process identity — keeps the child keys but sets every one `idle`, so a late
child event is still recognised (and ignored) rather than mistaken for a new session; physical
removal deletes the record and its children together. There is no time-based expiry for a child
that never stops: it holds the session `working` until the session's own `Interrupt` (Codex),
its next `SessionStart`, or its `SessionEnd`. Codex's `interrupt_agent` is known not to send a
`SubagentStop` (upstream: [openai/codex#38142](https://github.com/openai/codex/issues/38142)),
so an interrupted subagent otherwise keeps holding its parent working until one of those events
arrives.

Each agent sources child tracking differently:

- **Codex** shares its root `session_id` across children, each carrying its own `agent_id`.
  `SubagentStart`/`SubagentStop` attach and release a child; the root's `Interrupt` releases
  every child at once.
- **Factory Droid** children are separate sessions linked by `calling_session_id`, including
  grandchildren (a child whose `calling_session_id` names another tracked child). A child event
  that beats its own `SessionStart` is stored as a stray root in the meantime; once the
  `SessionStart` attaches it to its real parent, that stray record is deleted and reported to
  sinks as a `swept` event — the same cleanup OpenCode does for a late `parentID`.
  Droid wakes an idle parent with a `UserPromptSubmit` once a background Task child finishes,
  so a child's own `Stop` that takes it from non-`idle` to `idle` and would otherwise settle the
  aggregate `idle` while the parent's own phase is `idle` or `unknown` instead sets `held`: the
  aggregate stays `working` and nothing is forwarded. A child that goes idle any other way (its
  `SessionEnd` when a task is killed, an `idle_prompt` `Notification`) releases the parent as
  before, since no wake follows it. Any event of the parent's own (the wake,
  its `Stop`, `SessionStart`, `SessionEnd`) or its retirement clears `held`; other children's
  events do not, and a child's `permission` still outranks it. If the wake never comes, the
  parent reads `working` until its next own event. Codex completions do not wake the parent,
  so Codex never holds (`completion_wakes_root` in [`agents.rs`](../src/agents.rs)).
- **Claude Code** is stateless here: `Stop` stays `working` instead of settling `idle` when
  `background_tasks[]` still lists an entry of type `subagent`, `workflow`, `teammate`,
  `MCP task`, or `cloud session` with no `status` field or a `status` of `running`/`pending`; no
  `children` map is kept, and Claude's `SubagentStop` stays outside the installed vocabulary.
- **Pi** and **OpenCode** track children inside their adapters instead of this ledger — see
  [hooks and adapters](hooks-and-adapters.md).

## Consumer contract and delivery

[`protocol.rs`](../src/protocol.rs) defines the wire contract: camelCase keys,
kebab-case agent names, and snake_case phases. One protocol major versions all
consumer-facing envelopes, capability registrations, and status records. Consumers must reject unknown
envelope majors, skip records with unsupported majors, and surface `problems`.
An unrecognized phase string deserializes to `unknown` rather than failing the record,
so a future phase added in a protocol minor degrades instead of breaking. `Agent` has no
such fallback and cannot get a meaningful one — an unknown agent has no hook table,
executable names or lifecycle mapping — so accepting one is a protocol-major bump.

Optional additive fields preserve compatibility. Other wire changes need a
protocol bump and coordinated Juggler and ringleader changes. Consumers read state
through CLI JSON commands; the private files are an implementation detail.
The crate exports only this protocol model and the CLI entry point; stores, hook
reconciliation, normalization, and lifecycle operations remain internal.

[`SinkFanout`](../src/sinks.rs) posts one event per request, sequentially across
consumers with the `status` capability and a sink. Delivery is best effort and
never retried. Registration accepts plain HTTP only for loopback hosts; remote
sinks require HTTPS. Ureq configures separate 200 ms connection, response-receive, and
body-receive timeouts; these are not a 200 ms total ingest deadline. A failed sink
does not undo ledger updates or stop delivery attempts while budget remains.

Live and swept events share a one-second fan-out budget, with the live event attempted
first. Delivery attempts that do not fit are skipped and recorded as a health problem
rather than stalling the editor inside the host's hook timeout. The OpenCode and Pi
adapters also retain at most 32 pending ingest calls.

## Privacy and failures

The capped raw input exists in memory while parsing. `NativeEvent` extracts
session ID, transcript path, tool name, working directory, notification type, and whether
Claude's `background_tasks[]` still names a live child agent (one boolean, derived from each
entry's `type` and `status`; nothing else in that array is read).
Lifecycle handling also reads native `source`, `agent_id`, `source_type`, `source_id`,
`calling_session_id`, and Pi `reason`, retaining only parallel/retired flags and, for a tracked
child, its opaque key digest and phase, alongside private status. Raw payloads are never
persisted or forwarded; tool inputs, outputs, prompts, task names/results, and
`background_tasks[]` descriptions or commands do not enter status records, and a child's raw
session or agent ID is never stored, only its digest. JSON errors report position without the
offending value. A transcript path is metadata; ingest does not open the transcript.

Handled ingest failures exit zero and record health when the store is available; ingest
also prints the joined problem to stderr, which agent hosts ignore. An event name outside
an agent's vocabulary is itself recorded, so a vocabulary change is visible rather than
silent. Store-open and stdin-read failures return before normalization and sweeping.
The health log retains at most 50 problems, and a repeat of an identical problem refreshes
its entry instead of evicting the other 49. Each problem carries a `kind` — `sink` with the
consumer name, `ingest`, `sweep`, or `other` — which is what `doctor` classifies its
last-sink-error check on; records written before the field read back as `other`.
`observedAt` is parsed once, at deserialization, so an unparseable timestamp fails the
health read and is reported rather than replaying forever as a current fault.
`sessions --json` includes problems from the last ten minutes; `doctor` can still report an
older last sink failure.
Ledger writes use a lock and atomic replacement, with private state permissions.
Session envelopes retain valid records and report unreadable or malformed records in
`problems`; malformed health JSON is also reported, rather than treated as an empty log.
Consumer lookup follows the same partial-read policy for sink delivery: valid sinks still
receive events while damaged registrations produce health diagnostics.

[`tests/integration.rs`](../tests/integration.rs) covers identity, PID reuse,
unverifiable records, empty-ID fanout, late hooks, resume, and fork isolation,
privacy, and polling before a later sweep;
[`tests/lifecycle.rs`](../tests/lifecycle.rs) covers cross-agent replacement, independent
sessions, marker migration and wire privacy; [`tests/cli.rs`](../tests/cli.rs) covers
envelopes and ingest failure exits.
See [hooks and adapters](hooks-and-adapters.md) for event production and host
timeouts, and [installation](installation.md) for consumer registration.
