# Status lifecycle

Hooklinesinker stores the latest status per agent/process binding and sends it to
registered sinks. It has no daemon, event history, delivery queue, or retry worker.

## Ingest and identity

The flow starts in [`run_ingest`](../src/main.rs), which delegates to
[`handle_ingest`](../src/ingest.rs). That application layer normalizes the event and updates
[`StatusStore`](../src/state.rs):

1. Read at most 1 MiB of native JSON; gather terminal, tmux, Git, and remote-host
   context; discover the owning agent by walking the hook process's ancestors.
2. Map command-hook events through `events.rs` and adapter-specific events through
   `normalize.rs` to a phase update, removal, or ignored event. Event names outside an
   agent's vocabulary become ingest diagnostics.
3. Write or remove the binding, retire superseded foreground bindings, then
   sweep bindings whose processes have died.
4. Send the normalized event and synthetic removals to registered status sinks.

`session.id` identifies an agent conversation. `bindingId` hashes the agent,
session ID, host, PID, and process start time. Resuming one conversation in two
processes therefore produces two bindings. PID reuse does not preserve a binding:
[`SystemProcessLookup`](../src/processes.rs) checks both PID and start time. Liveness checks
refresh the requested PID while the ledger lock is held; the earlier ancestor-discovery
snapshot cannot classify a newly started process as dead. Only ancestor discovery needs that
snapshot, so `sessions` and `doctor` use `SystemProcessLiveness`, which holds no process
table at all.

Ancestor detection accepts executable names and script paths under Node, Bun,
or Deno. An unrecognized launcher can leave `process` null. A nonempty-session
record with no process identity is retained as unverifiable diagnostic state for
up to 24 hours. It is excluded from running results, then the next ingest sweep
removes it without emitting a synthetic removal event.

An empty session ID is different: `normalize` returns it as forward-only, so the event
reaches sinks but cannot enter the ledger. OpenCode's load-time event uses this path so consumers can
update a terminal row before a conversation ID becomes available.

Foreground bindings replace one another within the same agent, PID, process start
time, host, terminal, tmux pane, and remote-host context. Tmux session names are
navigation metadata: renames or failed name lookups do not change binding identity.
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
| Codex | Ordinary activity, including a root `source: fork`; `source: resume` reactivates | `agent_id` identifies child activity; app-server and legacy mcp-server modes stay independent |
| Droid | Ordinary activity; `SessionStart source: resume` reactivates | Distinct owner processes stay independent; no guaranteed native same-process child discriminator |
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

**Unverified Droid case:** a child with a different ID, the same owner process, and no child
marker currently looks like a replacement. Real child hook metadata/process ancestry still
needs verification; conservative retention is the safe fallback if exclusivity is unknown.
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
working, waiting for permission, or compacting. `phase` records the latest mapped
hook event; process inspection cannot reconstruct a missed working-to-idle event.
It stays at its previous value until another relevant event arrives.

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
never retried. Ureq configures separate 200 ms connection, response-receive, and
body-receive timeouts; these are not a 200 ms total ingest deadline. A failed sink
does not undo ledger updates or stop delivery attempts while budget remains.

Live and swept events share a one-second fan-out budget, with the live event attempted
first. Delivery attempts that do not fit are skipped and recorded as a health problem
rather than stalling the editor inside the host's hook timeout. The OpenCode and Pi
adapters also retain at most 32 pending ingest calls.

## Privacy and failures

The capped raw input exists in memory while parsing. `NativeEvent` extracts
session ID, transcript path, tool name, working directory, and notification type.
Lifecycle handling also reads native `source`, `agent_id`, `source_type`, `source_id`,
and Pi `reason`, retaining only parallel and retired flags alongside private status.
Raw payloads are never persisted or forwarded; tool inputs, outputs, and prompts
do not enter status records. JSON errors report position without the offending
value. A transcript path is metadata; ingest does not open the transcript.

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
