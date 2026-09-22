use hooklinesinker::consumers::ConsumerStore;
use hooklinesinker::environment::{self, EnvSource};
use hooklinesinker::hooks::{HookManager, HookRoots, HookState};
use hooklinesinker::ingest;
use hooklinesinker::install::{Candidate, Installer, SemVer};
use hooklinesinker::normalize::{HookEnvironment, Normalized, normalize};
use hooklinesinker::processes::{ProcessLiveness, ProcessLookup};
use hooklinesinker::protocol::{
    Agent, Capability, Consumer, PROTOCOL_VERSION, Phase, ProcessIdentity, SessionIdentity,
    StatusEvent,
};
use hooklinesinker::sinks::{HttpClient, SinkFanout};
use hooklinesinker::state::{HealthKind, HealthProblem, StatusStore};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;
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

fn ledger_files(root: &std::path::Path) -> usize {
    std::fs::read_dir(root.join("status")).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
            .count()
    })
}

fn consumer_store() -> ConsumerStore {
    ConsumerStore::open(temp_home()).unwrap()
}

fn consumer(name: &str, capabilities: &[&str], sink: Option<&str>) -> Consumer {
    Consumer {
        name: name.to_string(),
        protocol: PROTOCOL_VERSION,
        capabilities: capabilities
            .iter()
            .map(|capability| match *capability {
                "status" => Capability::Status,
                other => panic!("unsupported test capability {other}"),
            })
            .collect(),
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
    ctx: &ingest::IngestContext,
    agent: Agent,
    event: &str,
    input: &str,
    hook_pid: u32,
) -> ingest::IngestOutcome {
    ingest::handle_ingest(ctx, agent, event, input, &test_environment(), hook_pid)
}

fn status(session_id: &str, pid: u32, started_at: u64) -> StatusEvent {
    StatusEvent {
        protocol: PROTOCOL_VERSION,
        binding_id: test_binding_id(&format!("{session_id}-{pid}-{started_at}")),
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

fn test_binding_id(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut id = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(id, "{byte:02x}").unwrap();
    }
    id
}

struct AllAlive;

impl ProcessLiveness for AllAlive {
    fn process_is_alive(&self, _identity: &ProcessIdentity) -> bool {
        true
    }
}

impl ProcessLookup for AllAlive {
    fn owner_of(&self, _hook_pid: u32, _agent: Agent) -> Option<ProcessIdentity> {
        None
    }
}

const fn all_alive() -> AllAlive {
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

impl ProcessLiveness for FakeProcessLookup {
    fn process_is_alive(&self, identity: &ProcessIdentity) -> bool {
        self.alive
            .lock()
            .unwrap()
            .get(&identity.pid)
            .is_some_and(|started_at| started_at == &identity.started_at)
    }
}

impl ProcessLookup for FakeProcessLookup {
    fn owner_of(&self, _hook_pid: u32, _agent: Agent) -> Option<ProcessIdentity> {
        self.owner.clone()
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
                args.iter().map(ToString::to_string).collect(),
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
            args.iter().map(ToString::to_string).collect(),
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

fn test_process() -> ProcessIdentity {
    ProcessIdentity {
        pid: 4242,
        started_at: "2026-09-04T00:00:00Z".into(),
        host: "test-host".into(),
    }
}

fn normalize_for_test(agent: Agent, event: &str, native: &str) -> StatusEvent {
    match normalized_for_test(agent, event, native) {
        Normalized::Recordable(event) | Normalized::ForwardOnly(event) => event,
        Normalized::Ignored | Normalized::Unrecognized => {
            panic!("{agent:?} {event} should map to a status event")
        }
    }
}

fn normalized_for_test(agent: Agent, event: &str, native: &str) -> Normalized {
    normalize(
        agent,
        event,
        native,
        &test_environment(),
        Some(test_process()),
    )
    .expect("normalize should succeed")
}

const fn generic_native_json() -> &'static str {
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
fn codex_interrupt_keeps_the_session_live_and_pushes_idle_to_consumers() {
    let store = store();
    let consumers = consumer_store();
    consumers
        .register(&consumer(
            "juggler",
            &["status"],
            Some("http://127.0.0.1:7483/hook"),
        ))
        .unwrap();
    let process = test_process();
    let liveness = FakeProcessLookup::with_owner(process.clone());
    liveness.set_alive(process.pid, &process.started_at);
    let client = RecordingHttpClient::default();
    let ctx = ingest::IngestContext {
        store: &store,
        liveness: &liveness,
        consumers: Some(&consumers),
        http_client: &client,
    };
    let input = include_str!("fixtures/codex-interrupt.json");

    ingest(&ctx, Agent::Codex, "UserPromptSubmit", input, 1);
    let working = store.sessions_envelope(&liveness).sessions;
    assert_eq!(working.len(), 1);
    assert_eq!(working[0].phase, Phase::Working);

    ingest(&ctx, Agent::Codex, "Interrupt", input, 1);
    let interrupted = store.sessions_envelope(&liveness).sessions;
    assert_eq!(interrupted.len(), 1);
    assert_eq!(interrupted[0].binding_id, working[0].binding_id);
    assert_eq!(interrupted[0].phase, Phase::Idle);
    assert_eq!(interrupted[0].event, "Interrupt");
    assert!(interrupted[0].running);
    assert_eq!(interrupted[0].session.id, "codex-session");

    let bodies = client.bodies();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[1]["phase"], "idle");
    assert_eq!(bodies[1]["event"], "Interrupt");
    assert_eq!(bodies[1]["running"], true);
    assert_eq!(bodies[1]["bindingId"], working[0].binding_id);

    ingest(&ctx, Agent::Codex, "UserPromptSubmit", input, 1);
    let resumed = store.sessions_envelope(&liveness).sessions;
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].binding_id, working[0].binding_id);
    assert_eq!(resumed[0].phase, Phase::Working);
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
fn droid_notification_permission_prompt_fixture_normalizes_to_permission() {
    let native = include_str!("fixtures/droid-notification.json");
    let event = normalize_for_test(Agent::Droid, "Notification", native);
    assert_eq!(event.phase, Phase::Permission);
    assert_eq!(event.session.id, "droid-session");
}

#[test]
fn qwen_post_tool_use_failure_fixture_normalizes_to_working() {
    let native = include_str!("fixtures/qwen-failure.json");
    let event = normalize_for_test(Agent::Qwen, "PostToolUseFailure", native);
    assert_eq!(event.phase, Phase::Working);
    assert_eq!(event.session.id, "qwen-session");
}

#[test]
fn kimi_turn_started_fixture_normalizes_to_working() {
    let native = include_str!("fixtures/kimi-turn-started.json");
    let event = normalize_for_test(Agent::Kimi, "TurnStarted", native);
    assert_eq!(event.phase, Phase::Working);
    assert_eq!(event.session.id, "kimi-session");
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
        (Agent::Droid, "SessionStart", Phase::Idle),
        (Agent::Droid, "Stop", Phase::Idle),
        (Agent::Droid, "UserPromptSubmit", Phase::Working),
        (Agent::Droid, "PreToolUse", Phase::Working),
        (Agent::Droid, "PreCompact", Phase::Compacting),
        (Agent::Qwen, "SessionStart", Phase::Idle),
        (Agent::Qwen, "Stop", Phase::Idle),
        (Agent::Qwen, "StopFailure", Phase::Idle),
        (Agent::Qwen, "UserPromptSubmit", Phase::Working),
        (Agent::Qwen, "PreToolUse", Phase::Working),
        (Agent::Qwen, "PostToolUse", Phase::Working),
        (Agent::Qwen, "PostToolUseFailure", Phase::Working),
        (Agent::Qwen, "PermissionDenied", Phase::Working),
        (Agent::Qwen, "PostCompact", Phase::Working),
        (Agent::Qwen, "PermissionRequest", Phase::Permission),
        (Agent::Qwen, "PreCompact", Phase::Compacting),
        (Agent::Kimi, "SessionStart", Phase::Idle),
        (Agent::Kimi, "Stop", Phase::Idle),
        (Agent::Kimi, "StopFailure", Phase::Idle),
        (Agent::Kimi, "Interrupt", Phase::Idle),
        (Agent::Kimi, "TurnStarted", Phase::Working),
        (Agent::Kimi, "UserPromptSubmit", Phase::Working),
        (Agent::Kimi, "PreToolUse", Phase::Working),
        (Agent::Kimi, "PostToolUse", Phase::Working),
        (Agent::Kimi, "PostToolUseFailure", Phase::Working),
        (Agent::Kimi, "PermissionResult", Phase::Working),
        (Agent::Kimi, "PostCompact", Phase::Working),
        (Agent::Kimi, "PermissionRequest", Phase::Permission),
        (Agent::Kimi, "PreCompact", Phase::Compacting),
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
        (Agent::Droid, "SessionEnd"),
        (Agent::Qwen, "SessionEnd"),
        (Agent::Kimi, "SessionEnd"),
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
fn claude_subagent_stop_is_outside_the_installed_vocabulary() {
    // Never installed; a hand-wired SubagentStop must stay unrecognized so a child finishing
    // cannot mark its still-working parent idle.
    let result = normalized_for_test(Agent::Claude, "SubagentStop", generic_native_json());
    assert!(matches!(result, Normalized::Unrecognized));
}

#[test]
fn droid_notification_type_selects_the_right_phase() {
    let cases = [
        ("permission_prompt", Phase::Permission),
        ("elicitation_dialog", Phase::Permission),
        ("idle_prompt", Phase::Idle),
    ];
    for (notification_type, phase) in cases {
        let native = format!(r#"{{"session_id":"s","notification_type":"{notification_type}"}}"#);
        let event = normalize_for_test(Agent::Droid, "Notification", &native);
        assert_eq!(event.phase, phase, "{notification_type}");
    }
}

#[test]
fn droid_notification_auth_success_is_ignored() {
    let native = r#"{"session_id":"s","notification_type":"auth_success"}"#;
    let result = normalized_for_test(Agent::Droid, "Notification", native);
    // Any notification_type outside permission_prompt/elicitation_dialog/idle_prompt is ignored,
    // not unrecognized.
    assert!(matches!(result, Normalized::Ignored));
}

#[test]
fn droid_subagent_stop_is_outside_the_installed_vocabulary() {
    let result = normalized_for_test(Agent::Droid, "SubagentStop", generic_native_json());
    assert!(matches!(result, Normalized::Unrecognized));
}

#[test]
fn qwen_notification_type_selects_the_right_phase() {
    let cases = [
        ("permission_prompt", Phase::Permission),
        ("idle_prompt", Phase::Idle),
    ];
    for (notification_type, phase) in cases {
        let native = format!(r#"{{"session_id":"s","notification_type":"{notification_type}"}}"#);
        let event = normalize_for_test(Agent::Qwen, "Notification", &native);
        assert_eq!(event.phase, phase, "{notification_type}");
    }
}

#[test]
fn qwen_notification_auth_success_is_ignored() {
    let native = r#"{"session_id":"s","notification_type":"auth_success"}"#;
    let result = normalized_for_test(Agent::Qwen, "Notification", native);
    assert!(matches!(result, Normalized::Ignored));
}

#[test]
fn qwen_session_delete_is_not_tracked_since_it_names_a_different_sessions_id() {
    let result = normalized_for_test(
        Agent::Qwen,
        "SessionDelete",
        r#"{"deleted_session_id":"some-other-session"}"#,
    );
    assert!(matches!(result, Normalized::Unrecognized));
}

#[test]
fn qwen_unregistered_events_are_reported_as_unrecognized() {
    let events = [
        "MessageDisplay",
        "TodoCreated",
        "TodoCompleted",
        "SubagentStart",
        "SubagentStop",
    ];
    for event in events {
        let result = normalized_for_test(Agent::Qwen, event, generic_native_json());
        assert!(matches!(result, Normalized::Unrecognized), "{event}");
    }
}

#[test]
fn kimi_unregistered_events_are_reported_as_unrecognized() {
    let events = [
        "UserPromptQueued",
        "TaskStarted",
        "Notification",
        "SubagentStart",
        "SubagentStop",
        "SessionHeartbeat",
    ];
    for event in events {
        let result = normalized_for_test(Agent::Kimi, event, generic_native_json());
        assert!(matches!(result, Normalized::Unrecognized), "{event}");
    }
}

#[test]
fn unknown_events_are_reported_as_unrecognized() {
    let result = normalized_for_test(Agent::Claude, "TotallyUnknownEvent", generic_native_json());
    assert!(matches!(result, Normalized::Unrecognized));
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
        Some(test_process()),
    );
    assert!(result.is_err());
}

#[test]
fn oversized_stdin_is_rejected_before_processing() {
    let mut reader = std::io::Cursor::new(vec![b'a'; 2_000_000]);
    let result = ingest::read_capped(&mut reader, 1_048_576);
    assert!(result.is_err());
}

#[test]
fn stdin_at_or_under_the_cap_is_accepted() {
    let mut reader = std::io::Cursor::new(b"{}".to_vec());
    let result = ingest::read_capped(&mut reader, 1_048_576).unwrap();
    assert_eq!(result, "{}");
}

// The two sides of the cap, so an off-by-one in `take`/`>` cannot pass both.
#[test]
fn stdin_of_exactly_the_cap_is_accepted() {
    const LIMIT: usize = 1_048_576;
    let mut reader = std::io::Cursor::new(vec![b'a'; LIMIT]);
    let result = ingest::read_capped(&mut reader, LIMIT as u64).unwrap();
    assert_eq!(result.len(), LIMIT);
}

#[test]
fn stdin_one_byte_over_the_cap_is_rejected() {
    const LIMIT: usize = 1_048_576;
    let mut reader = std::io::Cursor::new(vec![b'a'; LIMIT + 1]);
    let result = ingest::read_capped(&mut reader, LIMIT as u64);
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
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
                event.binding_id = test_binding_id("shared-binding");
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
fn running_excludes_dead_bindings_without_deleting_them() {
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    store.record(&status("dead-session", 301, 31)).unwrap();
    let liveness = FakeProcessLookup::new();

    assert!(store.running(&liveness).unwrap().is_empty());
    assert_eq!(store.dead_records(&liveness).unwrap(), 1);
    assert_eq!(
        ledger_files(&root),
        1,
        "a read must leave the dead binding on disk for ingest to sweep and fan out"
    );
}

#[test]
fn binding_paths_reject_invalid_ids_before_accessing_the_filesystem() {
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    let outside = root.join("outside.json");
    std::fs::write(&outside, "keep").unwrap();

    let mut event = status("invalid-binding", 301, 31);
    event.binding_id = "../outside".into();
    assert_eq!(
        store.record(&event).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(
        store.end("../outside").unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "keep");
}

#[test]
fn sweep_removes_dead_bindings_and_returns_them_with_running_false() {
    let store = store();
    store.record(&status("swept-session", 302, 32)).unwrap();
    let liveness = FakeProcessLookup::new();
    let swept = store.sweep(&liveness).unwrap().events;
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
    let swept = store.sweep(&liveness).unwrap().events;
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
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    let mut event = status("orphan-session", 500, 50);
    event.process = None;
    event.observed_at = hooklinesinker::processes::now_rfc3339();
    store.record(&event).unwrap();
    assert!(store.running(&all_alive()).unwrap().is_empty());
    assert_eq!(store.dead_records(&all_alive()).unwrap(), 0);
    assert!(store.sweep(&all_alive()).unwrap().events.is_empty());
    assert!(store.running(&all_alive()).unwrap().is_empty());
    assert_eq!(ledger_files(&root), 1);
}

#[test]
fn unverifiable_records_expire_without_emitting_removal_events() {
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    let mut event = status("expired-orphan", 501, 51);
    event.process = None;
    event.observed_at = "2020-01-01T00:00:00Z".to_string();
    store.record(&event).unwrap();

    assert_eq!(store.dead_records(&all_alive()).unwrap(), 1);
    assert!(store.sweep(&all_alive()).unwrap().events.is_empty());
    assert_eq!(ledger_files(&root), 0);
}

#[test]
fn stale_parallel_records_expire_even_while_the_shared_process_is_alive() {
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    let event = status("stale-parallel", 502, 52);
    store.record(&event).unwrap();
    let path = std::fs::read_dir(root.join("status"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut wire: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    wire["parallel"] = serde_json::json!(true);
    wire["status"]["observedAt"] = serde_json::json!("2020-01-01T00:00:00Z");
    std::fs::write(&path, serde_json::to_vec(&wire).unwrap()).unwrap();
    let liveness = FakeProcessLookup::new();
    liveness.set_alive(502, "52");

    let events = store.sweep(&liveness).unwrap().events;
    assert_eq!(events.len(), 1);
    assert!(!events[0].running);
    assert_eq!(ledger_files(&root), 0);
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

    let mut updated = original;
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
        &ingest::IngestContext {
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
    // Omitting pid 700 simulates its exit before the unrelated ingest.

    let outcome = ingest(
        &ingest::IngestContext {
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
        &ingest::IngestContext {
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
        Some(test_process()),
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
        &ingest::IngestContext {
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
        Some(test_process()),
    )
    .unwrap()
    .into_event()
    .unwrap();

    assert_eq!(
        event.terminal.unwrap().terminal_type.as_deref(),
        Some("kitty")
    );
    assert_eq!(event.tmux.unwrap().session_name.as_deref(), Some("work"));
    assert_eq!(event.git.unwrap().branch.as_deref(), Some("main"));
    assert_eq!(event.remote_host.as_deref(), Some("alice@build"));
}

#[cfg(unix)]
#[test]
fn sessions_envelope_reports_a_problem_when_records_cannot_be_read() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    store
        .record(&status("unreadable-session", 999, 99))
        .unwrap();

    let status_dir = root.join("status");
    std::fs::set_permissions(&status_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

    let liveness = FakeProcessLookup::new();
    let envelope = store.sessions_envelope(&liveness);

    std::fs::set_permissions(&status_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

    assert!(envelope.sessions.is_empty());
    assert!(
        !envelope.problems.is_empty(),
        "expected a diagnostic when the ledger could not be read"
    );
}

#[test]
fn sessions_envelope_omits_a_dead_binding_but_leaves_it_for_the_sweep() {
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    store.record(&status("polled-session", 998, 98)).unwrap();

    let liveness = FakeProcessLookup::new();
    let envelope = store.sessions_envelope(&liveness);

    assert!(envelope.sessions.is_empty());
    assert!(envelope.problems.is_empty());
    assert_eq!(ledger_files(&root), 1);
}

#[test]
fn sessions_envelope_includes_recorded_health_problems() {
    let store = store();
    store
        .record_health(HealthProblem::new(
            HealthKind::Other,
            "a prior operational failure",
        ))
        .unwrap();
    let liveness = all_alive();
    let envelope = store.sessions_envelope(&liveness);
    assert_eq!(envelope.problems.len(), 1);
    assert_eq!(envelope.problems[0].message, "a prior operational failure");
}

#[test]
fn sessions_envelope_omits_stale_problems_but_keeps_recent_ones() {
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    let recent = hooklinesinker::processes::now_rfc3339();
    let health = format!(
        "[{{\"observedAt\":\"2020-01-01T00:00:00Z\",\"message\":\"ancient sink failure\"}},\
          {{\"observedAt\":\"{recent}\",\"message\":\"just now\"}}]"
    );
    std::fs::write(root.join("health.json"), health).unwrap();

    let envelope = store.sessions_envelope(&all_alive());
    let messages: Vec<&str> = envelope
        .problems
        .iter()
        .map(|p| p.message.as_str())
        .collect();
    assert_eq!(messages, ["just now"]);

    // Replay filtering must not prune history used by doctor diagnostics.
    assert_eq!(store.health_problems().unwrap().len(), 2);
    assert!(
        std::fs::read_to_string(root.join("health.json"))
            .unwrap()
            .contains("ancient sink failure")
    );
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
fn an_unknown_capability_keeps_the_rest_of_the_consumer_readable() {
    let store = consumer_store();
    store
        .register(&consumer("juggler", &["status"], None))
        .unwrap();
    let future = serde_json::from_value::<Consumer>(serde_json::json!({
        "name": "future",
        "protocol": PROTOCOL_VERSION,
        "capabilities": ["status", "raw"],
        "sink": null
    }))
    .expect("a newer capability must not fail the whole record");
    assert_eq!(
        future.capabilities,
        vec![Capability::Status, Capability::Unknown]
    );
    store.register(&future).unwrap();
    assert_eq!(store.list().unwrap().len(), 2);
}

#[test]
fn ingest_fans_out_the_recorded_event_to_registered_status_sinks() {
    let store = store();
    let consumers = consumer_store();
    consumers
        .register(&consumer(
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
        &ingest::IngestContext {
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
fn claude_replacement_uses_tmux_pane_identity_when_session_names_change() {
    let environment = |pane: &str, name: Option<&str>| {
        let mut source = FakeEnvSource::new().with_var("TMUX_PANE", pane);
        if let Some(name) = name {
            source = source.with_command(
                "tmux",
                &["display-message", "-p", "-t", pane, "#{session_name}"],
                name,
            );
        }
        environment::gather_environment(&source, "/tmp/project".into())
    };
    for (old_name, new_name, new_pane) in [
        (Some("work"), Some("project"), "%1"),
        (Some("work"), None, "%1"),
        (None, Some("work"), "%1"),
        (Some("work"), Some("work"), "%2"),
    ] {
        let store = store();
        let process = test_process();
        let liveness = FakeProcessLookup::with_owner(process.clone());
        liveness.set_alive(process.pid, &process.started_at);
        let consumers = consumer_store();
        consumers
            .register(&consumer(
                "monitor",
                &["status"],
                Some("http://127.0.0.1:7483/hook"),
            ))
            .unwrap();
        let client = RecordingHttpClient::default();
        let ctx = ingest::IngestContext {
            store: &store,
            liveness: &liveness,
            consumers: Some(&consumers),
            http_client: &client,
        };
        for (event, input, env) in [
            (
                "PermissionRequest",
                r#"{"session_id":"a"}"#,
                environment("%1", old_name),
            ),
            (
                "SessionStart",
                r#"{"session_id":"b","source":"clear"}"#,
                environment(new_pane, new_name),
            ),
        ] {
            let outcome =
                ingest::handle_ingest(&ctx, Agent::Claude, event, input, &env, process.pid);
            assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        }
        let mut ids: Vec<_> = store
            .running(&liveness)
            .unwrap()
            .into_iter()
            .map(|s| s.session.id)
            .collect();
        ids.sort();
        if new_pane == "%1" {
            assert_eq!(ids, ["b"], "{old_name:?} -> {new_name:?}");
            let before = client.bodies();
            assert_eq!(before.len(), 3);
            assert_eq!(before[2]["session"]["id"], "a");
            assert_eq!(before[2]["running"], false);
            let outcome = ingest::handle_ingest(
                &ctx,
                Agent::Claude,
                "Stop",
                r#"{"session_id":"a"}"#,
                &environment("%1", new_name),
                process.pid,
            );
            assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
            assert_eq!(client.bodies(), before);
            let sessions = store.running(&liveness).unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session.id, "b");
        } else {
            assert_eq!(ids, ["a", "b"]);
            assert_eq!(client.bodies().len(), 2);
        }
    }
}

#[test]
fn claude_session_start_immediately_replaces_every_foreground_phase() {
    for previous_event in [
        "SessionStart",
        "PreToolUse",
        "PermissionRequest",
        "Stop",
        "PreCompact",
    ] {
        let store = store();
        let process = test_process();
        let liveness = FakeProcessLookup::with_owner(process.clone());
        liveness.set_alive(process.pid, &process.started_at);
        let consumers = consumer_store();
        consumers
            .register(&consumer(
                "monitor",
                &["status"],
                Some("http://127.0.0.1:7483/hook"),
            ))
            .unwrap();
        let client = RecordingHttpClient::default();
        let ctx = ingest::IngestContext {
            store: &store,
            liveness: &liveness,
            consumers: Some(&consumers),
            http_client: &client,
        };
        for (event, input) in [
            (previous_event, r#"{"session_id":"a"}"#),
            ("SessionStart", r#"{"session_id":"b","source":"clear"}"#),
        ] {
            let outcome = ingest(&ctx, Agent::Claude, event, input, process.pid);
            assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        }
        let sessions = store.sessions_envelope(&liveness);
        assert!(sessions.problems.is_empty());
        assert_eq!(sessions.sessions.len(), 1, "{previous_event}");
        assert_eq!(sessions.sessions[0].session.id, "b");
        assert_eq!(sessions.sessions[0].event, "SessionStart");
        assert_eq!(sessions.sessions[0].phase, Phase::Idle);
        let removals: Vec<_> = client
            .bodies()
            .into_iter()
            .filter(|body| body["running"] == false)
            .collect();
        assert_eq!(removals.len(), 1);
        assert_eq!(removals[0]["session"]["id"], "a");
    }
}

#[test]
fn claude_activity_retires_an_abandoned_startup_in_the_same_process() {
    let root = temp_home();
    let store = StatusStore::open(&root).unwrap();
    let consumers = consumer_store();
    consumers
        .register(&consumer(
            "monitor",
            &["status"],
            Some("http://127.0.0.1:7483/hook"),
        ))
        .unwrap();
    let client = RecordingHttpClient::default();
    let process = test_process();
    let liveness = FakeProcessLookup::with_owner(process.clone());
    liveness.set_alive(process.pid, &process.started_at);
    let ctx = ingest::IngestContext {
        store: &store,
        liveness: &liveness,
        consumers: Some(&consumers),
        http_client: &client,
    };

    // Seed a pre-upgrade record, which has no private lifecycle metadata.
    store
        .record(&normalize_for_test(
            Agent::Claude,
            "SessionStart",
            r#"{"session_id":"startup"}"#,
        ))
        .unwrap();
    let outcome = ingest(
        &ctx,
        Agent::Claude,
        "PermissionRequest",
        r#"{"session_id":"conversation"}"#,
        process.pid,
    );
    assert!(outcome.problem.is_none(), "{:?}", outcome.problem);

    let sessions = StatusStore::open(&root)
        .unwrap()
        .sessions_envelope(&liveness);
    assert!(sessions.problems.is_empty());
    assert_eq!(sessions.sessions.len(), 1);
    assert_eq!(sessions.sessions[0].session.id, "conversation");
    assert_eq!(sessions.sessions[0].phase, Phase::Permission);

    let bodies = client.bodies();
    assert_eq!(bodies.len(), 2);
    let removed = bodies
        .iter()
        .find(|body| body["session"]["id"] == "startup")
        .unwrap();
    assert_eq!(removed["running"], false);
    let current = bodies
        .iter()
        .find(|body| body["session"]["id"] == "conversation")
        .unwrap();
    assert_eq!(current["running"], true);
    assert_eq!(current["phase"], "permission");

    ingest(
        &ctx,
        Agent::Claude,
        "SessionEnd",
        r#"{"session_id":"startup"}"#,
        process.pid,
    );
    assert_eq!(
        store.running(&liveness).unwrap()[0].session.id,
        "conversation"
    );
}

#[test]
fn claude_parallel_conversations_preserve_foreground_startups() {
    for parallel_input in [
        r#"{"session_id":"parallel","source":"fork"}"#,
        r#"{"session_id":"parallel","agent_id":"worker"}"#,
    ] {
        let root = temp_home();
        let store = StatusStore::open(&root).unwrap();
        let process = test_process();
        let liveness = FakeProcessLookup::with_owner(process.clone());
        liveness.set_alive(process.pid, &process.started_at);
        let client = RecordingHttpClient::default();
        let ctx = ingest::IngestContext {
            store: &store,
            liveness: &liveness,
            consumers: None,
            http_client: &client,
        };
        for input in [
            r#"{"session_id":"foreground","source":"startup"}"#,
            parallel_input,
        ] {
            let outcome = ingest(&ctx, Agent::Claude, "SessionStart", input, process.pid);
            assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        }

        let reopened = StatusStore::open(&root).unwrap();
        let ctx = ingest::IngestContext {
            store: &reopened,
            ..ctx
        };
        let outcome = ingest(
            &ctx,
            Agent::Claude,
            "PermissionRequest",
            r#"{"session_id":"parallel"}"#,
            process.pid,
        );
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        let sessions = reopened.running(&liveness).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(
            sessions
                .iter()
                .find(|s| s.session.id == "foreground")
                .unwrap()
                .event,
            "SessionStart"
        );
        assert_eq!(
            sessions
                .iter()
                .find(|s| s.session.id == "parallel")
                .unwrap()
                .phase,
            Phase::Permission
        );
    }
}

#[test]
fn claude_foreground_activity_preserves_fork_startups() {
    let store = store();
    let process = test_process();
    let liveness = FakeProcessLookup::with_owner(process.clone());
    liveness.set_alive(process.pid, &process.started_at);
    let client = RecordingHttpClient::default();
    let ctx = ingest::IngestContext {
        store: &store,
        liveness: &liveness,
        consumers: None,
        http_client: &client,
    };
    for (event, input) in [
        ("SessionStart", r#"{"session_id":"fork","source":"fork"}"#),
        (
            "SessionStart",
            r#"{"session_id":"abandoned","source":"startup"}"#,
        ),
        ("PermissionRequest", r#"{"session_id":"foreground"}"#),
    ] {
        let outcome = ingest(&ctx, Agent::Claude, event, input, process.pid);
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
    }
    let mut ids: Vec<_> = store
        .running(&liveness)
        .unwrap()
        .into_iter()
        .map(|s| s.session.id)
        .collect();
    ids.sort();
    assert_eq!(ids, ["foreground", "fork"]);
}

#[test]
fn claude_replacement_preserves_other_bindings() {
    struct OwnerAlive(ProcessIdentity);
    impl ProcessLiveness for OwnerAlive {
        fn process_is_alive(&self, _: &ProcessIdentity) -> bool {
            true
        }
    }
    impl ProcessLookup for OwnerAlive {
        fn owner_of(&self, _: u32, _: Agent) -> Option<ProcessIdentity> {
            Some(self.0.clone())
        }
    }

    let root = temp_home();
    let store = StatusStore::open(&root).unwrap();
    let process = test_process();
    let base = normalize_for_test(
        Agent::Claude,
        "SessionStart",
        r#"{"session_id":"original"}"#,
    );
    let mut expected = Vec::new();
    for (index, case) in [
        "pid",
        "start",
        "host",
        "agent",
        "terminal",
        "tmux",
        "remote",
        "unverifiable",
    ]
    .into_iter()
    .enumerate()
    {
        let mut other = base.clone();
        other.binding_id = format!("{index:064x}");
        other.session.id = case.into();
        match case {
            "pid" => other.process.as_mut().unwrap().pid += 1,
            "start" => other.process.as_mut().unwrap().started_at = "2026-09-03T00:00:00Z".into(),
            "host" => other.process.as_mut().unwrap().host = "another-host".into(),
            "agent" => other.agent = Agent::Codex,
            "terminal" => {
                other.terminal = Some(hooklinesinker::protocol::TerminalIdentity {
                    session_id: Some("another-terminal".into()),
                    terminal_type: Some("iterm2".into()),
                    kitty_listen_on: None,
                    kitty_pid: None,
                });
            }
            "tmux" => {
                other.tmux = Some(hooklinesinker::protocol::TmuxIdentity {
                    pane: Some("%2".into()),
                    session_name: Some("work".into()),
                });
            }
            "remote" => other.remote_host = Some("remote-host".into()),
            "unverifiable" => other.process = None,
            _ => unreachable!(),
        }
        expected.push(case.to_string());
        store.record(&other).unwrap();
    }
    let liveness = OwnerAlive(process.clone());
    let client = RecordingHttpClient::default();
    let outcome = ingest(
        &ingest::IngestContext {
            store: &store,
            liveness: &liveness,
            consumers: None,
            http_client: &client,
        },
        Agent::Claude,
        "PermissionRequest",
        r#"{"session_id":"current"}"#,
        process.pid,
    );
    assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
    expected.retain(|id| id != "unverifiable");
    expected.push("current".into());
    expected.sort();
    let mut actual: Vec<_> = store
        .running(&liveness)
        .unwrap()
        .into_iter()
        .map(|s| s.session.id)
        .collect();
    actual.sort();
    assert_eq!(actual, expected);
    assert!(root.join(format!("status/{:064x}.json", 7)).is_file());
}

#[test]
fn claude_replacement_requires_identified_foreground_activity() {
    for (agent, event, input, identified) in [
        (
            Agent::Claude,
            "PermissionRequest",
            r#"{"session_id":""}"#,
            true,
        ),
        (Agent::Claude, "SessionEnd", r#"{"session_id":"new"}"#, true),
        (
            Agent::Codex,
            "PermissionRequest",
            r#"{"session_id":"new"}"#,
            true,
        ),
        (
            Agent::Claude,
            "PermissionRequest",
            r#"{"session_id":"new","agent_id":"worker"}"#,
            true,
        ),
        (
            Agent::Claude,
            "PermissionRequest",
            r#"{"session_id":"new"}"#,
            false,
        ),
    ] {
        let root = temp_home();
        let store = StatusStore::open(&root).unwrap();
        let startup =
            normalize_for_test(Agent::Claude, "SessionStart", r#"{"session_id":"startup"}"#);
        store.record(&startup).unwrap();
        let process = test_process();
        let liveness = if identified {
            FakeProcessLookup::with_owner(process.clone())
        } else {
            FakeProcessLookup::new()
        };
        liveness.set_alive(process.pid, &process.started_at);
        let client = RecordingHttpClient::default();
        let outcome = ingest(
            &ingest::IngestContext {
                store: &store,
                liveness: &liveness,
                consumers: None,
                http_client: &client,
            },
            agent,
            event,
            input,
            process.pid,
        );
        assert!(outcome.problem.is_none(), "{:?}", outcome.problem);
        let preserved: StatusEvent = serde_json::from_slice(
            &std::fs::read(
                root.join("status")
                    .join(format!("{}.json", startup.binding_id)),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(preserved.session.id, "startup");
        assert_eq!(preserved.event, "SessionStart");
        assert!(preserved.running);
    }
}

#[test]
fn retired_claude_hooks_neither_resurrect_the_session_nor_reach_sinks() {
    let root = temp_home();
    let store = StatusStore::open(&root).unwrap();
    let process = test_process();
    let liveness = FakeProcessLookup::with_owner(process.clone());
    liveness.set_alive(process.pid, &process.started_at);
    let consumers = consumer_store();
    consumers
        .register(&consumer(
            "monitor",
            &["status"],
            Some("http://127.0.0.1:7483/hook"),
        ))
        .unwrap();
    let client = RecordingHttpClient::default();
    let ctx = ingest::IngestContext {
        store: &store,
        liveness: &liveness,
        consumers: Some(&consumers),
        http_client: &client,
    };
    for (event, input) in [
        ("PreToolUse", r#"{"session_id":"a"}"#),
        ("SessionStart", r#"{"session_id":"b","source":"clear"}"#),
        ("PermissionRequest", r#"{"session_id":"b"}"#),
    ] {
        assert!(
            ingest(&ctx, Agent::Claude, event, input, process.pid)
                .problem
                .is_none()
        );
    }
    let reopened = StatusStore::open(&root).unwrap();
    let ctx = ingest::IngestContext {
        store: &reopened,
        ..ctx
    };
    let before = client.bodies();
    for (event, input) in [
        ("SessionEnd", r#"{"session_id":"a"}"#),
        ("SessionStart", r#"{"session_id":"a","source":"startup"}"#),
        ("SessionStart", r#"{"session_id":"a","source":"clear"}"#),
        ("SessionStart", r#"{"session_id":"a","source":"compact"}"#),
        ("PreToolUse", r#"{"session_id":"a"}"#),
        (
            "PermissionRequest",
            r#"{"session_id":"a","agent_id":"worker"}"#,
        ),
        ("Stop", r#"{"session_id":"a"}"#),
    ] {
        assert!(
            ingest(&ctx, Agent::Claude, event, input, process.pid)
                .problem
                .is_none()
        );
    }
    assert_eq!(client.bodies(), before);
    let sessions = reopened.sessions_envelope(&liveness);
    assert!(sessions.problems.is_empty());
    assert_eq!(sessions.sessions.len(), 1);
    assert_eq!(sessions.sessions[0].session.id, "b");
    assert_eq!(sessions.sessions[0].phase, Phase::Permission);
}

#[test]
fn claude_can_explicitly_resume_a_retired_or_forked_conversation() {
    for source in ["startup", "fork"] {
        let store = store();
        let process = test_process();
        let liveness = FakeProcessLookup::with_owner(process.clone());
        liveness.set_alive(process.pid, &process.started_at);
        let client = RecordingHttpClient::default();
        let ctx = ingest::IngestContext {
            store: &store,
            liveness: &liveness,
            consumers: None,
            http_client: &client,
        };
        let initial = serde_json::json!({"session_id": "a", "source": source}).to_string();
        for (event, input) in [
            ("SessionStart", initial.as_str()),
            ("SessionStart", r#"{"session_id":"b","source":"clear"}"#),
            ("SessionStart", r#"{"session_id":"a","source":"resume"}"#),
            ("PermissionRequest", r#"{"session_id":"a"}"#),
            ("Stop", r#"{"session_id":"b"}"#),
        ] {
            assert!(
                ingest(&ctx, Agent::Claude, event, input, process.pid)
                    .problem
                    .is_none()
            );
        }
        let sessions = store.running(&liveness).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session.id, "a");
        assert_eq!(sessions[0].phase, Phase::Permission);
    }
}

#[test]
fn claude_end_retains_a_guard_until_process_exit_without_duplicate_removals() {
    let root = temp_home();
    let store = StatusStore::open(&root).unwrap();
    let process = test_process();
    let liveness = FakeProcessLookup::with_owner(process.clone());
    liveness.set_alive(process.pid, &process.started_at);
    let consumers = consumer_store();
    consumers
        .register(&consumer(
            "monitor",
            &["status"],
            Some("http://127.0.0.1:7483/hook"),
        ))
        .unwrap();
    let client = RecordingHttpClient::default();
    let ctx = ingest::IngestContext {
        store: &store,
        liveness: &liveness,
        consumers: Some(&consumers),
        http_client: &client,
    };
    for event in ["SessionStart", "SessionEnd"] {
        assert!(
            ingest(
                &ctx,
                Agent::Claude,
                event,
                r#"{"session_id":"a"}"#,
                process.pid
            )
            .problem
            .is_none()
        );
    }
    let before = client.bodies();
    assert_eq!(before.len(), 2);
    assert_eq!(before[1]["event"], "SessionEnd");
    assert_eq!(before[1]["running"], false);
    assert!(store.running(&liveness).unwrap().is_empty());
    assert!(
        ingest(
            &ctx,
            Agent::Claude,
            "Stop",
            r#"{"session_id":"a"}"#,
            process.pid
        )
        .problem
        .is_none()
    );
    assert_eq!(client.bodies(), before);
    assert_eq!(ledger_files(&root), 1);
    liveness.set_dead(process.pid);
    assert_eq!(store.dead_records(&liveness).unwrap(), 0);
    assert_eq!(ledger_files(&root), 1);
    // An event outside Claude's vocabulary is reported, and still drives the sweep.
    let outcome = ingest(&ctx, Agent::Claude, "Unknown", "{}", process.pid);
    assert!(
        outcome
            .problem
            .unwrap()
            .contains("unrecognized claude event")
    );
    assert_eq!(client.bodies(), before);
    assert_eq!(ledger_files(&root), 0);
}

#[test]
fn claude_subagent_hooks_do_not_change_the_parent_binding_to_parallel() {
    let store = store();
    let process = test_process();
    let liveness = FakeProcessLookup::with_owner(process.clone());
    liveness.set_alive(process.pid, &process.started_at);
    let client = RecordingHttpClient::default();
    let ctx = ingest::IngestContext {
        store: &store,
        liveness: &liveness,
        consumers: None,
        http_client: &client,
    };
    for (event, input) in [
        ("SessionStart", r#"{"session_id":"a","source":"startup"}"#),
        ("PreToolUse", r#"{"session_id":"a","agent_id":"worker"}"#),
        ("SessionStart", r#"{"session_id":"b","source":"clear"}"#),
    ] {
        assert!(
            ingest(&ctx, Agent::Claude, event, input, process.pid)
                .problem
                .is_none()
        );
    }
    let sessions = store.running(&liveness).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session.id, "b");
}

#[test]
fn failed_claude_status_write_preserves_the_startup() {
    let root = temp_home();
    let store = StatusStore::open(&root).unwrap();
    let startup = normalize_for_test(Agent::Claude, "SessionStart", r#"{"session_id":"startup"}"#);
    let current = normalize_for_test(
        Agent::Claude,
        "PermissionRequest",
        r#"{"session_id":"current"}"#,
    );
    store.record(&startup).unwrap();
    std::fs::create_dir(
        root.join("status")
            .join(format!("{}.json", current.binding_id)),
    )
    .unwrap();
    let process = test_process();
    let liveness = FakeProcessLookup::with_owner(process.clone());
    liveness.set_alive(process.pid, &process.started_at);
    let client = RecordingHttpClient::default();
    let outcome = ingest(
        &ingest::IngestContext {
            store: &store,
            liveness: &liveness,
            consumers: None,
            http_client: &client,
        },
        Agent::Claude,
        "PermissionRequest",
        r#"{"session_id":"current"}"#,
        process.pid,
    );
    assert!(outcome.problem.is_some());
    let sessions = store.sessions_envelope(&liveness);
    assert_eq!(sessions.sessions.len(), 1);
    assert_eq!(sessions.sessions[0].session.id, "startup");
    assert!(sessions.sessions[0].running);
}

#[test]
fn a_dead_binding_swept_during_an_unrelated_ingest_produces_a_running_false_post() {
    let store = store();
    let consumers = consumer_store();
    consumers
        .register(&consumer(
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
        &ingest::IngestContext {
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
    // Omitting pid 710 makes the unrelated ingest sweep it as dead.

    let outcome = ingest(
        &ingest::IngestContext {
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
fn a_read_between_the_kill_and_the_next_ingest_still_lets_the_sweep_fan_out() {
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    let consumers = consumer_store();
    consumers
        .register(&consumer(
            "juggler",
            &["status"],
            Some("http://127.0.0.1:7483/hook"),
        ))
        .unwrap();
    let client = RecordingHttpClient::default();

    let dying_process = ProcessIdentity {
        pid: 730,
        started_at: "75".into(),
        host: "host-a".into(),
    };
    let dying_liveness = FakeProcessLookup::with_owner(dying_process);
    dying_liveness.set_alive(730, "75");
    ingest(
        &ingest::IngestContext {
            store: &store,
            liveness: &dying_liveness,
            consumers: Some(&consumers),
            http_client: &client,
        },
        Agent::Claude,
        "SessionStart",
        r#"{"session_id":"killed-session"}"#,
        730,
    );

    let other_process = ProcessIdentity {
        pid: 731,
        started_at: "76".into(),
        host: "host-a".into(),
    };
    let other_liveness = FakeProcessLookup::with_owner(other_process);
    other_liveness.set_alive(731, "76");
    // Read-only polls must leave dead bindings for the next ingest sweep.
    let envelope = store.sessions_envelope(&other_liveness);
    assert!(envelope.sessions.is_empty());
    assert_eq!(store.dead_records(&other_liveness).unwrap(), 1);
    assert_eq!(ledger_files(&root), 1);

    ingest(
        &ingest::IngestContext {
            store: &store,
            liveness: &other_liveness,
            consumers: Some(&consumers),
            http_client: &client,
        },
        Agent::Codex,
        "SessionStart",
        r#"{"session_id":"surviving-session"}"#,
        731,
    );

    let swept_body = client
        .bodies()
        .into_iter()
        .find(|b| b["session"]["id"] == "killed-session" && b["event"] == "swept")
        .expect("the poll must not swallow the swept binding's running:false fan-out");
    assert_eq!(swept_body["running"], false);
}

#[test]
fn an_event_without_a_session_id_reaches_sinks_but_never_the_ledger() {
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    let consumers = consumer_store();
    consumers
        .register(&consumer(
            "juggler",
            &["status"],
            Some("http://127.0.0.1:7483/hook"),
        ))
        .unwrap();
    let client = RecordingHttpClient::default();
    let liveness = FakeProcessLookup::with_owner(ProcessIdentity {
        pid: 740,
        started_at: "77".into(),
        host: "host-a".into(),
    });
    liveness.set_alive(740, "77");
    let ctx = ingest::IngestContext {
        store: &store,
        liveness: &liveness,
        consumers: Some(&consumers),
        http_client: &client,
    };

    let outcome = ingest(&ctx, Agent::Opencode, "session.created", "{}", 740);

    assert!(outcome.problem.is_none());
    let bodies = client.bodies();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["session"]["id"], "");
    assert_eq!(bodies[0]["running"], true);
    assert_eq!(ledger_files(&root), 0);
    assert!(store.sessions_envelope(&liveness).sessions.is_empty());

    ingest(
        &ctx,
        Agent::Opencode,
        "session.status.busy",
        r#"{"session_id":"oc-1"}"#,
        740,
    );

    assert_eq!(ledger_files(&root), 1);
    let sessions = store.sessions_envelope(&liveness).sessions;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session.id, "oc-1");
}

#[test]
fn every_agent_drops_an_empty_session_id_from_the_ledger() {
    let cases = [
        (Agent::Claude, "SessionStart"),
        (Agent::Codex, "SessionStart"),
        (Agent::Opencode, "session.created"),
        (Agent::Pi, "session_start"),
        (Agent::Droid, "SessionStart"),
        (Agent::Qwen, "SessionStart"),
        (Agent::Kimi, "SessionStart"),
    ];
    for (agent, event) in cases {
        let root = temp_home();
        let store = StatusStore::open(root.clone()).unwrap();
        let liveness = FakeProcessLookup::with_owner(ProcessIdentity {
            pid: 750,
            started_at: "78".into(),
            host: "host-a".into(),
        });
        liveness.set_alive(750, "78");

        ingest(
            &ingest::IngestContext {
                store: &store,
                liveness: &liveness,
                consumers: None,
                http_client: &RecordingHttpClient::default(),
            },
            agent,
            event,
            r#"{"session_id":""}"#,
            750,
        );

        assert_eq!(ledger_files(&root), 0, "{agent:?} {event}");
    }
}

#[test]
fn a_sink_failure_during_sweep_fan_out_is_reported_without_disturbing_the_ledger() {
    let store = store();
    let consumers = consumer_store();
    consumers
        .register(&consumer(
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
        &ingest::IngestContext {
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
        &ingest::IngestContext {
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

    // Sink failures reach stderr so a human running ingest by hand sees them; the exit-0
    // contract is unaffected and is pinned by the CLI suite.
    assert!(
        outcome
            .problem
            .as_deref()
            .is_some_and(|problem| problem.contains("sink juggler failed")),
        "expected the sink failure to be reported, got {:?}",
        outcome.problem
    );
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

// Adapter stdin excludes navigation metadata; normalization must gather it from the hook environment.

#[test]
fn opencode_adapter_stdin_shape_normalizes_with_its_explicit_cwd() {
    let native = r#"{"session_id":"opencode-session","cwd":"/tmp/opencode-project"}"#;
    let event = normalize(
        Agent::Opencode,
        "session.status.busy",
        native,
        &test_environment(),
        Some(test_process()),
    )
    .unwrap()
    .into_event()
    .unwrap();
    assert_eq!(event.phase, Phase::Working);
    assert_eq!(event.session.id, "opencode-session");
    assert_eq!(event.session.cwd, "/tmp/opencode-project");
}

#[test]
fn opencode_adapter_synthetic_session_created_has_no_session_id_yet() {
    let result = normalized_for_test(Agent::Opencode, "session.created", "{}");
    let Normalized::ForwardOnly(event) = result else {
        panic!("an event without a session id must never be recordable");
    };
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
        Some(test_process()),
    )
    .unwrap()
    .into_event()
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
        factory_dir: base.join("factory"),
        qwen_config_dir: base.join("qwen"),
        kimi_code_dir: base.join("kimi-code"),
        binary_path,
    })
}

#[test]
fn two_consumers_share_one_activation_and_both_still_receive_sink_fanout() {
    let base = temp_home();
    let install = Installer::open(base.join("data")).unwrap();
    let consumers = consumer_store();

    let installed = install
        .install_candidate(&Candidate {
            version: SemVer::parse("0.1.0").unwrap(),
            protocol_major: PROTOCOL_VERSION,
            binary_path: fake_candidate_binary(&base, "0.1.0"),
        })
        .unwrap();
    consumers
        .register(&consumer(
            "juggler",
            &["status"],
            Some("http://127.0.0.1:7483/hook"),
        ))
        .unwrap();

    let installed_again = install
        .install_candidate(&Candidate {
            version: SemVer::parse("0.1.0").unwrap(),
            protocol_major: PROTOCOL_VERSION,
            binary_path: fake_candidate_binary(&base, "0.1.0"),
        })
        .unwrap();
    consumers
        .register(&consumer("ringleader", &["status"], None))
        .unwrap();

    assert_eq!(installed.active_version, installed_again.active_version);
    assert_eq!(
        consumers
            .requested_capabilities(Capability::Status)
            .unwrap()
            .len(),
        2
    );

    let client = RecordingHttpClient::default();
    let status_consumers = consumers
        .requested_capabilities(Capability::Status)
        .unwrap();
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
        .install_candidate(&Candidate {
            version: SemVer::parse("0.1.0").unwrap(),
            protocol_major: PROTOCOL_VERSION,
            binary_path: fake_candidate_binary(&base, "0.1.0"),
        })
        .unwrap();
    consumers
        .register(&consumer("juggler", &["status"], None))
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

#[test]
fn sessions_snapshot_retains_valid_records_and_reports_damaged_ones() {
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    store.record(&status("valid", 700, 70)).unwrap();
    std::fs::write(root.join("status/broken.json"), "{").unwrap();
    std::fs::create_dir(root.join("status/unreadable.json")).unwrap();
    std::fs::write(root.join("health.json"), "{").unwrap();
    let envelope = store.sessions_envelope(&all_alive());
    assert_eq!(envelope.sessions.len(), 1);
    assert_eq!(envelope.sessions[0].session.id, "valid");
    assert_eq!(envelope.problems.len(), 3);
    for file in ["broken.json", "unreadable.json", "health.json"] {
        assert!(
            envelope.problems.iter().any(|p| p.message.contains(file)),
            "missing {file}"
        );
    }
    assert_eq!(
        store.health_problems().unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    assert_eq!(
        std::fs::read_to_string(root.join("health.json")).unwrap(),
        "{"
    );
}

#[test]
fn partial_sweep_failure_still_delivers_successful_removals() {
    use std::cell::Cell;
    struct FailingUnlink {
        paths: HashMap<u32, PathBuf>,
        checks: Cell<usize>,
        failed_pid: Cell<Option<u32>>,
    }
    impl ProcessLookup for FailingUnlink {
        fn owner_of(&self, _: u32, _: Agent) -> Option<ProcessIdentity> {
            None
        }
    }
    impl ProcessLiveness for FailingUnlink {
        fn process_is_alive(&self, identity: &ProcessIdentity) -> bool {
            let check = self.checks.get();
            self.checks.set(check + 1);
            if check == 1 {
                let path = &self.paths[&identity.pid];
                std::fs::rename(path, path.with_extension("saved")).unwrap();
                std::fs::create_dir(path).unwrap();
                self.failed_pid.set(Some(identity.pid));
            }
            false
        }
    }
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    let mut paths = HashMap::new();
    for pid in 800..803 {
        let event = status("partial-sweep", pid, 80);
        paths.insert(
            pid,
            root.join("status")
                .join(format!("{}.json", event.binding_id)),
        );
        store.record(&event).unwrap();
    }
    let liveness = FailingUnlink {
        paths,
        checks: Cell::new(0),
        failed_pid: Cell::new(None),
    };
    let consumers = consumer_store();
    consumers
        .register(&consumer(
            "sink",
            &["status"],
            Some("http://localhost/hook"),
        ))
        .unwrap();
    let client = RecordingHttpClient::default();
    let ctx = ingest::IngestContext {
        store: &store,
        liveness: &liveness,
        consumers: Some(&consumers),
        http_client: &client,
    };
    let outcome = ingest(&ctx, Agent::Claude, "ignored", "{}", 999);
    assert!(
        outcome
            .problem
            .unwrap()
            .contains("failed to remove status record")
    );
    let problems = store.health_problems().unwrap();
    assert_eq!(problems.len(), 2);
    assert!(
        problems
            .iter()
            .any(|p| p.message.contains("unrecognized claude event"))
    );
    let bodies = client.bodies();
    assert_eq!(bodies.len(), 2);
    let mut delivered: Vec<_> = bodies
        .iter()
        .map(|event| event["process"]["pid"].as_u64().unwrap())
        .collect();
    delivered.sort_unstable();
    let expected: Vec<_> = (800..803)
        .filter(|pid| Some(*pid) != liveness.failed_pid.get())
        .map(u64::from)
        .collect();
    assert_eq!(delivered, expected);
    for event in &bodies {
        assert_eq!(event["running"], false);
        assert_eq!(event["event"], "swept");
    }
    assert_eq!(ledger_files(&root), 1);
}

#[test]
fn damaged_consumer_record_reports_health_while_valid_sink_receives_event() {
    let root = temp_home();
    let consumers = ConsumerStore::open(root.clone()).unwrap();
    consumers
        .register(&consumer(
            "sink",
            &["status"],
            Some("http://localhost/hook"),
        ))
        .unwrap();
    std::fs::write(root.join("consumers/broken.json"), "{").unwrap();
    let store = store();
    let liveness = all_alive();
    let client = RecordingHttpClient::default();
    let ctx = ingest::IngestContext {
        store: &store,
        liveness: &liveness,
        consumers: Some(&consumers),
        http_client: &client,
    };
    ingest(
        &ctx,
        Agent::Claude,
        "Stop",
        r#"{"session_id":"valid"}"#,
        999,
    );
    assert_eq!(client.bodies().len(), 1);
    assert_eq!(client.bodies()[0]["phase"], "idle");
    assert!(
        store.health_problems().unwrap()[0]
            .message
            .contains("broken.json")
    );
}

#[test]
fn an_identical_repeated_problem_refreshes_its_entry_instead_of_evicting_others() {
    let store = store();
    store
        .record_health(HealthProblem::new(HealthKind::Ingest, "a recurring fault"))
        .unwrap();
    store
        .record_health(HealthProblem::new(HealthKind::Other, "something else"))
        .unwrap();
    for _ in 0..80 {
        store
            .record_health(HealthProblem::new(HealthKind::Ingest, "a recurring fault"))
            .unwrap();
    }

    let problems = store.health_problems().unwrap();
    let messages: Vec<&str> = problems.iter().map(|p| p.message.as_str()).collect();
    assert_eq!(messages, ["something else", "a recurring fault"]);
}

#[test]
fn the_same_message_under_a_different_kind_is_a_separate_problem() {
    let store = store();
    store
        .record_health(HealthProblem::new(HealthKind::Ingest, "same words"))
        .unwrap();
    store
        .record_health(HealthProblem::new(HealthKind::Sweep, "same words"))
        .unwrap();
    assert_eq!(store.health_problems().unwrap().len(), 2);
}

#[test]
fn health_history_keeps_only_the_newest_fifty_problems() {
    let store = store();
    for index in 0..51 {
        store
            .record_health(HealthProblem::new(
                HealthKind::Other,
                format!("problem-{index:02}"),
            ))
            .unwrap();
    }

    let problems = store.health_problems().unwrap();
    assert_eq!(problems.len(), 50);
    assert_eq!(problems.first().unwrap().message, "problem-01");
    assert_eq!(problems.last().unwrap().message, "problem-50");
}

#[test]
fn a_sink_failure_is_classified_by_kind_not_by_its_message_wording() {
    let store = store();
    store
        .record_health(HealthProblem::new(
            HealthKind::Sink {
                consumer: "juggler".into(),
            },
            "anything at all",
        ))
        .unwrap();
    store
        .record_health(HealthProblem::new(
            HealthKind::Ingest,
            "sink juggler failed: a message that only looks like one",
        ))
        .unwrap();

    let problems = store.health_problems().unwrap();
    let sink: Vec<&str> = problems
        .iter()
        .filter(|p| p.is_sink_failure())
        .map(|p| p.message.as_str())
        .collect();
    assert_eq!(sink, ["anything at all"]);
}

#[test]
fn a_malformed_health_timestamp_is_reported_instead_of_replaying_as_current() {
    let root = temp_home();
    let store = StatusStore::open(root.clone()).unwrap();
    std::fs::write(
        root.join("health.json"),
        r#"[{"observedAt":"not a timestamp","message":"ancient fault"}]"#,
    )
    .unwrap();

    assert_eq!(
        store.health_problems().unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    let envelope = store.sessions_envelope(&all_alive());
    assert_eq!(envelope.problems.len(), 1);
    assert!(
        envelope.problems[0]
            .message
            .contains("failed to read health problems")
    );
}

#[test]
fn a_slow_sink_cannot_stretch_a_sweep_backlog_past_the_fanout_budget() {
    struct SlowHttpClient {
        calls: Mutex<usize>,
    }

    impl HttpClient for SlowHttpClient {
        fn post_json(&self, _url: &str, _body: &[u8]) -> Result<u16, String> {
            *self.calls.lock().unwrap() += 1;
            std::thread::sleep(std::time::Duration::from_millis(150));
            Ok(200)
        }
    }

    let root = temp_home();
    let store = StatusStore::open(&root).unwrap();
    let liveness = FakeProcessLookup::new();
    for pid in 900..940 {
        store.record(&status("backlog", pid, 90)).unwrap();
    }
    let consumers = consumer_store();
    consumers
        .register(&consumer(
            "sink",
            &["status"],
            Some("http://localhost/hook"),
        ))
        .unwrap();
    let client = SlowHttpClient {
        calls: Mutex::new(0),
    };
    let ctx = ingest::IngestContext {
        store: &store,
        liveness: &liveness,
        consumers: Some(&consumers),
        http_client: &client,
    };

    let started = std::time::Instant::now();
    ingest(&ctx, Agent::Claude, "Stop", r#"{"session_id":"live"}"#, 1);
    let elapsed = started.elapsed();

    let sent = *client.calls.lock().unwrap();
    assert!(sent < 40, "every one of 40 swept events was delivered");
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "fan-out took {elapsed:?}, which no longer fits a hook budget"
    );
    let problems = store.health_problems().unwrap();
    assert!(
        problems
            .iter()
            .any(|p| p.message.contains("sink delivery attempt(s) were skipped")),
        "the dropped events were not recorded: {problems:?}"
    );
}
