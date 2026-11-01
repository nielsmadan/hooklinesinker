# Native CLI validation

Eleven agents are supported; Antigravity is a twelfth configuration profile with
provisional lifecycle support. Configuration status reports owned artifact integrity,
not host discovery, trust, authentication, or successful hook delivery.

The five additions were checked on 2026-11-01 against these versions. Versions are
evidence pins, not minimum-version promises.

| CLI | Contract pin | Native observation | Remaining native checks |
|---|---|---|---|
| Devin | 3000.11.234, build `92c83011d113`; bundled hook documentation | Version/help and isolated auth status; prompt stopped at login | Discovery, inherited Claude suppression, tool/permission/stop/end delivery and owner death |
| Goose | 1.57.0; source `30bba95105e59bc3e7df624cf6b4d433690cfe12` | Version/help; prompt stopped because no provider was configured | Discovery and complete turn/tool/failure/session-end delivery; desktop remains outside scope |
| GitHub Copilot | 1.0.105; official hook reference | Version/help; prompt stopped during account-validation network request, including after public CA retry | Discovery, sandbox access, inherited Claude suppression, permission notifications, `/clear`, and crash/timeout handling |
| Antigravity | 1.4.12 (`agy` 1.1.11 also inspected); official hook reference | Generated named hooks discovered; model execution blocked at authentication | Actual CLI owner and backend lifetime, eventual settlement after `Stop(false)`, callback/neutral response handling and timeout behavior |
| Kilo | 7.8.25; source `b3b2f3d1d9460e3de0bcf528d757c0e8ac4cbb2c` | Generated standalone plugin loaded; explicit-model turn succeeded; actual Kilo PID and native disposal removal observed | Background children/wake settlement, overlapping permissions, compaction and long-lived shared backend teardown |

The Kilo turn used `openrouter/openai/gpt-4.1-mini`. Its default provider selection
failed before that successful explicit-model run. The Antigravity loader observation
does not establish lifecycle support. In a separate helper-only probe, a held status
lock delayed neutral `Stop` output by 10.518 seconds, exceeding the registered
five-second deadline; SIGTERM produced no neutral output. Host handling of these
conditions is unverified. See [the Antigravity support gates](../hooks-and-adapters.md#antigravity-cli--provisional-support-blocked).

## What fixtures establish

The built CLI was exercised for all twelve profiles: missing/install/status,
byte-preserving reinstallation, uninstall, two-consumer retention, and final-consumer
removal. Controlled processes exercised normalized lifecycle, PID attribution, polling
and loopback-sink parity, malformed input recovery, delayed sinks, and owner death.
Those process fixtures are not native-agent executions. The native command profiles
ran their installed shell commands; OpenCode, Kilo and Pi also have installed-artifact
adapter harness coverage for host callbacks, queues, permissions and disposal.

Ownership fixtures cover foreign-handler preservation, unsupported namespaces and
execution overrides, duplicate/timeout drift, and repair. The actual pre-feature
1.3.0 binary was used to install the existing seven profiles before refreshing with
the expanded build. Both builds report 1.3.0, so a healthy equal-version activation
correctly reused the older binary; an isolated missing-binary repair activated the
new build before the twelve-profile refresh. This is not evidence of a newer-version
promotion; use the [two-version upgrade procedure](../tests/shared-upgrade/README.md)
when release binaries are available.

Existing-seven native agents were not prompted again for this change. Their coverage
is the CLI regression checks and the existing adapter/lifecycle suites. No fixture
or failed authentication attempt is counted as native delivery evidence.

## Repeating a native check

Use the [isolated smoke procedure](../tests/native-agent-smoke/README.md). It supplies
complete commands and environment isolation without depending on retained scratch
scripts or copying credential stores. A blocked check stays blocked until its stated
prerequisite exists; it does not justify changing the user's active configuration.

Protocol remains 1. Older Rust SDKs have a closed `Agent` enum: upgrade before typed
decoding of `devin`, `goose`, `copilot`, `kilo`, or `antigravity`, or skip unknown-agent
records before decoding. Exhaustive matches may need changes. See the
[wire compatibility contract](../../README.md#protocol-1).
