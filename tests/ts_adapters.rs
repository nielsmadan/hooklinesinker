//! Behavioural coverage for the bundled TypeScript adapters in `adapters/`.
//!
//! `hooks install` writes these verbatim (bar the binary path), so the events an agent produces
//! are decided entirely by this TypeScript — Rust never sees the difference between a correct
//! adapter and one that ingests the wrong event. `tests/adapters_harness.mjs` runs each adapter
//! against a stubbed host and a fake binary that records what it was asked to ingest.
//!
//! Skips with a message when node is missing or too old for type stripping, so `cargo test`
//! stays green on a machine without node.

use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// `--experimental-strip-types` landed in 22.6; the adapters are `.ts`.
const MIN_NODE: (u32, u32) = (22, 6);

// Both adapters set HOOK_TIMEOUT_MS to this; a regression in either shows up as a hang.
const ADAPTER_HOOK_TIMEOUT_MS: u64 = 2_000;
// The adapter's own timeout plus process spawn and kill on a loaded CI box.
const HANG_BUDGET_MS: u64 = ADAPTER_HOOK_TIMEOUT_MS + 3_000;

const PI_ADAPTER: &str = "adapters/pi-hooklinesinker.ts";
const OPENCODE_ADAPTER: &str = "adapters/opencode-hooklinesinker.ts";

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "hooklinesinker-ts-{label}-{}-{nanos}-{id}",
        std::process::id()
    ))
}

fn usable_node() -> Option<String> {
    let Ok(output) = Command::new("node").arg("--version").output() else {
        eprintln!(
            "SKIP: node is not on PATH; TypeScript adapter tests need node {}.{}+",
            MIN_NODE.0, MIN_NODE.1
        );
        return None;
    };
    if !output.status.success() {
        eprintln!("SKIP: `node --version` failed; TypeScript adapter tests need node");
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let mut parts = version.trim_start_matches('v').split('.');
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    if (major, minor) < MIN_NODE {
        eprintln!(
            "SKIP: node {version} cannot strip TypeScript types; adapter tests need {}.{}+",
            MIN_NODE.0, MIN_NODE.1
        );
        return None;
    }
    Some(version)
}

struct Run {
    invocations: Vec<Value>,
    running_pids: Vec<u64>,
    registered_channels: Vec<String>,
    subscribed_channels: Vec<String>,
    elapsed_ms: u64,
}

impl Run {
    fn events(&self) -> Vec<String> {
        self.invocations
            .iter()
            .map(|invocation| {
                invocation["args"]
                    .as_array()
                    .and_then(|args| args.last())
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    fn agents(&self) -> Vec<String> {
        self.invocations
            .iter()
            .map(|invocation| {
                invocation["args"][2]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    fn stdin(&self, index: usize) -> &Value {
        &self.invocations[index]["stdin"]
    }

    fn session_ids(&self) -> Vec<String> {
        self.invocations
            .iter()
            .map(|invocation| {
                invocation["stdin"]["session_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }
}

/// The whole suite runs in a single node process, once, behind a `OnceLock`: cargo's parallel
/// test threads would otherwise each boot node, and the resulting contention trips the
/// adapters' own 2s hook timeout — dropping events for reasons unrelated to the behaviour
/// under test.
static REPORT: OnceLock<Option<HashMap<String, Run>>> = OnceLock::new();

fn report() -> Option<&'static HashMap<String, Run>> {
    REPORT.get_or_init(run_harness).as_ref()
}

fn run(scenario: &str) -> Option<&'static Run> {
    let report = report()?;
    Some(
        report
            .get(scenario)
            .unwrap_or_else(|| panic!("harness produced no result for {scenario}")),
    )
}

fn run_harness() -> Option<HashMap<String, Run>> {
    usable_node()?;

    let root = repo_root();
    let workdir = unique_temp_dir("adapters");
    let output = Command::new("node")
        .arg("--experimental-strip-types")
        .arg(root.join("tests/adapters_harness.mjs"))
        .arg(&root)
        .arg(&workdir)
        .env("NODE_NO_WARNINGS", "1")
        .output()
        .expect("failed to launch node");
    let _ = std::fs::remove_dir_all(&workdir);

    assert!(
        output.status.success(),
        "adapter harness failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "adapter harness output was not JSON ({e}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    let scenarios = value.as_object().expect("harness report is not an object");
    Some(
        scenarios
            .iter()
            .map(|(scenario, result)| {
                (
                    scenario.clone(),
                    Run {
                        invocations: result["invocations"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default(),
                        running_pids: result["runningPids"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_u64)
                            .collect(),
                        registered_channels: string_list(&result["registeredChannels"]),
                        subscribed_channels: string_list(&result["subscribedChannels"]),
                        elapsed_ms: result["elapsedMs"].as_u64().unwrap_or_default(),
                    },
                )
            })
            .collect(),
    )
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn adapters_carry_the_binary_placeholder() {
    for asset in [PI_ADAPTER, OPENCODE_ADAPTER] {
        let source = std::fs::read_to_string(repo_root().join(asset)).unwrap();
        assert!(
            source.contains("__HOOKLINESINKER_BIN__"),
            "{asset} lost the placeholder the installer rewrites"
        );
    }
}

fn run_hook_body(source: &str) -> String {
    let start = source
        .find("function runHook(")
        .expect("adapter defines runHook");
    let rest = &source[start..];
    let end = rest
        .find("\n}\n")
        .expect("runHook body is brace-terminated");
    rest[..end].to_string()
}

fn shared_declaration(source: &str, prefix: &str) -> String {
    source
        .lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("adapter declares {prefix}"))
        .to_string()
}

#[test]
fn both_adapters_share_one_hook_budget() {
    let opencode = std::fs::read_to_string(repo_root().join(OPENCODE_ADAPTER)).unwrap();
    let pi = std::fs::read_to_string(repo_root().join(PI_ADAPTER)).unwrap();
    for prefix in [
        "const HOOK_TIMEOUT_MS",
        "const MAX_PENDING_HOOKS",
        "function isRecord",
    ] {
        assert_eq!(
            shared_declaration(&opencode, prefix),
            shared_declaration(&pi, prefix),
            "{prefix} drifted between the two adapters"
        );
    }
    assert_eq!(
        shared_declaration(&opencode, "const HOOK_TIMEOUT_MS"),
        format!("const HOOK_TIMEOUT_MS = {ADAPTER_HOOK_TIMEOUT_MS};"),
        "the adapters' hook budget no longer matches this suite's hang budget"
    );
}

#[test]
fn both_adapters_share_one_run_hook_implementation() {
    // The two files cannot import a common module, so drift is only caught by comparing them.
    let opencode =
        run_hook_body(&std::fs::read_to_string(repo_root().join(OPENCODE_ADAPTER)).unwrap());
    let pi = run_hook_body(&std::fs::read_to_string(repo_root().join(PI_ADAPTER)).unwrap());

    let differing = opencode
        .lines()
        .zip(pi.lines())
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(
        opencode.lines().count(),
        pi.lines().count(),
        "runHook bodies differ in length; they must stay line-for-line parallel"
    );
    assert_eq!(
        differing, 3,
        "runHook bodies differ on {differing} lines; only the agent name and the third \
         parameter may differ, so reconcile them or update this guard deliberately"
    );

    let translated = opencode
        .replace("\"opencode\"", "\"pi\"")
        .replace("  cwd,", "  reason,")
        .replace(
            "if (cwd) native.cwd = cwd;",
            "if (reason) native.reason = reason;",
        );
    assert_eq!(
        translated, pi,
        "runHook bodies diverge beyond the agent name and the third parameter"
    );
}

// MARK: - Pi

#[test]
fn pi_session_starts_forward_the_native_switch_reason() {
    let Some(run) = run("pi:session_switches") else {
        return;
    };
    assert_eq!(
        run.events(),
        [
            "session_start",
            "session_start",
            "agent_start",
            "session_start",
            "agent_start",
            "session_start",
            "agent_start",
            "session_start",
            "agent_start"
        ]
    );
    assert_eq!(
        run.stdin(0),
        &serde_json::json!({"session_id": "pi-parent", "reason": "startup"})
    );
    for (index, reason) in ["new", "resume", "fork", "reload"].iter().enumerate() {
        let id = format!("pi-{reason}");
        assert_eq!(
            run.stdin(index * 2 + 1),
            &serde_json::json!({"session_id": id, "reason": reason})
        );
        assert_eq!(
            run.stdin(index * 2 + 2),
            &serde_json::json!({"session_id": id})
        );
    }
}

#[test]
fn pi_session_start_accepts_hosts_without_a_reason() {
    let Some(run) = run("pi:start_without_reason") else {
        return;
    };
    assert_eq!(run.events(), ["session_start"]);
    assert_eq!(
        run.stdin(0),
        &serde_json::json!({"session_id": "pi-parent"})
    );
}

#[test]
fn pi_prompt_and_decision_produce_the_permission_lifecycle() {
    let Some(run) = run("pi:permission_lifecycle") else {
        return;
    };
    assert_eq!(
        run.events(),
        ["session_start", "permission_prompt", "permission_resolved"]
    );
    assert!(run.agents().iter().all(|agent| agent == "pi"));
    assert_eq!(run.stdin(0)["session_id"], "pi-parent");
    assert_eq!(
        run.registered_channels,
        [
            "permissions:ui_prompt",
            "permissions:decision",
            "subagent:async-started",
            "subagent:async-complete"
        ]
    );
    assert_eq!(
        run.subscribed_channels,
        [
            "permissions:ui_prompt",
            "permissions:decision",
            "subagent:async-started",
            "subagent:async-complete"
        ]
    );
}

#[test]
fn pi_permission_lifecycle_survives_session_switches() {
    let Some(run) = run("pi:permission_after_switch") else {
        return;
    };
    assert_eq!(
        run.events(),
        [
            "session_start",
            "session_start",
            "permission_prompt",
            "permission_resolved"
        ]
    );
    assert_eq!(run.stdin(2)["session_id"], "pi-resumed");
}

#[test]
fn pi_handles_malformed_permission_payloads() {
    let Some(run) = run("pi:malformed_permission_payloads") else {
        return;
    };
    assert_eq!(
        run.events(),
        ["session_start", "permission_prompt", "permission_resolved"]
    );
}

#[test]
fn pi_ignores_silent_and_orphan_permission_decisions() {
    let Some(run) = run("pi:silent_and_orphan_decisions") else {
        return;
    };
    assert_eq!(
        run.events(),
        ["session_start", "permission_prompt", "permission_resolved"]
    );
}

#[test]
fn pi_settled_session_discards_pending_prompts() {
    let Some(run) = run("pi:settled_discards_prompts") else {
        return;
    };
    assert_eq!(
        run.events(),
        ["session_start", "permission_prompt", "agent_settled"]
    );
}

#[test]
fn pi_child_sessions_ingest_nothing() {
    let Some(run) = run("pi:child_session_silent") else {
        return;
    };
    assert!(
        run.events().is_empty(),
        "a headless child session ingested {:?}",
        run.events()
    );
}

/// The `event?.reason === "quit"` gate: `new`/`resume`/`reload`/`fork` keep the same terminal
/// session and are followed by a `session_start`, so removing the row on those would drop a
/// live session out of the UI.
#[test]
fn pi_only_a_quit_shutdown_removes_the_session() {
    let Some(quit) = run("pi:shutdown_quit") else {
        return;
    };
    assert_eq!(quit.events(), ["session_start", "session_shutdown"]);

    for scenario in [
        "pi:shutdown_reload",
        "pi:shutdown_new",
        "pi:shutdown_resume",
        "pi:shutdown_fork",
    ] {
        let run = run(scenario).expect("node was available a moment ago");
        assert_eq!(
            run.events(),
            ["session_start"],
            "{scenario} ingested a shutdown"
        );
    }
}

#[test]
fn pi_a_hanging_binary_cannot_block_the_adapter() {
    let Some(run) = run("pi:hanging_binary:hang") else {
        return;
    };
    // The fake binary records and then blocks forever; only the adapter's own timeout and kill
    // can end the call, and the harness watchdog fails the run if they don't.
    assert_eq!(run.events(), ["session_start"]);
    assert!(
        run.elapsed_ms < HANG_BUDGET_MS,
        "adapter took {}ms to give up on a hanging binary, budget is {HANG_BUDGET_MS}ms",
        run.elapsed_ms
    );
    assert_eq!(run.running_pids, [] as [u64; 0]);
}

#[test]
fn pi_adapter_admits_exactly_its_queue_bound() {
    // A fast binary lets every admitted hook finish, so the cap is the only thing that can
    // reduce 100 fired events to a smaller number.
    let Some(run) = run("pi:queue_bound") else {
        return;
    };
    let prompts = run
        .events()
        .iter()
        .filter(|event| event.as_str() == "permission_prompt")
        .count();
    assert_eq!(prompts, 32);
}

#[test]
fn pi_adapter_drops_queued_hooks_past_their_deadline() {
    let Some(run) = run("pi:queue_bound:slow") else {
        return;
    };
    let prompts = run
        .events()
        .iter()
        .filter(|event| event.as_str() == "permission_prompt")
        .count();
    // Timing decides how many of the 32 admitted hooks outlive their enqueue deadline, so
    // only the cap and the back-pressure itself are deterministic here.
    assert!(prompts <= 32, "queue bound exceeded: {prompts}");
    assert!(prompts >= 1, "every queued hook was dropped");
}

#[test]
fn pi_async_runs_hold_working_until_the_woken_parent_settles() {
    let Some(run) = run("pi:async_runs_hold_working_until_the_woken_parent_settles") else {
        return;
    };
    assert_eq!(
        run.events(),
        [
            "session_start",
            "agent_start",
            "agent_start",
            "agent_start",
            "agent_settled"
        ]
    );
}

#[test]
fn pi_async_run_completing_without_a_wake_settles() {
    let Some(run) = run("pi:async_run_completing_without_a_wake_settles") else {
        return;
    };
    assert_eq!(
        run.events(),
        [
            "session_start",
            "agent_start",
            "agent_start",
            "agent_settled"
        ]
    );
}

#[test]
fn pi_dead_async_runs_do_not_hold_working() {
    let Some(run) = run("pi:dead_async_runs_do_not_hold_working") else {
        return;
    };
    assert_eq!(run.events(), ["session_start", "agent_settled"]);
}

#[test]
fn pi_async_runs_of_other_sessions_are_ignored() {
    let Some(run) = run("pi:async_runs_of_other_sessions_are_ignored") else {
        return;
    };
    assert_eq!(run.events(), ["session_start", "agent_settled"]);
}

#[test]
fn pi_async_run_finishing_mid_turn_settles_normally() {
    let Some(run) = run("pi:async_run_finishing_mid_turn_settles_normally") else {
        return;
    };
    assert_eq!(
        run.events(),
        ["session_start", "agent_start", "agent_settled"]
    );
}

#[test]
fn pi_async_runs_rebuilt_on_start() {
    let Some(run) = run("pi:async_runs_rebuilt_on_start") else {
        return;
    };
    assert_eq!(
        run.events(),
        [
            "session_start",
            "agent_start",
            "agent_start",
            "agent_settled"
        ]
    );
}

#[test]
fn pi_scan_excludes_dead_pids_before_any_settle() {
    let Some(run) = run("pi:scan_excludes_dead_pids_before_any_settle") else {
        return;
    };
    assert_eq!(run.events(), ["session_start", "agent_settled"]);
}

#[test]
fn pi_completion_during_rebuild_is_not_resurrected() {
    let Some(run) = run("pi:completion_during_rebuild_is_not_resurrected") else {
        return;
    };
    assert_eq!(
        run.events(),
        ["session_start", "agent_settled", "agent_settled"]
    );
}

#[test]
fn pi_shutdown_forgets_async_runs() {
    let Some(run) = run("pi:shutdown_forgets_async_runs") else {
        return;
    };
    assert_eq!(run.events(), ["session_start", "session_shutdown"]);
}

#[test]
fn pi_scan_skips_runs_owned_by_another_process() {
    let Some(run) = run("pi:scan_skips_runs_owned_by_another_process") else {
        return;
    };
    assert_eq!(
        run.events(),
        [
            "session_start",
            "agent_start",
            "agent_start",
            "agent_settled",
            "session_before_compact"
        ]
    );
}

#[test]
fn pi_completion_without_a_wake_mid_turn_settles_once() {
    let Some(run) = run("pi:completion_without_a_wake_mid_turn_settles_once") else {
        return;
    };
    assert_eq!(
        run.events(),
        ["session_start", "agent_start", "agent_settled"]
    );
}

#[test]
fn pi_one_of_two_runs_completing_keeps_the_hold() {
    let Some(run) = run("pi:one_of_two_runs_completing_keeps_the_hold") else {
        return;
    };
    assert_eq!(
        run.events(),
        [
            "session_start",
            "agent_start",
            "agent_start",
            "permission_prompt",
            "agent_settled",
            "session_before_compact"
        ]
    );
}

#[test]
fn pi_unknown_completion_keeps_the_hold() {
    let Some(run) = run("pi:unknown_completion_keeps_the_hold") else {
        return;
    };
    assert_eq!(
        run.events(),
        [
            "session_start",
            "agent_start",
            "agent_start",
            "permission_prompt",
            "agent_start",
            "agent_settled"
        ]
    );
}

#[test]
fn pi_manual_compaction_with_live_runs_holds_working() {
    let Some(run) = run("pi:manual_compaction_with_live_runs_holds_working") else {
        return;
    };
    assert_eq!(
        run.events(),
        ["session_start", "agent_start", "agent_start"]
    );
}

// MARK: - OpenCode

#[test]
fn opencode_selection_and_status_hooks_keep_arrival_order() {
    let Some(run) = run("opencode:selection_and_status_are_serialized") else {
        return;
    };
    assert_eq!(
        run.events(),
        [
            "session.created",
            "tui.session.select",
            "session.status.busy",
            "tui.session.select",
            "session.deleted"
        ]
    );
    for (index, id) in ["a", "a", "b", "a"].iter().enumerate() {
        assert_eq!(
            run.stdin(index + 1),
            &serde_json::json!({"session_id": id, "cwd": "/work/repo"})
        );
    }
}

#[test]
fn opencode_instance_disposal_removes_each_known_live_session_once() {
    let Some(run) = run("opencode:instance_disposal_removes_known_sessions") else {
        return;
    };
    assert_eq!(
        run.events(),
        [
            "session.created",
            "session.idle",
            "session.idle",
            "session.deleted",
            "session.deleted"
        ]
    );
    assert_eq!(run.stdin(3)["session_id"], "a");
    assert_eq!(run.stdin(4)["session_id"], "b");
}

#[test]
fn opencode_plugin_load_ingests_session_created_with_the_directory() {
    let Some(run) = run("opencode:load_posts_created") else {
        return;
    };
    assert_eq!(run.events(), ["session.created"]);
    assert_eq!(run.agents(), ["opencode"]);
    assert_eq!(run.stdin(0)["cwd"], "/work/repo");
}

#[test]
fn opencode_status_events_carry_their_status_suffix() {
    let Some(run) = run("opencode:status_becomes_event_suffix") else {
        return;
    };
    assert_eq!(run.events(), ["session.created", "session.status.busy"]);
    assert_eq!(run.stdin(1)["session_id"], "oc-1");
    assert_eq!(run.stdin(1)["cwd"], "/work/repo");
}

#[test]
fn opencode_untracked_and_typeless_events_are_dropped() {
    for scenario in [
        "opencode:untracked_event_is_dropped",
        "opencode:status_without_type_is_dropped",
        "opencode:malformed_events_are_dropped",
    ] {
        let Some(run) = run(scenario) else {
            return;
        };
        assert_eq!(
            run.events(),
            ["session.created"],
            "{scenario} ingested more than the load-time event"
        );
    }
}

#[test]
fn opencode_session_ids_use_the_first_valid_string() {
    let Some(run) = run("opencode:session_id_fallbacks") else {
        return;
    };
    assert_eq!(
        run.events(),
        [
            "session.created",
            "session.created",
            "session.idle",
            "session.deleted",
            "session.idle"
        ]
    );
    for (index, id) in ["from-info", "from-snake", "from-camel"].iter().enumerate() {
        assert_eq!(run.stdin(index + 1)["session_id"], *id);
    }
    assert_eq!(run.stdin(4), &serde_json::json!({ "cwd": "/work/repo" }));
}

#[test]
fn opencode_child_sessions_report_through_the_root() {
    let Some(run) = run("opencode:child_sessions_report_through_the_root") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.status.busy",
            "session.status.idle",
            "session.status.busy",
            "session.status.idle"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R", "R", "R"]);
}

#[test]
fn opencode_background_child_sessions_hold_the_root_until_its_own_idle() {
    let Some(run) = run("opencode:child_sessions_report_through_the_root:background") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.status.busy",
            "session.status.busy",
            "session.status.idle"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R", "R"]);
}

#[test]
fn opencode_child_permission_surfaces_on_the_root() {
    let Some(run) = run("opencode:child_permission_surfaces_on_the_root") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "permission.asked",
            "session.status.busy"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R"]);
}

#[test]
fn opencode_root_permission_reply_resumes_working() {
    let Some(run) = run("opencode:root_permission_reply_resumes_working") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "permission.asked",
            "session.status.busy"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R"]);
}

#[test]
fn opencode_grandchildren_resolve_to_the_root() {
    let Some(run) = run("opencode:grandchildren_resolve_to_the_root") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        ["session.created", "session.status.busy"]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R"]);
}

#[test]
fn opencode_child_learned_late_loses_its_row() {
    let Some(run) = run("opencode:child_learned_late_loses_its_row") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.status.busy",
            "session.deleted"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "X", "R", "X"]);
}

#[test]
fn opencode_child_learned_late_through_an_update_loses_its_row() {
    let Some(run) = run("opencode:child_learned_late_through_an_update") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.status.busy",
            "session.deleted"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "X", "R", "X"]);
}

#[test]
fn opencode_late_link_under_a_busy_root_reclaims_the_row_first() {
    let Some(run) = run("opencode:late_link_under_a_busy_root_reclaims_the_row_first") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.status.busy",
            "session.status.busy",
            "session.deleted"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "X", "R", "X"]);
}

#[test]
fn opencode_child_selection_selects_the_root() {
    let Some(run) = run("opencode:child_selection_selects_the_root") else {
        return;
    };
    assert_eq!(run.events()[1..], ["session.created", "tui.session.select"]);
    assert_eq!(run.session_ids()[1..], ["R", "R"]);
}

#[test]
fn opencode_root_busy_under_a_pending_child_ask_stays_permission() {
    let Some(run) = run("opencode:root_busy_under_a_pending_child_ask_stays_permission") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        ["session.created", "permission.asked", "permission.asked"]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R"]);
}

#[test]
fn opencode_each_pending_ask_is_tracked() {
    let Some(run) = run("opencode:each_pending_ask_is_tracked") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "permission.asked",
            "tui.session.select",
            "session.status.busy"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R", "R"]);
}

#[test]
fn opencode_child_idle_clears_its_asks() {
    let Some(run) = run("opencode:child_idle_clears_its_asks") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "permission.asked",
            "session.status.idle"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R"]);
}

#[test]
fn opencode_deleting_a_busy_child_releases_its_root() {
    let Some(run) = run("opencode:deleting_a_busy_child_releases_its_root") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.status.idle"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R"]);
}

#[test]
fn opencode_disposal_removes_only_roots() {
    let Some(run) = run("opencode:disposal_removes_only_roots") else {
        return;
    };
    assert_eq!(run.events()[1..], ["session.created", "session.deleted"]);
    assert_eq!(run.session_ids()[1..], ["R", "R"]);
}

#[test]
fn opencode_child_compaction_keeps_the_root_phase() {
    let Some(run) = run("opencode:child_compaction_keeps_the_root_phase") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.status.idle",
            "session.status.busy",
            "session.status.idle"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R", "R"]);
}

#[test]
fn opencode_background_child_compaction_holds_the_root_until_its_own_idle() {
    let Some(run) = run("opencode:child_compaction_keeps_the_root_phase:background") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.status.busy",
            "session.status.idle"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R"]);
}

#[test]
fn opencode_root_deletion_forgets_its_descendants() {
    let Some(run) = run("opencode:root_deletion_forgets_its_descendants") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.deleted",
            "session.created",
            "session.status.idle"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R", "R"]);
}

#[test]
fn opencode_sessionless_tree_events_are_dropped() {
    let Some(run) = run("opencode:sessionless_tree_events_are_dropped") else {
        return;
    };
    assert_eq!(run.events(), ["session.created"]);
}

#[test]
fn opencode_overlapping_child_events_settle_on_the_tree_phase() {
    let Some(run) = run("opencode:overlapping_child_events_settle_on_the_tree_phase") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "permission.asked",
            "session.status.busy",
            "permission.asked",
            "session.status.busy",
            "permission.asked"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R", "R", "R", "R"]);
}

#[test]
fn opencode_background_child_completion_holds_the_root_until_its_wake() {
    let Some(run) =
        run("opencode:background_child_completion_holds_the_root_until_its_wake:background")
    else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.status.busy",
            "session.status.busy",
            "session.status.idle"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R", "R"]);
}

#[test]
fn opencode_experimental_flag_also_enables_the_background_hold() {
    let Some(run) =
        run("opencode:background_child_completion_holds_the_root_until_its_wake:experimental")
    else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.status.busy",
            "session.status.busy",
            "session.status.idle"
        ]
    );
}

#[test]
fn opencode_the_roots_own_event_releases_its_hold() {
    let Some(run) = run("opencode:the_roots_own_event_releases_its_hold:background") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.status.busy",
            "session.status.busy",
            "session.status.idle"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R", "R"]);
}

#[test]
fn opencode_deleting_a_child_keeps_a_held_root_held() {
    let Some(run) = run("opencode:deleting_a_child_keeps_a_held_root_held:background") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.status.busy",
            "session.status.idle"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R"]);
}

#[test]
fn opencode_child_permission_interrupts_a_hold() {
    let Some(run) = run("opencode:child_permission_interrupts_a_hold:background") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "permission.asked",
            "session.status.busy",
            "session.status.busy",
            "session.status.idle"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R", "R", "R"]);
}

#[test]
fn opencode_the_roots_own_idle_and_error_release_its_hold() {
    let Some(run) = run("opencode:the_roots_own_idle_and_error_release_its_hold:background") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.idle",
            "session.status.busy",
            "session.error"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R", "R", "R"]);
}

#[test]
fn opencode_root_deletion_drops_its_hold() {
    let Some(run) = run("opencode:root_deletion_drops_its_hold:background") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.deleted",
            "session.created",
            "session.status.busy",
            "session.status.idle",
            "session.status.idle",
            "session.deleted"
        ]
    );
    assert_eq!(
        run.session_ids()[1..],
        ["R", "R", "R", "R", "X", "X", "R", "X"]
    );
}

#[test]
fn opencode_late_link_under_a_held_root_keeps_it_working() {
    let Some(run) = run("opencode:late_link_under_a_held_root_keeps_it_working:background") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.status.busy",
            "session.status.idle",
            "session.status.busy",
            "session.deleted",
            "session.status.busy",
            "session.status.idle"
        ]
    );
    assert_eq!(
        run.session_ids()[1..],
        ["R", "R", "X", "X", "R", "X", "R", "R"]
    );
}

#[test]
fn opencode_foreground_child_completion_under_a_busy_root() {
    let Some(run) = run("opencode:foreground_child_completion_under_a_busy_root") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        [
            "session.created",
            "session.status.busy",
            "session.status.idle"
        ]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R", "R"]);
}

#[test]
fn opencode_phase_dropped_by_the_queue_bound_is_resent() {
    let Some(run) = run("opencode:phase_dropped_by_the_queue_bound_is_resent") else {
        return;
    };
    let events = run.events();
    assert_eq!(events.len(), 35);
    assert_eq!(events[1], "session.created");
    assert!(events[2..34].iter().all(|event| event == "session.idle"));
    assert_eq!(events[34], "session.status.busy");
    assert_eq!(run.session_ids()[34], "R");
}

#[test]
fn opencode_phase_dropped_by_the_deadline_is_resent() {
    let Some(run) = run("opencode:phase_dropped_by_the_deadline_is_resent:stubborn") else {
        return;
    };
    assert_eq!(
        run.events()[1..],
        ["session.created", "session.status.busy"]
    );
    assert_eq!(run.session_ids()[1..], ["R", "R"]);
}

#[test]
fn opencode_a_hanging_binary_cannot_block_the_adapter() {
    let Some(run) = run("opencode:hanging_binary:hang") else {
        return;
    };
    assert_eq!(run.events(), ["session.created"]);
    assert!(
        run.elapsed_ms < HANG_BUDGET_MS,
        "adapter took {}ms to give up on a hanging binary, budget is {HANG_BUDGET_MS}ms",
        run.elapsed_ms
    );
    assert_eq!(run.running_pids, [] as [u64; 0]);
}

#[test]
fn opencode_adapter_admits_exactly_its_queue_bound() {
    let Some(run) = run("opencode:queue_bound") else {
        return;
    };
    let idle = run
        .events()
        .iter()
        .filter(|event| event.as_str() == "session.idle")
        .count();
    assert_eq!(idle, 32);
}

#[test]
fn opencode_adapter_drops_queued_hooks_past_their_deadline() {
    let Some(run) = run("opencode:queue_bound:slow") else {
        return;
    };
    let idle = run
        .events()
        .iter()
        .filter(|event| event.as_str() == "session.idle")
        .count();
    assert!(idle <= 32, "queue bound exceeded: {idle}");
    assert!(idle >= 1, "every queued hook was dropped");
}

/// Prevent stale asset paths from silently skipping adapter assertions.
#[test]
fn harness_and_adapters_exist() {
    for path in ["tests/adapters_harness.mjs", PI_ADAPTER, OPENCODE_ADAPTER] {
        let full: &Path = &repo_root().join(path);
        assert!(full.exists(), "missing {}", full.display());
    }
}
