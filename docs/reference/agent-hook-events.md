# Agent hook events

How external coding agents expose session, turn, tool, and user-interaction
lifecycle changes.

- [Names used in hook records](#names-used-in-hook-records)
- [Question and permission lifecycle](#question-and-permission-lifecycle)
- [Observed Droid timelines](#observed-droid-timelines)
- [Cross-agent lifecycle comparison](#cross-agent-lifecycle-comparison)
- [Interpreting tool events](#interpreting-tool-events)
- [How this affects hooklinesinker](#how-this-affects-hooklinesinker)
- [Sources](#sources)

## Names used in hook records

This reference separates three concepts that are easy to conflate:

- **Event name** is the lifecycle event, such as `PreToolUse`, `PostToolUse`, or
  `Notification`.
- **Tool name** identifies the tool involved in a tool event, such as `Read`,
  `AskUser`, or `Execute`. It is carried in the event's `tool_name` field.
- **Notification type** explains a `Notification` event, such as
  `permission_prompt`, `elicitation_dialog`, or `idle_prompt`.

`Read` is a Droid tool that reads a file. It is not a hook event. Instead of
joining the event name and tool name with a slash, this documentation says:

> a `PostToolUse` event for the `Read` tool

## Question and permission lifecycle

Agents do not expose one common "user answered" event. Some expose a dedicated
resolution event, while others expose only the completion of the tool that was
waiting.

| Agent | Waiting begins | Waiting ends |
|---|---|---|
| Claude Code | `PermissionRequest` | No dedicated resolution event; a later tool or turn event supplies the next state |
| Codex | `PreToolUse` with `tool_name: request_user_input`, or `PermissionRequest` | The corresponding `PostToolUse`, or a later tool or turn event |
| Factory Droid | `Notification` with `notification_type: elicitation_dialog` or `permission_prompt` | No dedicated answer event; the corresponding `PostToolUse` follows after the interaction resolves |
| Qwen Code | `PermissionRequest`, or `Notification` with `notification_type: permission_prompt` | `PermissionDenied`, the corresponding `PostToolUse`, or a later turn event |
| Kimi Code CLI | `PermissionRequest` | `PermissionResult` |
| OpenCode | `permission.asked` | A later authoritative session status, normally `session.status.busy` |
| Pi | `permission_prompt`, synthesized from the optional permission extension | `permission_resolved`, synthesized after its pending prompts resolve |

For Droid, an `AskUser` call therefore has three relevant records:

1. A `PreToolUse` event with `tool_name: AskUser` before the question opens.
2. A `Notification` event while Droid is waiting.
3. A `PostToolUse` event with `tool_name: AskUser` after the user answers or
   cancels the question.

The third record is not a special answer event. It is the ordinary completion
event for the `AskUser` tool call.

## Observed Droid timelines

These examples were extracted on 2026-09-16 from local Droid 0.218.2 session
transcripts. Prompts, answers, tool inputs, and tool outputs were not read or
copied. Transcript timestamps describe when each hook record was persisted, not
a global order for the independent hooklinesinker processes.

### One question

The same tool call ID appears on the events before and after the wait:

| Time | Event name | Tool name | Meaning |
|---|---|---|---|
| 12:05:00.112 | `PreToolUse` | `AskUser` | Droid is about to open a question |
| 12:05:00.351 | `Notification` | None | Droid reports that it needs user input |
| 13:43:29.093 | `PostToolUse` | `AskUser` | The question tool completed after the user responded |

There is no separate hook event between the answer and that `PostToolUse`.

### A question alongside another tool

The incident that motivated this reference involved two tool calls from one
assistant message:

| Time | Event name | Tool name | Tool call |
|---|---|---|---|
| 22:43:45.432 | `PreToolUse` | `TodoWrite` | A |
| 22:43:45.434 | `PreToolUse` | `AskUser` | B |
| 22:43:45.659 | `PostToolUse` | `TodoWrite` | A |
| 22:43:45.694 | `Notification` | None | Not applicable |

Tool call B remained open, so no `PostToolUse` event for `AskUser` had fired.
The completion of tool call A did not mean the question had been answered.

This is why a generic `PostToolUse` event cannot safely clear every waiting
state. A completion is authoritative only for the interaction it resolves.

### Limits of the evidence

Droid transcripts retain the event name, tool matcher, tool call ID, and hook
completion timing. They do not retain the raw hook input for `Notification`, so
the transcript alone does not reveal its `notification_type`.

Hooklinesinker deliberately discards raw native input and stores only the latest
normalized status per binding. Its `observedAt` value has one-second precision.
The final ledger can show which status won, but cannot reconstruct every
intermediate event or prove the scheduling order of concurrent ingest processes.

## Cross-agent lifecycle comparison

This table names the event available at each lifecycle point. "Later status"
means the agent has no dedicated resolution event for that interaction.

| Agent | Turn begins | Before a tool | After a tool | User wait begins | User wait ends | Turn settles |
|---|---|---|---|---|---|---|
| Claude Code | `UserPromptSubmit` | `PreToolUse` | `PostToolUse` or `PostToolUseFailure` | `PermissionRequest` | later tool or turn event | `Stop` or `StopFailure` |
| Codex | `UserPromptSubmit` | `PreToolUse` | `PostToolUse` | `PermissionRequest`, or `PreToolUse` for `request_user_input` | corresponding `PostToolUse` | `Stop` |
| Factory Droid | `UserPromptSubmit` | `PreToolUse` | `PostToolUse` | `Notification` | corresponding `PostToolUse` | `Stop` |
| Qwen Code | `UserPromptSubmit` | `PreToolUse` | `PostToolUse` or `PostToolUseFailure` | `PermissionRequest` or `Notification` | `PermissionDenied`, corresponding `PostToolUse`, or later status | `Stop` or `StopFailure` |
| Kimi Code CLI | `TurnStarted` or `UserPromptSubmit` | `PreToolUse` | `PostToolUse` or `PostToolUseFailure` | `PermissionRequest` | `PermissionResult` | `Stop` or `StopFailure` |
| OpenCode | `session.status.busy` | No equivalent used | No equivalent used | `permission.asked` | `session.status.busy` | `session.status.idle`, `session.idle`, or `session.error` |
| Pi | `agent_start` | No equivalent used | No equivalent used | `permission_prompt` | `permission_resolved` | `agent_settled` |

Factory Droid's `Notification` needs its `notification_type` to establish the
meaning. `permission_prompt` and `elicitation_dialog` mean Droid is waiting for
the user. `idle_prompt` means it is waiting for ordinary input. Other
notification types do not describe session phase.

Codex's `request_user_input` is a tool name, not an event name. Pi's permission
events are synthesized from the optional `@gotgenes/pi-permission-system` event
bus rather than Pi core. OpenCode supplies authoritative session statuses
instead of before- and after-tool transitions.

## Interpreting tool events

`PreToolUse` is strong evidence that a particular tool is about to begin.
`PostToolUse` is evidence only that that particular tool completed. It is not a
turn boundary and does not prove that:

- no other tool is still running;
- the model is no longer waiting for the user;
- the whole turn is working rather than idle, permission-gated, or compacting.

When tools can overlap, consumers need correlation or state precedence. Event
arrival order alone is insufficient because each command hook starts a separate
ingest process and sink delivery is asynchronous with respect to other hooks.

## How this affects hooklinesinker

The external lifecycle comparison constrains normalization:

- Event registration and current phase mapping are canonical in
  [`src/events.rs`](../../src/events.rs).
- [`src/normalize.rs`](../../src/normalize.rs) extracts `tool_name` and
  `notification_type`; it does not combine them with the event name.
- Droid command hooks can overlap. Pi avoids the same race by serializing its
  adapter calls through one promise queue.
- Generic tool completion is not strong enough to clear an unrelated waiting
  state. A robust transition needs the responsible tool call or stronger state
  precedence.

## Sources

- Verified Droid behavior: two local Droid 0.218.2 session transcript probes,
  inspected 2026-09-16
- OpenCode adapter: [`adapters/opencode-hooklinesinker.ts`](../../adapters/opencode-hooklinesinker.ts)
- Pi adapter: [`adapters/pi-hooklinesinker.ts`](../../adapters/pi-hooklinesinker.ts)
- Factory Droid hook contract:
  <https://docs.factory.ai/harness/hooks.md>
- Status storage and evidence limits:
  [Status lifecycle](../status-lifecycle.md)
