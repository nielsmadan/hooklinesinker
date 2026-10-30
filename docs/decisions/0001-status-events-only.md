# 0001 — Capture status events with an allowlisted protocol

**Status:** accepted
**Recorded:** 2026-09-11, extracted from the existing status-events-only design note.

## Context

Juggler and ringleader need agent status without each installing its own hooks. Capturing
every native event would incur a process launch even when nobody wants the result. Some hooks
run synchronously before an agent proceeds, so forwarding high-frequency output events would
put unnecessary work directly in that path.

Hooklinesinker has no resident process to hold a queue. General event delivery would require
new persistence, ordering, and reliability guarantees. Persisting native tool payloads would
also change the privacy boundary.

## Decision

Protocol 1 offers the named `status` capability. Install the native hooks needed for status,
normalize their inputs into a typed, allowlisted event, retain the latest status per binding,
and offer CLI snapshots plus best-effort sink delivery. Do not persist, log, or forward raw
native input. The original input does pass through a capped memory buffer; only the six fields
in [NativeEvent](../../src/normalize.rs) are deserialized into the native event model —
`session_id`, `transcript_path`, `tool_name`, `cwd`, `notification_type`, and a boolean derived
from `background_tasks`.

Capture a second event class only when a concrete consumer needs it. That addition must add
a `Capability` variant, its hook-event subscription, an allowlisted schema, and explicit
delivery semantics. Review
protocol compatibility against existing consumers rather than assuming every new capability
fits protocol 1. At that point, derive installed hooks from the union of active subscriptions;
the current [installers](../../src/hooks.rs) use a fixed status hook set.

## Consequences

- A snapshot can recover missed sink delivery of a recorded status. Process liveness also
  filters sessions whose end event never arrived. A missed idle event for a process that is
  still alive remains stale until another hook updates it; this is not complete event replay.
- Sinks accept loss. There is no retry queue, history, or ordering guarantee across ingest
  processes. The HTTP client has 200 ms timeouts for connection, response, and body phases,
  not one global 200 ms deadline. See [status lifecycle](../status-lifecycle.md).
- New event capabilities require installer and consumer changes as well as a schema. Stronger
  delivery guarantees may justify a daemon, but status alone does not require one.

The capability and public event contracts live in [protocol.rs](../../src/protocol.rs).
[events.rs](../../src/events.rs) maps each capability to the native hook events it requires,
and sink dispatch matches the typed capability rather than comparing open-ended strings.

## Amendment (2026-10-08): child agents

Tracking a session's child/background agents (Codex subagents, Droid Task children, Claude
background tasks) extends the private ledger record and this decision's privacy boundary only
by the child count and the details named in the 2026-10-30 amendment below:

- The ledger may store only a child's opaque ID digest (the first 8 bytes of a SHA-256 hash of
  its native session or agent ID, hex-encoded) and its current phase. It never stores a raw
  child ID, a prompt, a task name or result, or a `background_tasks[]` description or command.
- Normalization derives from Claude's `background_tasks[]` a boolean (whether it still names a
  live subagent, workflow, teammate, MCP task, or cloud session) and a count of live entries,
  and discards the rest of the array; the count reaches sinks as `activity.subagents`.
- `ownPhase` and `children` are additive, private record fields (see
  [child agents](../status-lifecycle.md#child-agents)). `status.phase`, the only phase a
  consumer ever sees, stays the protocol 1 aggregate — no capability change follows
  from this.

## Amendment (2026-10-30): session details

Consumers show a session's model, settings and current activity, so the status record carries a
small set of additional allowlisted fields. They are optional additions to protocol 1, with no
protocol bump and no new `Capability`: older consumers ignore them and older records parse
without them.

- `session` gains `title`, `model`, `effort` and `permissionMode`; a new top-level `activity`
  object carries `tool`, `problem`, `subagents`, `contextPercent` and `turns`. Hooklinesinker
  owns the rules that merge them across events (see
  [session details](../status-lifecycle.md#session-details)); consumers display what they
  receive.
- The privacy boundary moves only by these named values. They are read leniently from named
  hook-input fields, so a bad value is skipped rather than failing the event, and every string
  passes one sanitizer (control and format characters removed, at most 120 Unicode scalars).
  Prompt text, tool input and output, message bodies and tool error text are still never read;
  an `error` field is read only from a turn-failure event, as a problem token. Raw payloads are
  still never persisted or forwarded.
- Events that only update details (a model switch, for example) are stored without being sent,
  so they add no deliveries, never change a phase and never create a record. Droid's
  `PostToolUse` is the one detail-only event that is sent, and only when it clears a tool the
  consumer is showing; it still never changes a phase or creates a record.
- `activity.turns` counts the turn starts hooklinesinker already recognises for each agent
  (`UserPromptSubmit`, Kimi `TurnStarted`, Pi `agent_start`). Hooklinesinker owns the count
  because it alone sees the turn boundary: a session held `working` by background work starts
  its next turn without a phase change, so a consumer counting `idle` → `working` transitions
  undercounts. The count is a number derived from event names; it reads no prompt.

This also revises the child-agents amendment above: the ledger still stores only child digests
and phases, but the number of tracked children that are not idle now reaches the wire as
`activity.subagents`, and for Claude normalization also derives a count of live child entries
from `background_tasks[]` beside the boolean, still discarding everything else in that array.
A change in that count is forwarded even when the aggregate phase does not change.
