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
background tasks) extends the private ledger record, not the wire protocol or this decision's
privacy boundary:

- The ledger may store only a child's opaque ID digest (the first 8 bytes of a SHA-256 hash of
  its native session or agent ID, hex-encoded) and its current phase. It never stores a raw
  child ID, a prompt, a task name or result, or a `background_tasks[]` description or command.
- Normalization derives exactly one boolean from Claude's `background_tasks[]` — whether it
  still names a live subagent, workflow, teammate, MCP task, or cloud session — and discards
  the rest of the array; nothing else from it reaches the ledger or a sink.
- `ownPhase` and `children` are additive, private record fields (see
  [child agents](../status-lifecycle.md#child-agents)). `status.phase`, the only phase a
  consumer ever sees, stays the protocol 1 aggregate — no capability or schema change follows
  from this.
