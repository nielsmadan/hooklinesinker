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

/// `Some(version)` when a usable node is on PATH, `None` (with a printed reason) otherwise.
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
    registered_channels: Vec<String>,
    subscribed_channels: Vec<String>,
    elapsed_ms: u64,
}

impl Run {
    /// The `--event` value of each fake-binary invocation, in order.
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

    /// The native JSON the adapter piped to the binary's stdin.
    fn stdin(&self, index: usize) -> &Value {
        &self.invocations[index]["stdin"]
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

// MARK: - Pi

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
        ["permissions:ui_prompt", "permissions:decision"]
    );
    // Shutdown unsubscribes both channels, so a later prompt cannot reach a dead session.
    assert!(run.subscribed_channels.is_empty());
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
    // The decision after agent_settled resolves nothing: settling already cleared the prompt.
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
        run.elapsed_ms < 9_000,
        "adapter took {}ms to give up on a hanging binary",
        run.elapsed_ms
    );
}

// MARK: - OpenCode

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
fn opencode_a_hanging_binary_cannot_block_the_adapter() {
    let Some(run) = run("opencode:hanging_binary:hang") else {
        return;
    };
    assert_eq!(run.events(), ["session.created"]);
    assert!(
        run.elapsed_ms < 9_000,
        "adapter took {}ms to give up on a hanging binary",
        run.elapsed_ms
    );
}

/// Guards the harness itself: a stale `Path` here would make every assertion above vacuous.
#[test]
fn harness_and_adapters_exist() {
    for path in ["tests/adapters_harness.mjs", PI_ADAPTER, OPENCODE_ADAPTER] {
        let full: &Path = &repo_root().join(path);
        assert!(full.exists(), "missing {}", full.display());
    }
}
