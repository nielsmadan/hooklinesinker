# Status lifecycle

Hooklinesinker stores the latest status per agent/process binding and sends it to
registered sinks. It has no daemon, event history, delivery queue, or retry worker.

## Ingest and identity

The flow starts in [`run_ingest`](../src/main.rs), passes through
[`normalize`](../src/normalize.rs), and updates [`StatusStore`](../src/state.rs):

1. Read at most 1 MiB of native JSON; gather terminal, tmux, Git, and remote-host
   context; discover the owning agent by walking the hook process's ancestors.
2. Map the native event to a phase update, removal, or ignored event. The mapping
   in `normalize.rs` is canonical; unknown event names are ignored.
3. Write or remove the binding, then sweep bindings whose processes have died.
4. Send the normalized event and synthetic removals to registered status sinks.

`session.id` identifies an agent conversation. `bindingId` hashes the agent,
session ID, host, PID, and process start time. Resuming one conversation in two
processes therefore produces two bindings. PID reuse does not preserve a binding:
[`SystemProcessLookup`](../src/processes.rs) checks both PID and start time.

Ancestor detection accepts executable names and script paths under Node, Bun,
or Deno. An unrecognized launcher can leave `process` null. A nonempty-session
record with no process identity is retained as unverifiable diagnostic state;
it is excluded from running results and dead counts, and a sweep leaves it alone.

An empty session ID is different: the normalized event reaches sinks but never
enters the ledger. OpenCode's load-time event uses this path so consumers can
update a terminal row before a conversation ID becomes available.

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
further hook arrives, dead files remain while live-session reads exclude them.

## Consumer contract and delivery

[`protocol.rs`](../src/protocol.rs) defines the wire contract: camelCase keys,
kebab-case agent names, and snake_case phases. One protocol major versions all
consumer-facing envelopes and status records. Consumers must reject unknown
envelope majors, skip records with unsupported majors, and surface `problems`.
Consumers supporting future phase strings must implement their own fallback to
`unknown`; the Rust `Phase` deserializer currently accepts only named variants.

Optional additive fields preserve compatibility. Other wire changes need a
protocol bump and coordinated Juggler and ringleader changes. Consumers read state
through CLI JSON commands; the private files are an implementation detail.

[`SinkFanout`](../src/sinks.rs) posts one event per request, sequentially across
consumers with the `status` capability and a sink. Delivery is best effort and
never retried. Ureq configures separate 200 ms connection, response-receive, and
body-receive timeouts; these are not a 200 ms total ingest deadline. A failed sink
does not undo ledger updates or stop delivery attempts to the remaining sinks.

## Privacy and failures

The capped raw input exists in memory while parsing. `NativeEvent` extracts only
session ID, transcript path, tool name, working directory, and notification type.
Raw payloads are never persisted or forwarded; tool inputs, outputs, and prompts
do not enter status records. JSON errors report position without the offending
value. A transcript path is metadata; ingest does not open the transcript.

Handled ingest failures exit zero and record health when the store is available.
Store-open and stdin-read failures return before normalization and sweeping.
The health log retains at most 50 problems. `sessions --json` includes problems
from the last ten minutes; `doctor` can still report an older last sink failure.
Ledger writes use a lock and atomic replacement, with private state permissions.

[`tests/integration.rs`](../tests/integration.rs) covers identity, PID reuse,
unverifiable records, empty-ID fanout, privacy, and polling before a later sweep;
[`tests/cli.rs`](../tests/cli.rs) covers envelopes and ingest failure exits.
See [hooks and adapters](hooks-and-adapters.md) for event production and host
timeouts, and [installation](installation.md) for consumer registration.
