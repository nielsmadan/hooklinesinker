# 0003 — Aggregate child agents onto the root's record

**Status:** accepted
**Recorded:** 2026-10-08, with the child-agent phase tracking implementation.

## Context

A parent agent ends its turn (`Stop` → idle) while background or child agents keep working, and
child events either overwrite the parent's phase or arrive as sessions of their own. Consumers
such as Juggler key rows by terminal and tmux pane, so a child reported as its own session
takes over or deletes the parent's row. Each agent exposes children differently: Codex children
share the root's `session_id` and carry `agent_id`; Droid children are separate sessions in the
parent's process whose `SessionStart` alone carries `calling_session_id`; Claude's `Stop` input
lists `background_tasks`; Pi and OpenCode children are visible only inside their hosts.
Hooklinesinker has no daemon, so any state that spans hook invocations lives in the ledger.

## Decision

Store child state on the root's private ledger record (`ownPhase` plus a map of child-id digest
to phase) and report only the root, whose `phase` is the aggregate: permission if the root or
any child waits on the user, else working while any child works, else the root's own phase.
Children are never reported as sessions. Raw child ids, prompts, task names and results are
never stored ([0001](0001-status-events-only.md) amendment).

Rejected: separate child records folded at read time (every ledger reader — `sessions --json`,
sweep, retirement, consumer hydration — would have to hide children); consumer-side
aggregation (needs a protocol change and duplicates the logic in every sink); counting
start/stop events (Claude's internal helper agents also fire `SubagentStop`, and Codex skips it
on error or interrupt).

Only an explicit start event creates a child (Codex `SubagentStart`, Droid `SessionStart` with
`calling_session_id`); other child events update an existing entry. Untracked children can
still surface a permission prompt through a provisional entry that never holds the root
working. Claude stays stateless: a `Stop` whose `background_tasks` lists a running or pending
subagent, workflow, teammate, MCP task or cloud session maps to working. Pi and OpenCode track
children in their adapters and emit only root events.

## Consequences

- Children have no time-based expiry. A child that ends without a stop signal holds the root
  working until the root's next `SessionStart`, a Codex root `Interrupt`, or `SessionEnd`
  (Codex `interrupt_agent` sends no `SubagentStop`: openai/codex#38142). Clearing children on
  the root's next prompt was rejected because the user can prompt while children still run.
- Child-caused updates carry `event` values prefixed `child:` and corrective re-sends `resend:`,
  so consumers that branch on exact event names (Juggler's `UserPromptSubmit`) never see a
  replayed or borrowed name.
- Registering Codex `SubagentStart`/`SubagentStop` makes existing Codex installs report drift and
  needs consumer trust. Juggler re-trusts them only for users who already trusted its hooks, and
  must ship its matching change before any hooklinesinker release with these hooks reaches users,
  because the shared installation is never downgraded and ringleader bundles it too.
- Pi releases its hold on a run's completion only when the completion will not wake the parent
  (`triggerTurn: false`); in non-default pi-subagents setups a session stays working until its
  next turn.
- Droid and OpenCode background mode hold the root working through the wake that follows a
  child's completion (a private `held` flag set by a Droid child's own `Stop`; withheld idle in
  the OpenCode adapter, only when `OPENCODE_EXPERIMENTAL_BACKGROUND_SUBAGENTS` is enabled), so the ~0.6 s / ~134 ms idle flash before the wake is not reported. The cost: a
  completion that never wakes the root (a cancelled OpenCode child, a Droid crash) leaves it
  working until its next own event.
