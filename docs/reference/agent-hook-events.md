# Agent hook events

How external coding agents expose session, turn, tool, and user-interaction
lifecycle changes.

- [Names used in hook records](#names-used-in-hook-records)
- [Question and permission lifecycle](#question-and-permission-lifecycle)
- [Observed Droid timelines](#observed-droid-timelines)
- [Cross-agent lifecycle comparison](#cross-agent-lifecycle-comparison)
- [Session identity, subagents, and shared processes](#session-identity-subagents-and-shared-processes)
- [OpenCode selection is navigation](#opencode-selection-is-navigation)
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
| Claude Code | `PreToolUse` or `PermissionRequest` for `AskUserQuestion` (input wait); other `PermissionRequest` events (approval) | `PostToolUse`, `PostToolUseFailure`, or a later tool or turn event supplies the next state |
| Codex | `PreToolUse` with `tool_name: request_user_input`, or `PermissionRequest` | The corresponding `PostToolUse`, or a later tool or turn event |
| Factory Droid | `Notification` with `notification_type: elicitation_dialog` or `permission_prompt` | No dedicated answer event; the corresponding `PostToolUse` follows after the interaction resolves |
| Qwen Code | `PermissionRequest`, or `Notification` with `notification_type: permission_prompt` | `PermissionDenied`, the corresponding `PostToolUse`, or a later turn event |
| Kimi Code CLI | `PermissionRequest` | `PermissionResult` |
| Devin CLI | `PermissionRequest` (permission decision, possibly resolved by policy) | No documented resolution; hooklinesinker conservatively waits for root prompt/start/stop |
| Copilot CLI | lowercase `notification`: `permission_prompt` or `elicitation_dialog` | Root prompt/start/stop; tool completion alone does not resolve a wait |
| Cursor CLI | No dedicated user-wait signal used by HLS; `preToolUse` alone stays working | Tool completion or a later root event supplies the next state |
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
| Claude Code | `UserPromptSubmit` | `PreToolUse` | `PostToolUse` or `PostToolUseFailure` | `PermissionRequest`, or `PreToolUse` for `AskUserQuestion` | corresponding tool completion or later turn event | `Stop` or `StopFailure` |
| Codex | `UserPromptSubmit` | `PreToolUse` | `PostToolUse` | `PermissionRequest`, or `PreToolUse` for `request_user_input` | corresponding `PostToolUse` | `Stop` |
| Factory Droid | `UserPromptSubmit` | `PreToolUse` | `PostToolUse` | `Notification` | corresponding `PostToolUse` | `Stop` |
| Qwen Code | `UserPromptSubmit` | `PreToolUse` | `PostToolUse` or `PostToolUseFailure` | `PermissionRequest` or `Notification` | `PermissionDenied`, corresponding `PostToolUse`, or later status | `Stop` or `StopFailure` |
| Kimi Code CLI | `TurnStarted` or `UserPromptSubmit` | `PreToolUse` | `PostToolUse` or `PostToolUseFailure` | `PermissionRequest` | `PermissionResult` | `Stop` or `StopFailure` |
| Devin CLI | `UserPromptSubmit` | `PreToolUse` | `PostToolUse` | `PermissionRequest` | root prompt/start/stop | `Stop` |
| Copilot CLI | `UserPromptSubmit` | `PreToolUse` | `PostToolUse` / `PostToolUseFailure` | `notification` permission / elicitation | root prompt/start/stop | `Stop` |
| OpenCode | `session.status.busy` | No equivalent used | No equivalent used | `permission.asked` | `session.status.busy` | `session.status.idle`, `session.idle`, or `session.error` |
| Pi | `agent_start` | No equivalent used | No equivalent used | `permission_prompt` | `permission_resolved` | `agent_settled` |

Claude Code's `PostModelSwitch` (`to_model`) reports a mid-session model change and has no phase
of its own; hooklinesinker registers it as an annotation. Claude Code ignores hook names it does
not know since 2.1.101, and `PostModelSwitch` needs 2.1.251 or later to fire.

Factory Droid's `Notification` needs its `notification_type` to establish the
meaning. `permission_prompt` and `elicitation_dialog` mean Droid is waiting for
the user. `idle_prompt` means it is waiting for ordinary input. Other
notification types do not describe session phase.

Codex's `request_user_input` is a tool name, not an event name. Pi's permission
events are synthesized from the optional `@gotgenes/pi-permission-system` event
bus rather than Pi core. OpenCode supplies authoritative session statuses
instead of before- and after-tool transitions.

## Session identity, subagents, and shared processes

A session ID identifies a conversation, not an operating-system process. One
process can host several conversations, including children. The process launched
for a command hook is also distinct from the agent process that owns the session.
A different session ID with the same owner PID therefore does not, by itself,
prove that the previous conversation was replaced.

Evidence below was inspected on **2026-09-18**. **Documented** means the upstream
contract describes it; **source-inspected** means the linked implementation shows
it. These findings are not end-to-end verification of installed agent versions.

| Agent | Session and child identity | Evidence and limits |
|---|---|---|
| Claude Code | Documented subagent examples use the parent `session_id` with a separate child `agent_id`; tool hooks inside children carry `agent_id` and `agent_type`. | **Documented:** [hook inputs and subagent events](https://code.claude.com/docs/en/hooks). In-process teammates also exist; do not assume every child has a separate PID. `Stop`'s `background_tasks[]` array, used to keep a parent `working` while a background agent runs, was verified live on Claude Code 2.1.280. |
| Codex | A child shares the root's `session_id`; its own `agent_id` (plus `agent_type`) on `SubagentStart`/`SubagentStop` and tool hook inputs is what distinguishes it from the root. | **Source-inspected** (rust-v0.155.1): [hook runtime](https://github.com/openai/codex/blob/main/codex-rs/core/src/hook_runtime.rs) and [PreToolUse payload](https://github.com/openai/codex/blob/main/codex-rs/hooks/src/events/pre_tool_use.rs). The shared `session_id` was re-verified against a codex-cli 0.157.0 subagent rollout; installed 0.161.0 has no subagent rollout yet to re-check live. `SubagentStart`/`SubagentStop` are distinct lifecycle events from root session starts/stops. |
| OpenCode | Child sessions have their own IDs and a `parentID`. One backend can host multiple root and child sessions. | **Source-inspected:** [task session creation](https://github.com/anomalyco/opencode/blob/65c35977bd564e23c0e9cf124b3e3e3b9308e9e8/packages/opencode/src/tool/task.ts) and [backend plugin event subscription](https://github.com/anomalyco/opencode/blob/65c35977bd564e23c0e9cf124b3e3e3b9308e9e8/packages/opencode/src/plugin/index.ts). A root session is not necessarily the currently viewed session. SDK 1.17.0 types confirm `permission.asked`/`permission.replied` carry `properties.id`/`properties.requestID`, keyed to the session that asked. |
| Pi | SDK extensions can create child sessions with distinct IDs inside the parent process. Headless extension contexts expose `hasUI: false`. | **Source-inspected:** [pi-subagents session construction](https://github.com/gotgenes/pi-packages/blob/9bfe0369940766f4571f7c46fd1ab74ecb330166/packages/pi-subagents/src/lifecycle/create-subagent-session.ts) and [Pi extension context](https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/src/core/extensions/types.ts). `hasUI` also includes RPC contexts; it does not establish exclusive ownership of a terminal. Custom extensions may behave differently. pi-subagents **0.71.0**'s `subagent:async-started`/`subagent:async-complete` events and its `status.json` run-state files were inspected to track async runs without blocking session start. |
| Qwen Code | The inspected native subagent path shares the parent hook system; base hook input gets `session_id` from its configuration. It also exposes optional `source_type` and `source_id` attribution. | **Source-inspected:** [subagent manager](https://github.com/QwenLM/qwen-code/blob/be9d5e8d3f1470d80ec36546788c801225d66e3e/packages/core/src/subagents/subagent-manager.ts) and [hook input construction](https://github.com/QwenLM/qwen-code/blob/be9d5e8d3f1470d80ec36546788c801225d66e3e/packages/core/src/hooks/hookEventHandler.ts). This path does not establish that every ACP or custom integration uses the same identity model. |
| Kimi Code CLI | In the inspected Python implementation, a child runtime reuses the parent session and does not inherit its hook engine. | **Source-inspected:** [runtime copy for subagents](https://github.com/MoonshotAI/kimi-cli/blob/main/src/kimi_cli/soul/agent.py). Scope this finding to that implementation; it is not a guarantee for other Kimi clients or shared server modes. |
| Factory Droid | A Task child's `SessionStart` (startup or resume) carries `calling_session_id`, naming the parent's `session_id` — or, for a grandchild, another tracked child's. A `SessionStart` without it is always a root. | **Verified** against a local capture (2026-10-06, droid 0.218.2): see [Sources](#sources) and [child agents](../status-lifecycle.md#child-agents). A reliable discriminator for same-process Droid sessions *without* that marker is still unestablished; do not infer process separation from the existence of a task ID alone. |

Preserving a marked child prevents it from replacing another conversation. For Codex and Droid
it now does more than that: child activity is aggregated onto the parent's phase rather than
only being kept from replacing it — see [child agents](../status-lifecycle.md#child-agents). Pi
and OpenCode aggregate child and async-run activity inside their own adapters instead of the
shared ledger. Claude, Qwen and Kimi still get only the isolation half: a recognized child cannot
replace the parent, but when parent and child hooks share one session ID a child's hook still
overwrites the parent's phase. Claude alone also keeps its own `Stop` at `working` while
`background_tasks[]` names live child work; its children are not tracked.

## OpenCode selection is navigation

**Source-inspected at `65c35977bd564e23c0e9cf124b3e3e3b9308e9e8`, 2026-09-18.**
The [`tui.session.select` handler](https://github.com/anomalyco/opencode/blob/65c35977bd564e23c0e9cf124b3e3e3b9308e9e8/packages/tui/src/app.tsx)
calls `route.navigate`. The [route implementation](https://github.com/anomalyco/opencode/blob/65c35977bd564e23c0e9cf124b3e3e3b9308e9e8/packages/tui/src/context/route.tsx)
updates local UI state; selection does not end the previous session. The ordinary
[session picker](https://github.com/anomalyco/opencode/blob/65c35977bd564e23c0e9cf124b3e3e3b9308e9e8/packages/tui/src/component/dialog-session-list.tsx)
also calls `route.navigate` directly, without publishing `tui.session.select`.

Consequently, a backend plugin cannot use that event as either an exhaustive
foreground-selection feed or evidence that the previous session ended. Selecting
A, then B, is compatible with A continuing to work or request permission. Returning
to A through the picker need not produce a backend selection event.

Tracking live conversations requires preserving independent session identities.
Tracking which conversation a particular TUI is displaying additionally requires
a TUI integration with client/terminal attribution. Even complete navigation
tracking would not make it safe to retire another live conversation.

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
- [`src/normalize.rs`](../../src/normalize.rs) extracts `tool_name`,
  `notification_type`, and whether `background_tasks[]` still names live child work; it does
  not combine them with the event name.
- Child-agent phase arithmetic is canonical in [`src/children.rs`](../../src/children.rs);
  [`src/lifecycle.rs`](../../src/lifecycle.rs) classifies each event as a root or child claim,
  and [`src/state.rs`](../../src/state.rs) routes a child claim onto its root's record. Pi and
  OpenCode instead resolve child/async-run activity inside their own adapters.
- Droid command hooks can overlap. Pi avoids the same race by serializing its
  adapter calls through one promise queue.
- Generic tool completion is not strong enough to clear an unrelated waiting
  state. A robust transition needs the responsible tool call or stronger state
  precedence.

## Sources

- Verified Droid behavior: two local Droid 0.218.2 session transcript probes,
  inspected 2026-09-16
- Verified Droid `calling_session_id`/Task-child behavior, including grandchildren: a local
  Droid 0.218.2 session transcript capture, inspected 2026-10-06
- OpenCode adapter: [`adapters/opencode-hooklinesinker.ts`](../../adapters/opencode-hooklinesinker.ts)
- Pi adapter: [`adapters/pi-hooklinesinker.ts`](../../adapters/pi-hooklinesinker.ts)
- Factory Droid hook contract:
  <https://docs.factory.ai/harness/hooks.md>
- Status storage and evidence limits:
  [Status lifecycle](../status-lifecycle.md)

Devin CLI contract evidence (2026-11-01): [hook overview](https://docs.devin.ai/cli/extensibility/hooks/overview),
[lifecycle hooks](https://docs.devin.ai/cli/extensibility/hooks/lifecycle-hooks), and
[public release manifest](https://static.devin.ai/cli/3000.11.234/manifest.json).
The installer identifies the executable as `devin`; fixtures use native `session_id`,
`prompt_id`, and `tool_name`. The docs provide no complete child-hook contract, so child
aggregation remains unreported. Release metadata is version evidence, not a live hook test.

Goose CLI v1.57.0 source evidence (2026-11-01):
[hook schema and runner](https://github.com/block/goose/blob/30bba95105e59bc3e7df624cf6b4d433690cfe12/crates/goose/src/hooks/mod.rs),
[plugin discovery](https://github.com/block/goose/blob/30bba95105e59bc3e7df624cf6b4d433690cfe12/crates/goose/src/plugins/discovery.rs),
[path roots](https://github.com/block/goose/blob/30bba95105e59bc3e7df624cf6b4d433690cfe12/crates/goose/src/config/paths.rs),
and [desktop backend launch](https://github.com/block/goose/blob/30bba95105e59bc3e7df624cf6b4d433690cfe12/ui/desktop/src/main.ts).
`HookContext` supplies native `session_id`, `working_dir`, and optional `tool_name`/
`tool_call_id`. Tool failure is completion of one tool, never proof the turn settled.
`PreToolUseResult` reports policy allow/deny and is not a user-wait signal; it is not subscribed.
Only `Stop` settles the turn, and no compaction or complete child lifecycle is inferred.


Copilot CLI contract evidence (2026-11-01): [official hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference),
checked alongside the official 1.0.105 binary. PascalCase registrations receive `session_id`
and `tool_name`; lowercase `notification` receives `sessionId` and `notification_type`.
Canonicalization accepts both documented identity/tool spellings only for Copilot, before
normalization and lifecycle recording. Contradictory identity aliases are rejected.
`PermissionRequest` runs before the permission service and does not establish a visible wait.
`PreCompact` starts compaction; `Stop` settles the root; errors alone do not end its turn.
Known child `parent_agent_id` / `agent_type` fields (including camelCase equivalents) suppress
child status callbacks. An `agent_id` alone does not suppress a root. Child counts and complete
child activity coverage remain unavailable. `/clear` ends the old session independently of
its replacement; detached late end hooks remove only their named session.

Antigravity CLI is **provisional; support blocked**. The preparatory subscription is
`SessionStart` (idle), `PreInvocation` (working, not a turn), `PostToolUse` (refresh), and
`Stop` (boolean `fullyIdle` decides idle/working). `PreToolUse` and `PostInvocation` are not
subscribed. Missing/invalid fullyIdle is an ingest health error without a state transition.
The [official hook contract](https://antigravity.google/docs/hooks) was checked 2026-11-01;
1.1.11 and 1.4.12 isolated prompts were authentication-blocked. No real ownership or eventual
Stop(false) settlement contract is established. See the
[full limits](../hooks-and-adapters.md#antigravity-cli--provisional-support-blocked).


## Kilo CLI source contract

Verified against Kilo CLI **7.8.25**, commit
`b3b2f3d1d9460e3de0bcf528d757c0e8ac4cbb2c` (2026-11-01):

- [Generated SDK types](https://github.com/Kilo-Org/kilocode/blob/b3b2f3d1d9460e3de0bcf528d757c0e8ac4cbb2c/packages/sdk/js/src/v2/gen/types.gen.ts)
  define `parentID?: string`, `sessionID: string`, permission ask `id`, reply `requestID`, and
  status variants idle/busy/retry/offline/scheduled.
- [Plugin discovery](https://github.com/Kilo-Org/kilocode/blob/b3b2f3d1d9460e3de0bcf528d757c0e8ac4cbb2c/packages/opencode/src/config/plugin.ts)
  scans `"{plugin,plugins}/*.{ts,js}"`.
  [Paths](https://github.com/Kilo-Org/kilocode/blob/b3b2f3d1d9460e3de0bcf528d757c0e8ac4cbb2c/packages/opencode/src/config/paths.ts)
  include global Kilo config, `.kilocode`/`.kilo`, and `KILO_CONFIG_DIR`.
- [Runtime flags](https://github.com/Kilo-Org/kilocode/blob/b3b2f3d1d9460e3de0bcf528d757c0e8ac4cbb2c/packages/opencode/src/effect/runtime-flags.ts)
  use `Config.withDefault(true)` for `KILO_EXPERIMENTAL_BACKGROUND_SUBAGENTS`.
- [Scheduled status](https://github.com/Kilo-Org/kilocode/blob/b3b2f3d1d9460e3de0bcf528d757c0e8ac4cbb2c/packages/opencode/src/kilocode/session/scheduled.ts):
  “Precedence: permission > question > a live turn > a future wakeup > idle.”
  The status mapper includes `if (input.statusType === "offline") return "retry"`.
- [Compaction](https://github.com/Kilo-Org/kilocode/blob/b3b2f3d1d9460e3de0bcf528d757c0e8ac4cbb2c/packages/opencode/src/session/compaction.ts)
  executes `yield* prune({ sessionID: input.sessionID, reason: "post-compaction" })` before
  `yield* events.publish(Event.Compacted, { sessionID: input.sessionID })`.
- [Plugin loader](https://github.com/Kilo-Org/kilocode/blob/b3b2f3d1d9460e3de0bcf528d757c0e8ac4cbb2c/packages/opencode/src/plugin/index.ts)
  uses `const plugins = flags.pure ? [] : (cfg.plugin_origins ?? [])` and awaits
  `Promise.resolve(hook.dispose?.())` during finalization. Legacy function exports are accepted.

| Native event | Normalized effect |
| --- | --- |
| `session.created`, `session.idle`, idle status | Idle, subject to tree precedence |
| busy, retry, offline status | Working, subject to permission precedence |
| scheduled status | Idle, subject to tree precedence |
| `permission.asked` / `permission.replied` | Track ask/reply and recompute root phase |
| `session.compacted` | Annotate completed compaction, preserve phase |
| `tui.session.select` | Select root, preserve existing phase |
| `session.deleted`, instance disposal, plugin disposal | Remove affected roots |

The shared adapter fixtures and installed artifacts are tested. The isolated live prompt
completed using `openrouter/openai/gpt-4.1-mini`, with native plugin discovery, Kilo process
attribution and disposal removal observed. The default model first returned an endpoint/tool-
compatibility error. Real background-child settlement remains unverified live.
