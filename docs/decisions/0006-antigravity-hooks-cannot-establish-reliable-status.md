# 0006 — Keep Antigravity unsupported until its hooks expose reliable lifecycle signals

**Status:** accepted
**Recorded:** 2026-11-15, following native CLI probes on 2026-11-13–14.

## Context

The investigation used `agy` 1.5.8 and 1.5.9, captured native hook payloads in print
and interactive modes, and compared them with the CLI's status line in a real terminal.
Getting callbacks into the ledger did not establish usable status:

- **Transcript identity:** 1.5.8 sent `transcript_full.jsonl`, while hls accepts the
  documented `transcript.jsonl`. A local fix admitted those events and demonstrated
  idle → working → idle for a simple turn, with hooks running under the actual CLI PID.
- **Idle settlement:** a parent emitted `Stop(fullyIdle:false)` while its child ran.
  The child's report woke the parent, which could emit another `Stop(false)` before
  the child's final `Stop(true)`. No later parent stop arrived. This happened in
  print and interactive captures; an interactive parent remained stale for 80 seconds
  until the probe deadline. The CLI's own status line reported the parent idle.
- **Child identity:** children emitted their own hooks and conversation IDs under
  the same CLI process, but no parent ID or explicit subagent marker. They appeared
  as separate root sessions. A `send_message` recipient identifies a destination,
  not a parent relationship, and is unavailable before the message is sent.
- **Cancellation:** upstream issue [#1348](https://github.com/google-antigravity/antigravity-cli/issues/1348)
  reported missing stop hooks on Esc/Ctrl+C. This was not reproduced locally: the
  attempted cancellation test completed its response. Cancellation and killed-child
  settlement therefore remained unverified.
- **Permission waits:** the real-terminal probe showed no hook marking the approval
  wait. The status line exposed it, but hooks alone left the session working.

These are findings from those versions and probes, not claims about future releases.
Related upstream reports found during the investigation were
[#928](https://github.com/google-antigravity/antigravity-cli/issues/928) (completion ordering),
[#1527](https://github.com/google-antigravity/antigravity-cli/issues/1527) (parent/root IDs),
[#922](https://github.com/google-antigravity/antigravity-cli/issues/922) (subagent marker), and
[#995](https://github.com/google-antigravity/antigravity-cli/issues/995) (awaiting input).

## Decision

Hls is hooks-only. Keep Antigravity unsupported because the observed hook contract
cannot reliably settle idle sessions and identify child sessions. Remove the unpushed
transcript-path fix `6a61a97`; admitting more callbacks does not repair that contract.
The previously published provisional configuration profile remains preparatory code,
not a supported integration.

Reject the investigated workarounds:

- Mapping every `Stop` to idle fixes the captured parent/child ordering race, but
  cannot settle a cancelled or killed conversation whose stop never arrives, or
  establish which sessions are children. It is insufficient evidence for support.
- Timing, empty prompts, and message recipients are not authoritative parentage.
  Do not infer child relationships or completion from them.
- A status-line command is outside the hooks-only boundary. It also reported only
  the foreground conversation, and remained `initializing` throughout one automated
  terminal run. Do not install or wrap `statusLine` as a fallback.
- The investigated language-server API required an address and CSRF token that
  hooks did not receive. Plugins exposed the same hooks without additional lifecycle
  signals; ACP covered only sessions launched through ACP. None supplied a hook-based
  solution for ordinary terminal sessions.

## Consequences

Do not reopen support just because authentication works, a transcript filename changes,
or a simple turn reaches idle. Reconsider when upstream hooks supply authoritative
parent/child identity and settlement signals. Validate normal completion, child completion
after the parent stops, nested children, cancellation, and killed children in the native CLI
before claiming support. Permission classification is secondary to correct idle status.

The existing [provisional implementation](../hooks-and-adapters.md#antigravity-cli--provisional-support-blocked)
still describes its current mappings and limits. Process death can retire its records;
it cannot repair an idle conversation while the shared CLI process remains alive.
