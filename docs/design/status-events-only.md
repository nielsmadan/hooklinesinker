# Why hooklinesinker only ships status events

Protocol 1 exposes exactly one capability, `status`. Consumer registration rejects anything
else, hooks are installed only for the events the status projection maps, and every other event
an agent could emit is never captured at all. This document records why that is a decision, not
an accident, and what the extension path is when a real second use case shows up.

## The tempting generalization

"Capture every event, and when no consumer is registered for an event type, discard it on
arrival." Two properties of this tool's architecture make that the wrong shape.

### Discard-at-receipt is the expensive form of discard

There is no daemon. Every captured event costs a fork/exec of this binary inside the agent's
hook path — and some of those paths are synchronous (a `PreToolUse` hook runs before the tool
call proceeds). Several agents emit high-frequency events (Qwen's `MessageDisplay` fires on
streamed reply text); capturing them means spawning a process per chunk of model output just to
throw the event away. The cheap discard is not installing the hook at all. The consumer registry
already knows what every registered consumer wants, so the right mechanism is
subscription-derived hook installation: the installed hook set per agent is the union of the
event classes active consumers subscribed to. The installers are already idempotent per-event
reconcilers with exact ownership matching, so adding and removing event entries as subscriptions
change is mechanism that exists, not mechanism to invent.

### "Group and hold" implies a bus this tool deliberately is not

With no resident process there is nowhere to hold events. Either they are persisted — which for
raw events collides with the privacy boundary (raw native input is never persisted, logged, or
forwarded, enforced structurally today: extraction reads a five-field allowlist and tool
payloads never materialize in memory) — or they are relayed synchronously to sinks with a 200 ms
budget and no retry, and loss is accepted.

Status tolerates that lossy transport because it is a projection: the ledger plus PID-liveness
re-converges on the truth no matter which individual events were dropped. An arbitrary event
stream has no such convergence — its consumers generally need ordering and delivery guarantees
this tool was designed not to provide. A generic event bus is therefore not a schema change; it
is a different reliability contract.

## The extension path

When a concrete second consumer arrives with a concrete event need:

1. Define a **named capability** for the event class (`status` today; e.g. `tool-activity`,
   `notifications` later). Capabilities are additive; protocol major stays 1.
2. Give the class a **typed, allowlisted schema** of its own, normalized per agent the way
   status phases are. Raw passthrough of native payloads is off the table permanently — it is
   incompatible with the privacy boundary and with versioning.
3. Derive each agent's installed hook set from the **union of active subscriptions**, so events
   nobody wants are never captured.
4. Decide the class's **delivery semantics explicitly** (best-effort sink fan-out like status,
   or something stronger — and if stronger, that is the moment to argue about a daemon, not
   before).

Until then: if you have a use case, open an issue naming the events and what you would do with
them — the schema and the delivery semantics should be designed against a real consumer, not a
hypothetical one.
