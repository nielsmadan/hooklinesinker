use hooklinesinker::consumers::{Consumer, ConsumerStore};
use hooklinesinker::environment::{self, EnvSource};
use hooklinesinker::hooks::{HookManager, HookRoots, HookState};
use hooklinesinker::install::{Candidate, Installer, SemVer};
use hooklinesinker::normalize::{HookEnvironment, normalize};
use hooklinesinker::processes::ProcessLookup;
use hooklinesinker::protocol::{
    Agent, PROTOCOL_VERSION, Phase, ProcessIdentity, SessionIdentity, StatusEvent,
};
use hooklinesinker::sinks::{HttpClient, SinkFanout};
use hooklinesinker::state::{self, StatusStore};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn temp_home() -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "hooklinesinker-test-{}-{nanos}-{id}",
        std::process::id()
    ))
}

fn store() -> StatusStore {
    StatusStore::open(temp_home()).unwrap()
}

fn consumer_store() -> ConsumerStore {
    ConsumerStore::open(temp_home()).unwrap()
}

fn consumer(name: &str, capabilities: &[&str], sink: Option<&str>) -> Consumer {
    Consumer {
        name: name.to_string(),
        protocol: PROTOCOL_VERSION,
        capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
        sink: sink.map(str::to_string),
    }
}

fn status_event() -> StatusEvent {
    status("fanout-session", 4200, 42)
}

type RecordedCall = (String, Vec<u8>);

#[derive(Clone, Default)]
struct RecordingHttpClient {
    calls: Arc<Mutex<Vec<RecordedCall>>>,
}

impl RecordingHttpClient {
    fn urls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(url, _)| url.clone())
            .collect()
    }

    fn bodies(&self) -> Vec<serde_json::Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, body)| serde_json::from_slice(body).unwrap())
            .collect()
    }
}

impl HttpClient for RecordingHttpClient {
    fn post_json(&self, url: &str, body: &[u8]) -> Result<u16, String> {
        self.calls
            .lock()
            .unwrap()
            .push((url.to_string(), body.to_vec()));
        Ok(200)
    }
}

struct FailingHttpClient;

impl HttpClient for FailingHttpClient {
    fn post_json(&self, _url: &str, _body: &[u8]) -> Result<u16, String> {
        Err("connection refused".to_string())
    }
}

fn ingest(
    ctx: &state::IngestContext,
    agent: Agent,
    event: &str,
    input: &str,
    hook_pid: u32,
) -> state::IngestOutcome {
    state::handle_ingest(ctx, agent, event, input, &test_environment(), hook_pid)
}

fn status(session_id: &str, pid: u32, started_at: u64) -> StatusEvent {
    StatusEvent {
        protocol: PROTOCOL_VERSION,
        binding_id: format!("{session_id}-{pid}-{started_at}"),
        agent: Agent::Claude,
        event: "PreToolUse".into(),
        phase: Phase::Working,
        running: true,
        observed_at: "2026-09-04T00:00:00Z".into(),
        session: SessionIdentity {
            id: session_id.into(),
            cwd: "/tmp".into(),
            transcript_path: None,
        },
        process: Some(ProcessIdentity {
            pid,
            started_at: started_at.to_string(),
            host: "test-host".into(),
        }),
        terminal: None,
        tmux: None,
        git: None,
        remote_host: None,
    }
}

struct AllAlive;

impl ProcessLookup for AllAlive {
    fn owner_of(&self, _hook_pid: u32, _agent: Agent) -> Option<ProcessIdentity> {
        None
    }

    fn is_alive(&self, _identity: &ProcessIdentity) -> bool {
        true
    }
}

fn all_alive() -> AllAlive {
    AllAlive
}

struct FakeProcessLookup {
    owner: Option<ProcessIdentity>,
    alive: Mutex<HashMap<u32, String>>,
}

impl FakeProcessLookup {
    fn new() -> Self {
        Self {
            owner: None,
            alive: Mutex::new(HashMap::new()),
        }
    }

    fn with_owner(identity: ProcessIdentity) -> Self {
        Self {
            owner: Some(identity),
            alive: Mutex::new(HashMap::new()),
        }
    }

    fn set_alive(&self, pid: u32, started_at: &str) {
        self.alive
            .lock()
            .unwrap()
            .insert(pid, started_at.to_string());
    }

    fn set_dead(&self, pid: u32) {
        self.alive.lock().unwrap().remove(&pid);
    }
}

impl ProcessLookup for FakeProcessLookup {
    fn owner_of(&self, _hook_pid: u32, _agent: Agent) -> Option<ProcessIdentity> {
        self.owner.clone()
    }

    fn is_alive(&self, identity: &ProcessIdentity) -> bool {
        self.alive
            .lock()
            .unwrap()
            .get(&identity.pid)
            .is_some_and(|started_at| started_at == &identity.started_at)
    }
}

struct FakeEnvSource {
    vars: HashMap<String, String>,
    commands: HashMap<(String, Vec<String>), String>,
    hostname: String,
}

impl FakeEnvSource {
    fn new() -> Self {
        Self {
            vars: HashMap::new(),
            commands: HashMap::new(),
            hostname: "test-host".into(),
        }
    }

    fn with_var(mut self, key: &str, value: &str) -> Self {
        self.vars.insert(key.to_string(), value.to_string());
        self
    }

    fn with_command(mut self, program: &str, args: &[&str], output: &str) -> Self {
        self.commands.insert(
            (
                program.to_string(),
                args.iter().map(|a| a.to_string()).collect(),
            ),
            output.to_string(),
        );
        self
    }
}

impl EnvSource for FakeEnvSource {
    fn var(&self, name: &str) -> Option<String> {
        self.vars.get(name).cloned()
    }

    fn command_output(&self, program: &str, args: &[&str]) -> Option<String> {
        let key = (
            program.to_string(),
            args.iter().map(|a| a.to_string()).collect(),
        );
        self.commands.get(&key).cloned()
    }

    fn local_hostname(&self) -> String {
        self.hostname.clone()
    }
}

fn test_environment() -> HookEnvironment {
    HookEnvironment {
        cwd: "/tmp/project".into(),
        host: "test-host".into(),
        ..Default::default()
    }
}

fn test_process() -> Option<ProcessIdentity> {
    Some(ProcessIdentity {
        pid: 4242,
        started_at: "2026-09-04T00:00:00Z".into(),
        host: "test-host".into(),
    })
}

fn normalize_for_test(agent: Agent, event: &str, native: &str) -> StatusEvent {
    normalize(agent, event, native, &test_environment(), test_process())
        .expect("normalize should succeed")
        .expect("event should not be ignored")
}

fn generic_native_json() -> &'static str {
    r#"{"session_id":"generic-session"}"#
}

#[test]
fn claude_pre_tool_use_becomes_working_without_tool_payload() {
    let native = include_str!("fixtures/claude-start.json");
    let event = normalize_for_test(Agent::Claude, "PreToolUse", native);
    assert_eq!(event.phase, Phase::Working);
    assert_eq!(event.session.id, "claude-session");
    let encoded = serde_json::to_value(event).unwrap();
    assert!(encoded.pointer("/tool_input").is_none());
    assert!(encoded.pointer("/tool_result").is_none());
}

#[test]
fn two_processes_for_one_session_remain_two_bindings() {
    let first = status("same-session", 100, 10);
    let second = status("same-session", 200, 20);
    let store = store();
    store.record(&first).unwrap();
    store.record(&second).unwrap();
    assert_eq!(store.running(&all_alive()).unwrap().len(), 2);
}

#[test]
fn codex_stop_fixture_normalizes_to_idle_and_running() {
    let native = include_str!("fixtures/codex-stop.json");
    let event = normalize_for_test(Agent::Codex, "Stop", native);
    assert_eq!(event.phase, Phase::Idle);
    assert!(event.running);
    assert_eq!(event.session.id, "codex-session");
}

#[test]
fn opencode_busy_fixture_normalizes_to_working() {
    let native = include_str!("fixtures/opencode-busy.json");
    let event = normalize_for_test(Agent::Opencode, "session.status.busy", native);
    assert_eq!(event.phase, Phase::Working);
    assert_eq!(event.session.id, "opencode-session");
}

#[test]
fn pi_settled_fixture_normalizes_to_idle() {
    let native = include_str!("fixtures/pi-settled.json");
    let event = normalize_for_test(Agent::Pi, "agent_settled", native);
    assert_eq!(event.phase, Phase::Idle);
    assert_eq!(event.session.id, "pi-session");
}

#[test]
fn every_normalized_phase_maps_correctly() {
    let cases = [
        (Agent::Claude, "SessionStart", Phase::Idle),
        (Agent::Claude, "Stop", Phase::Idle),
        (Agent::Claude, "StopFailure", Phase::Idle),
        (Agent::Claude, "UserPromptSubmit", Phase::Working),
        (Agent::Claude, "PreToolUse", Phase::Working),
        (Agent::Claude, "PostToolUse", Phase::Working),
        (Agent::Claude, "PostToolUseFailure", Phase::Working),
        (Agent::Claude, "SubagentStart", Phase::Working),
        (Agent::Claude, "PermissionRequest", Phase::Permission),
        (Agent::Claude, "PreCompact", Phase::Compacting),
        (Agent::Codex, "SessionStart", Phase::Idle),
        (Agent::Codex, "Stop", Phase::Idle),
        (Agent::Codex, "UserPromptSubmit", Phase::Working),
        (Agent::Codex, "PreToolUse", Phase::Working),
        (Agent::Codex, "PostToolUse", Phase::Working),
        (Agent::Codex, "PostCompact", Phase::Working),
        (Agent::Codex, "PermissionRequest", Phase::Permission),
        (Agent::Codex, "PreCompact", Phase::Compacting),
        (Agent::Opencode, "session.created", Phase::Idle),
        (Agent::Opencode, "session.status.idle", Phase::Idle),
        (Agent::Opencode, "session.idle", Phase::Idle),
        (Agent::Opencode, "session.error", Phase::Idle),
        (Agent::Opencode, "session.status.busy", Phase::Working),
        (Agent::Opencode, "session.status.retry", Phase::Working),
        (Agent::Opencode, "permission.asked", Phase::Permission),
        (Agent::Opencode, "session.compacted", Phase::Compacting),
        (Agent::Pi, "session_start", Phase::Idle),
        (Agent::Pi, "agent_settled", Phase::Idle),
        (Agent::Pi, "session_compact_idle", Phase::Idle),
        (Agent::Pi, "agent_start", Phase::Working),
        (Agent::Pi, "session_compact_working", Phase::Working),
        (Agent::Pi, "permission_resolved", Phase::Working),
        (Agent::Pi, "permission_prompt", Phase::Permission),
        (Agent::Pi, "session_before_compact", Phase::Compacting),
    ];
    for (agent, event, phase) in cases {
        let result = normalize_for_test(agent, event, generic_native_json());
        assert_eq!(result.phase, phase, "{agent:?} {event}");
        assert!(result.running, "{agent:?} {event}");
    }
}

#[test]
fn removal_events_end_the_binding() {
    let cases = [
        (Agent::Claude, "SessionEnd"),
        (Agent::Codex, "SessionEnd"),
        (Agent::Opencode, "session.deleted"),
        (Agent::Opencode, "server.instance.disposed"),
        (Agent::Pi, "session_shutdown"),
    ];
    for (agent, event) in cases {
        let result = normalize_for_test(agent, event, generic_native_json());
        assert!(!result.running, "{agent:?} {event}");
    }
}

#[test]
fn codex_stop_remains_idle_and_running_rather_than_ending() {
    let result = normalize_for_test(Agent::Codex, "Stop", generic_native_json());
    assert_eq!(result.phase, Phase::Idle);
    assert!(result.running);
}

#[test]
fn claude_subagent_stop_is_ignored_to_avoid_racing_the_parent_stop() {
    let result = normalize(
        Agent::Claude,
        "SubagentStop",
        generic_native_json(),
        &test_environment(),
        test_process(),
    )
    .unwrap();
    assert!(result.is_none());
}

#[test]
fn unknown_events_are_ignored() {
    let result = normalize(
        Agent::Claude,
        "TotallyUnknownEvent",
        generic_native_json(),
        &test_environment(),
        test_process(),
    )
    .unwrap();
    assert!(result.is_none());
}

#[test]
fn codex_request_user_input_tool_use_becomes_idle() {
    let native = r#"{"session_id":"codex-session","tool_name":"request_user_input"}"#;
    let result = normalize_for_test(Agent::Codex, "PreToolUse", native);
    assert_eq!(result.phase, Phase::Idle);
    assert!(result.running);
}

#[test]
fn malformed_native_json_is_rejected() {
    let result = normalize(
        Agent::Claude,
        "PreToolUse",
        "{not json",
        &test_environment(),
        test_process(),
    );
    assert!(result.is_err());
}

#[test]
fn oversized_stdin_is_rejected_before_processing() {
    let mut reader = std::io::Cursor::new(vec![b'a'; 2_000_000]);
    let result = state::read_capped(&mut reader, 1_048_576);
    assert!(result.is_err());
}

#[test]
fn stdin_at_or_under_the_cap_is_accepted() {
    let mut reader = std::io::Cursor::new(b"{}".to_vec());
    let result = state::read_capped(&mut reader, 1_048_576).unwrap();
    assert_eq!(result, "{}");
}

#[test]
fn concurrent_record_writes_all_land() {
    let store = std::sync::Arc::new(store());
    let mut handles = Vec::new();
    for i in 0..16u32 {
        let store = store.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..10 {
                store.record(&status("same-session", 100 + i, 10)).unwrap();
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(store.running(&all_alive()).unwrap().len(), 16);
}

#[test]
fn concurrent_writes_to_the_same_binding_never_produce_a_torn_file() {
    let store = std::sync::Arc::new(store());
    let mut handles = Vec::new();
    for i in 0..8u32 {
        let store = store.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..25 {
                let mut event = status(&format!("session-{i}"), 900, 90);
                event.binding_id = "shared-binding".into();
                store.record(&event).unwrap();
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(store.running(&all_alive()).unwrap().len(), 1);
}

#[test]
fn running_excludes_and_removes_dead_bindings() {
    let store = store();
    store.record(&status("dead-session", 301, 31)).unwrap();
    let liveness = FakeProcessLookup::new();
    assert!(store.running(&liveness).unwrap().is_empty());
    assert!(store.running(&all_alive()).unwrap().is_empty());
}

#[test]
fn sweep_removes_dead_bindings_and_returns_them_with_running_false() {
    let store = store();
    store.record(&status("swept-session", 302, 32)).unwrap();
    let liveness = FakeProcessLookup::new();
    let swept = store.sweep(&liveness).unwrap();
    assert_eq!(swept.len(), 1);
    assert!(!swept[0].running);
    assert_eq!(swept[0].session.id, "swept-session");
    assert!(store.running(&all_alive()).unwrap().is_empty());
}

#[test]
fn a_binding_that_was_alive_is_swept_once_its_process_exits() {
    let store = store();
    store.record(&status("exiting-session", 303, 33)).unwrap();
    let liveness = FakeProcessLookup::new();
    liveness.set_alive(303, "33");
    assert_eq!(store.running(&liveness).unwrap().len(), 1);

    liveness.set_dead(303);
    let swept = store.sweep(&liveness).unwrap();
    assert_eq!(swept.len(), 1);
    assert_eq!(swept[0].session.id, "exiting-session");
    assert!(store.running(&liveness).unwrap().is_empty());
}

#[test]
fn pid_reuse_is_treated_as_a_dead_binding() {
    let store = store();
    store.record(&status("reused-session", 400, 1)).unwrap();
    let liveness = FakeProcessLookup::new();
    liveness.set_alive(400, "2");
    assert!(store.running(&liveness).unwrap().is_empty());
}

#[test]
fn records_without_process_identity_are_diagnostics_not_running() {
    let store = store();
    let mut event = status("orphan-session", 500, 50);
    event.process = None;
    store.record(&event).unwrap();
    assert!(store.running(&all_alive()).unwrap().is_empty());
    assert!(store.sweep(&all_alive()).unwrap().is_empty());
    assert!(store.running(&all_alive()).unwrap().is_empty());
}

#[test]
fn failed_write_leaves_the_previous_complete_record_intact() {
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    let mut original = status("stable-session", 600, 60);
    original.phase = Phase::Idle;
    store.record(&original).unwrap();

    let status_dir = root.join("status");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&status_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    }

    let mut updated = original.clone();
    updated.phase = Phase::Working;
    let result = store.record(&updated);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&status_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    assert!(result.is_err());
    let records = store.running(&all_alive()).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].phase, Phase::Idle);
}

#[cfg(unix)]
#[test]
fn state_directories_and_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    store.record(&status("perm-session", 800, 80)).unwrap();

    let root_mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
    assert_eq!(root_mode, 0o700);

    let status_dir = root.join("status");
    let status_dir_mode = std::fs::metadata(&status_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(status_dir_mode, 0o700);

    let mut entries = std::fs::read_dir(&status_dir).unwrap();
    let file_path = entries.next().unwrap().unwrap().path();
    let file_mode = std::fs::metadata(&file_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(file_mode, 0o600);
}

#[test]
fn unrelated_ingest_sweeps_a_dead_binding() {
    let store = store();

    let dying_process = ProcessIdentity {
        pid: 700,
        started_at: "70".into(),
        host: "host-a".into(),
    };
    let dying_liveness = FakeProcessLookup::with_owner(dying_process);
    dying_liveness.set_alive(700, "70");

    let outcome = ingest(
        &state::IngestContext {
            store: &store,
            liveness: &dying_liveness,
            consumers: None,
            http_client: &RecordingHttpClient::default(),
        },
        Agent::Claude,
        "SessionStart",
        r#"{"session_id":"dying-session"}"#,
        700,
    );
    assert!(outcome.problem.is_none());
    assert_eq!(store.running(&dying_liveness).unwrap().len(), 1);

    let other_process = ProcessIdentity {
        pid: 701,
        started_at: "71".into(),
        host: "host-a".into(),
    };
    let other_liveness = FakeProcessLookup::with_owner(other_process);
    other_liveness.set_alive(701, "71");
    // pid 700 is unknown to `other_liveness`, so it reads as dead: this
    // simulates the dying session's process having exited by the time the
    // unrelated session's ingest runs.

    let outcome = ingest(
        &state::IngestContext {
            store: &store,
            liveness: &other_liveness,
            consumers: None,
            http_client: &RecordingHttpClient::default(),
        },
        Agent::Codex,
        "SessionStart",
        r#"{"session_id":"other-session"}"#,
        701,
    );
    assert!(outcome.problem.is_none());

    let remaining = store.running(&other_liveness).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].session.id, "other-session");
}

#[test]
fn ingest_records_a_health_problem_on_malformed_input_and_still_succeeds() {
    let store = store();
    let liveness = FakeProcessLookup::new();
    let outcome = ingest(
        &state::IngestContext {
            store: &store,
            liveness: &liveness,
            consumers: None,
            http_client: &RecordingHttpClient::default(),
        },
        Agent::Claude,
        "PreToolUse",
        "{not json",
        1,
    );
    assert!(outcome.problem.is_some());
    let problems = store.health_problems().unwrap();
    assert_eq!(problems.len(), 1);
}

#[test]
fn scalar_type_mismatch_error_omits_the_offending_value() {
    let result = normalize(
        Agent::Claude,
        "PreToolUse",
        r#"{"session_id": 424242}"#,
        &test_environment(),
        test_process(),
    );
    let message = result.unwrap_err().to_string();
    assert!(!message.contains("424242"));
    assert!(message.contains("invalid native event JSON"));
}

#[test]
fn a_type_mismatched_scalar_in_native_json_never_reaches_the_health_record() {
    let store = store();
    let liveness = FakeProcessLookup::new();
    let outcome = ingest(
        &state::IngestContext {
            store: &store,
            liveness: &liveness,
            consumers: None,
            http_client: &RecordingHttpClient::default(),
        },
        Agent::Claude,
        "PreToolUse",
        r#"{"session_id": 424242}"#,
        1,
    );
    assert!(outcome.problem.is_some());
    let problems = store.health_problems().unwrap();
    assert_eq!(problems.len(), 1);
    assert!(!problems[0].message.contains("424242"));
    assert!(problems[0].message.contains("invalid native event JSON"));
}

#[test]
fn detects_kitty_terminal_from_env_vars() {
    let env = FakeEnvSource::new()
        .with_var("KITTY_WINDOW_ID", "7")
        .with_var("KITTY_LISTEN_ON", "unix:/tmp/kitty")
        .with_var("KITTY_PID", "999");
    let gathered = environment::gather_environment(&env, "/tmp/project".into());
    let terminal = gathered.terminal.unwrap();
    assert_eq!(terminal.terminal_type.as_deref(), Some("kitty"));
    assert_eq!(terminal.session_id.as_deref(), Some("7"));
    assert_eq!(terminal.kitty_listen_on.as_deref(), Some("unix:/tmp/kitty"));
    assert_eq!(terminal.kitty_pid.as_deref(), Some("999"));
}

#[test]
fn iterm_takes_precedence_over_wezterm_when_kitty_absent() {
    let env = FakeEnvSource::new()
        .with_var("ITERM_SESSION_ID", "iterm-1")
        .with_var("WEZTERM_PANE", "wez-1");
    let gathered = environment::gather_environment(&env, "/tmp".into());
    let terminal = gathered.terminal.unwrap();
    assert_eq!(terminal.terminal_type.as_deref(), Some("iterm2"));
    assert_eq!(terminal.session_id.as_deref(), Some("iterm-1"));
}

#[test]
fn no_terminal_env_vars_means_no_terminal_identity() {
    let env = FakeEnvSource::new();
    let gathered = environment::gather_environment(&env, "/tmp".into());
    assert!(gathered.terminal.is_none());
}

#[test]
fn tmux_pane_resolves_session_name_via_the_injected_command_runner() {
    let env = FakeEnvSource::new()
        .with_var("TMUX_PANE", "%3")
        .with_command(
            "tmux",
            &["display-message", "-p", "-t", "%3", "#{session_name}"],
            "work",
        );
    let gathered = environment::gather_environment(&env, "/tmp".into());
    let tmux = gathered.tmux.unwrap();
    assert_eq!(tmux.pane.as_deref(), Some("%3"));
    assert_eq!(tmux.session_name.as_deref(), Some("work"));
}

#[test]
fn git_identity_resolves_via_the_injected_command_runner() {
    let env = FakeEnvSource::new()
        .with_command(
            "git",
            &["-C", "/tmp/project", "rev-parse", "--show-toplevel"],
            "/tmp/project",
        )
        .with_command(
            "git",
            &["-C", "/tmp/project", "rev-parse", "--abbrev-ref", "HEAD"],
            "main",
        );
    let gathered = environment::gather_environment(&env, "/tmp/project".into());
    let git = gathered.git.unwrap();
    assert_eq!(git.repo.as_deref(), Some("project"));
    assert_eq!(git.branch.as_deref(), Some("main"));
}

#[test]
fn no_git_toplevel_means_no_git_identity() {
    let env = FakeEnvSource::new();
    let gathered = environment::gather_environment(&env, "/tmp/project".into());
    assert!(gathered.git.is_none());
}

#[test]
fn remote_host_combines_user_and_short_hostname_when_ssh_connected() {
    let env = FakeEnvSource::new()
        .with_var("SSH_CONNECTION", "1.2.3.4 1 5.6.7.8 22")
        .with_var("USER", "alice")
        .with_var("HOSTNAME", "build.example.com");
    let gathered = environment::gather_environment(&env, "/tmp".into());
    assert_eq!(gathered.remote_host.as_deref(), Some("alice@build"));
}

#[test]
fn remote_host_is_none_without_ssh_connection() {
    let env = FakeEnvSource::new().with_var("USER", "alice");
    let gathered = environment::gather_environment(&env, "/tmp".into());
    assert!(gathered.remote_host.is_none());
}

#[test]
fn a_populated_environment_lands_on_the_normalized_status_event() {
    let env = FakeEnvSource::new()
        .with_var("KITTY_WINDOW_ID", "7")
        .with_var("TMUX_PANE", "%3")
        .with_command(
            "tmux",
            &["display-message", "-p", "-t", "%3", "#{session_name}"],
            "work",
        )
        .with_command(
            "git",
            &["-C", "/tmp/project", "rev-parse", "--show-toplevel"],
            "/tmp/project",
        )
        .with_command(
            "git",
            &["-C", "/tmp/project", "rev-parse", "--abbrev-ref", "HEAD"],
            "main",
        )
        .with_var("SSH_CONNECTION", "1.2.3.4 1 5.6.7.8 22")
        .with_var("USER", "alice")
        .with_var("HOSTNAME", "build.example.com");
    let hook_env = environment::gather_environment(&env, "/tmp/project".into());

    let event = normalize(
        Agent::Claude,
        "SessionStart",
        generic_native_json(),
        &hook_env,
        test_process(),
    )
    .unwrap()
    .unwrap();

    assert_eq!(
        event.terminal.unwrap().terminal_type.as_deref(),
        Some("kitty")
    );
    assert_eq!(event.tmux.unwrap().session_name.as_deref(), Some("work"));
    assert_eq!(event.git.unwrap().branch.as_deref(), Some("main"));
    assert_eq!(event.remote_host.as_deref(), Some("alice@build"));
}

#[test]
fn sessions_envelope_reports_a_problem_when_a_dead_record_cannot_be_removed() {
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    store.record(&status("stuck-session", 999, 99)).unwrap();

    let status_dir = root.join("status");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&status_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    }

    let liveness = FakeProcessLookup::new();
    let envelope = store.sessions_envelope(&liveness);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&status_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    assert!(envelope.sessions.is_empty());
    assert!(
        !envelope.problems.is_empty(),
        "expected a diagnostic when a dead record could not be removed"
    );
}

#[test]
fn sessions_envelope_includes_recorded_health_problems() {
    let store = store();
    store.record_health("a prior operational failure").unwrap();
    let liveness = all_alive();
    let envelope = store.sessions_envelope(&liveness);
    assert_eq!(envelope.problems.len(), 1);
    assert_eq!(envelope.problems[0].message, "a prior operational failure");
}

#[test]
fn fanout_sends_only_to_status_consumers() {
    let client = RecordingHttpClient::default();
    let consumers = vec![
        consumer("juggler", &["status"], Some("http://127.0.0.1:7483/hook")),
        consumer("ringleader", &["status"], None),
    ];
    SinkFanout::new(client.clone()).send(&status_event(), &consumers);
    assert_eq!(client.urls(), ["http://127.0.0.1:7483/hook"]);
}

#[test]
fn unsupported_capability_does_not_change_registration() {
    let store = consumer_store();
    store
        .register(consumer("juggler", &["status"], None))
        .unwrap();
    assert!(store.register(consumer("future", &["raw"], None)).is_err());
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn ingest_fans_out_the_recorded_event_to_registered_status_sinks() {
    let store = store();
    let consumers = consumer_store();
    consumers
        .register(consumer(
            "juggler",
            &["status"],
            Some("http://127.0.0.1:7483/hook"),
        ))
        .unwrap();
    let client = RecordingHttpClient::default();
    let liveness = FakeProcessLookup::with_owner(ProcessIdentity {
        pid: 900,
        started_at: "90".into(),
        host: "host-a".into(),
    });
    liveness.set_alive(900, "90");

    ingest(
        &state::IngestContext {
            store: &store,
            liveness: &liveness,
            consumers: Some(&consumers),
            http_client: &client,
        },
        Agent::Claude,
        "SessionStart",
        r#"{"session_id":"fanout-session"}"#,
        900,
    );

    let bodies = client.bodies();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["session"]["id"], "fanout-session");
    assert_eq!(bodies[0]["running"], true);
    assert_eq!(bodies[0]["event"], "SessionStart");
}

#[test]
fn a_dead_binding_swept_during_an_unrelated_ingest_produces_a_running_false_post() {
    let store = store();
    let consumers = consumer_store();
    consumers
        .register(consumer(
            "juggler",
            &["status"],
            Some("http://127.0.0.1:7483/hook"),
        ))
        .unwrap();
    let client = RecordingHttpClient::default();

    let dying_process = ProcessIdentity {
        pid: 710,
        started_at: "71".into(),
        host: "host-a".into(),
    };
    let dying_liveness = FakeProcessLookup::with_owner(dying_process);
    dying_liveness.set_alive(710, "71");
    ingest(
        &state::IngestContext {
            store: &store,
            liveness: &dying_liveness,
            consumers: Some(&consumers),
            http_client: &client,
        },
        Agent::Claude,
        "SessionStart",
        r#"{"session_id":"dying-session"}"#,
        710,
    );
    assert_eq!(client.urls().len(), 1);

    let other_process = ProcessIdentity {
        pid: 711,
        started_at: "72".into(),
        host: "host-a".into(),
    };
    let other_liveness = FakeProcessLookup::with_owner(other_process);
    other_liveness.set_alive(711, "72");
    // pid 710 is unknown to `other_liveness`, so the dying session reads as
    // dead and is swept by this unrelated ingest.

    let outcome = ingest(
        &state::IngestContext {
            store: &store,
            liveness: &other_liveness,
            consumers: Some(&consumers),
            http_client: &client,
        },
        Agent::Codex,
        "SessionStart",
        r#"{"session_id":"other-session"}"#,
        711,
    );
    assert!(outcome.problem.is_none());

    let bodies = client.bodies();
    assert_eq!(bodies.len(), 3);
    let swept_body = bodies
        .iter()
        .find(|b| b["session"]["id"] == "dying-session" && b["event"] == "swept")
        .expect("expected a synthetic swept POST for the dead binding");
    assert_eq!(swept_body["running"], false);
}

#[test]
fn a_sink_failure_during_sweep_fan_out_never_changes_ingests_exit_status() {
    let store = store();
    let consumers = consumer_store();
    consumers
        .register(consumer(
            "juggler",
            &["status"],
            Some("http://127.0.0.1:7483/hook"),
        ))
        .unwrap();

    let dying_process = ProcessIdentity {
        pid: 720,
        started_at: "73".into(),
        host: "host-a".into(),
    };
    let dying_liveness = FakeProcessLookup::with_owner(dying_process);
    dying_liveness.set_alive(720, "73");
    ingest(
        &state::IngestContext {
            store: &store,
            liveness: &dying_liveness,
            consumers: Some(&consumers),
            http_client: &FailingHttpClient,
        },
        Agent::Claude,
        "SessionStart",
        r#"{"session_id":"failing-sink-session"}"#,
        720,
    );

    let other_process = ProcessIdentity {
        pid: 721,
        started_at: "74".into(),
        host: "host-a".into(),
    };
    let other_liveness = FakeProcessLookup::with_owner(other_process);
    other_liveness.set_alive(721, "74");

    let outcome = ingest(
        &state::IngestContext {
            store: &store,
            liveness: &other_liveness,
            consumers: Some(&consumers),
            http_client: &FailingHttpClient,
        },
        Agent::Codex,
        "SessionStart",
        r#"{"session_id":"other-session-2"}"#,
        721,
    );

    assert!(outcome.problem.is_none());
    let remaining = store.running(&other_liveness).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].session.id, "other-session-2");

    let problems = store.health_problems().unwrap();
    assert!(
        problems
            .iter()
            .any(|p| p.message.starts_with("sink juggler failed"))
    );
}

// The OpenCode and Pi TypeScript adapters (assets/opencode-hooklinesinker.ts,
// assets/pi-hooklinesinker.ts) pipe a bare `{"session_id": ..., "cwd": ...}`
// object to `ingest` on stdin — never terminal/tmux/git/remote fields, which
// normalize.rs sources from the hook environment instead. These tests pin
// that exact stdin contract against normalize()'s NativeEvent expectations.

#[test]
fn opencode_adapter_stdin_shape_normalizes_with_its_explicit_cwd() {
    let native = r#"{"session_id":"opencode-session","cwd":"/tmp/opencode-project"}"#;
    let event = normalize(
        Agent::Opencode,
        "session.status.busy",
        native,
        &test_environment(),
        test_process(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(event.phase, Phase::Working);
    assert_eq!(event.session.id, "opencode-session");
    assert_eq!(event.session.cwd, "/tmp/opencode-project");
}

#[test]
fn opencode_adapter_synthetic_session_created_has_no_session_id_yet() {
    let native = "{}";
    let event = normalize(
        Agent::Opencode,
        "session.created",
        native,
        &test_environment(),
        test_process(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(event.phase, Phase::Idle);
    assert_eq!(event.session.id, "");
}

#[test]
fn pi_adapter_stdin_shape_omits_cwd_and_falls_back_to_the_hook_environment() {
    let native = r#"{"session_id":"pi-session"}"#;
    let event = normalize(
        Agent::Pi,
        "agent_start",
        native,
        &test_environment(),
        test_process(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(event.phase, Phase::Working);
    assert_eq!(event.session.id, "pi-session");
    assert_eq!(event.session.cwd, test_environment().cwd);
}

fn fake_candidate_binary(base: &std::path::Path, version: &str) -> PathBuf {
    let path = base
        .join("candidate-src")
        .join(format!("hooklinesinker-{version}"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!("candidate {version}")).unwrap();
    path
}

fn hook_manager_at(base: &std::path::Path, binary_path: PathBuf) -> HookManager {
    HookManager::new(HookRoots {
        claude_dir: base.join("claude"),
        codex_dir: base.join("codex"),
        opencode_config_dir: base.join("opencode"),
        pi_agent_dir: base.join("pi"),
        binary_path,
    })
}

#[test]
fn two_consumers_share_one_activation_and_both_still_receive_sink_fanout() {
    let base = temp_home();
    let install = Installer::open(base.join("data")).unwrap();
    let consumers = consumer_store();

    let installed = install
        .install_candidate(Candidate {
            version: SemVer::parse("0.1.0").unwrap(),
            protocol_major: PROTOCOL_VERSION,
            binary_path: fake_candidate_binary(&base, "0.1.0"),
        })
        .unwrap();
    consumers
        .register(consumer(
            "juggler",
            &["status"],
            Some("http://127.0.0.1:7483/hook"),
        ))
        .unwrap();

    // A second, unrelated consumer registering afterward must not reactivate
    // the same version, and both consumers must remain registered together.
    let installed_again = install
        .install_candidate(Candidate {
            version: SemVer::parse("0.1.0").unwrap(),
            protocol_major: PROTOCOL_VERSION,
            binary_path: fake_candidate_binary(&base, "0.1.0"),
        })
        .unwrap();
    consumers
        .register(consumer("ringleader", &["status"], None))
        .unwrap();

    assert_eq!(installed.active_version, installed_again.active_version);
    assert_eq!(consumers.requested_capabilities("status").unwrap().len(), 2);

    let client = RecordingHttpClient::default();
    let status_consumers = consumers.requested_capabilities("status").unwrap();
    SinkFanout::new(&client).send(&status_event(), &status_consumers);
    assert_eq!(client.urls(), ["http://127.0.0.1:7483/hook"]);
}

#[test]
fn last_consumer_uninstall_removes_installed_claude_hooks_and_the_active_symlink() {
    let base = temp_home();
    let install = Installer::open(base.join("data")).unwrap();
    let consumers = consumer_store();
    let hooks = hook_manager_at(&base, install.binary_path());

    install
        .install_candidate(Candidate {
            version: SemVer::parse("0.1.0").unwrap(),
            protocol_major: PROTOCOL_VERSION,
            binary_path: fake_candidate_binary(&base, "0.1.0"),
        })
        .unwrap();
    consumers
        .register(consumer("juggler", &["status"], None))
        .unwrap();
    hooks.install(Agent::Claude).unwrap();
    assert!(base.join("claude/settings.json").exists());

    let outcome = install
        .uninstall_consumer(&consumers, &hooks, "juggler")
        .unwrap();
    assert!(outcome.was_last_consumer);
    assert_eq!(
        hooks.status(Agent::Claude).unwrap().state,
        HookState::Missing
    );
    assert!(!install.binary_path().exists());
    assert!(
        base.join("data/versions/0.1.0/hooklinesinker").exists(),
        "version directories must survive last-consumer cleanup"
    );
}
