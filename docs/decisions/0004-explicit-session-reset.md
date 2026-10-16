# 0004 — Explicit session reset without removal events for live sessions

**Status:** accepted
**Recorded:** 2026-10-14, with `sessions reset`.

## Context

The ledger can hold records that are wrong rather than dead: older versions wrote a detached
tmux worker's record with the terminal tab that started its server, and kept duplicate records
when macOS renamed the host. Ingest corrects those causes, but a record whose process is alive
and never hooks again stays in `sessions --json` and keeps reaching consumers at hydration.
Users of a consumer had no way to clear it short of deleting files in private state.

## Decision

Add `sessions reset`, which deletes every status record under the ledger lock and leaves
consumer registrations, the health log, hooks and binaries alone. It is the only deleter
besides the ingest sweep. It sends the sweep's `swept` event for each dead session it removes
and nothing for live ones. Each running session's next hook recreates its record, normally
under the same `bindingId`. It is a subcommand rather than a flag on `sessions`, so the listing
keeps its read-only contract and each has one envelope.

Reset asks for no confirmation: every record it deletes is rebuilt from the next hook, and a
consumer that offers it confirms in its own interface. Unlike the read commands it exits 1 on
any problem, so its problems also go to stderr for callers that stop at a nonzero exit. They
stay plain strings, as in `consumers --json`: callers act on the exit status and `removed`, and
the strings are for a person to read.

Rejected: sending `running:false` for each live record (a terminal-keyed consumer would drop
live rows that come back moments later, and a stale record sharing a terminal with a live
session would delete the live row); deleting dead records silently (a push consumer other than
the one resetting would keep a dead row that no later event clears, which the ingest sweep
exists to prevent); wiping records automatically on upgrade (it would discard correct state on
every release to fix a rare one).

## Consequences

- Reset clears the ledger for every consumer on the machine. Pull consumers see the empty
  ledger at their next poll; push consumers keep live rows until each session's next event
  or their next hydration, unless they clear them as part of offering the reset.
- Idle sessions are missing until their next turn, child aggregation restarts empty (children
  already running no longer hold their root working, an exception to
  [0003](0003-child-agent-aggregation.md)), and retired markers are gone, so a late
  end hook for a retired binding is forwarded once more.
