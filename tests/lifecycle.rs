use crate as hooklinesinker;
use hooklinesinker::consumers::ConsumerStore;
use hooklinesinker::ingest::{IngestContext, handle_ingest};
use hooklinesinker::normalize::{HookEnvironment, normalize};
use hooklinesinker::processes::{ProcessLiveness, ProcessLookup};
use hooklinesinker::protocol::{
    Agent, Capability, Consumer, Phase, ProcessIdentity, StatusEvent, TerminalIdentity,
};
use hooklinesinker::sinks::HttpClient;
use hooklinesinker::state::StatusStore;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct Owner {
    identity: ProcessIdentity,
    shared: bool,
}

impl ProcessLiveness for Owner {
    fn process_is_alive(&self, _: &ProcessIdentity) -> bool {
        true
    }
}

impl ProcessLookup for Owner {
    fn owner_of(&self, _: u32, _: Agent) -> Option<ProcessIdentity> {
        Some(self.identity.clone())
    }

    fn has_exclusive_session(&self, _: &ProcessIdentity, _: Agent) -> bool {
        !self.shared
    }
}

type AfterPost = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Sink(Mutex<Vec<Value>>, Mutex<Option<AfterPost>>);

impl HttpClient for Sink {
    fn post_json(&self, _: &str, body: &[u8]) -> Result<u16, String> {
        self.0
            .lock()
            .unwrap()
            .push(serde_json::from_slice(body).unwrap());
        let after = self.1.lock().unwrap().take();
        if let Some(after) = after {
            after();
        }
        Ok(200)
    }
}

struct Fixture {
    root: PathBuf,
    store: StatusStore,
    consumers: ConsumerStore,
    owner: Owner,
    sink: Sink,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "hls-lifecycle-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed),
        ));
        let store = StatusStore::open(&root).unwrap();
        let consumers = ConsumerStore::open(&root).unwrap();
        consumers
            .register(&Consumer {
                name: "monitor".into(),
                protocol: 1,
                capabilities: vec![Capability::Status],
                sink: Some("http://127.0.0.1:7483/hook".into()),
            })
            .unwrap();
        Self {
            root,
            store,
            consumers,
            owner: Owner {
                identity: ProcessIdentity {
                    pid: 42,
                    started_at: "2026-09-18T00:00:00Z".into(),
                    host: "test-host".into(),
                },
                shared: false,
            },
            sink: Sink::default(),
        }
    }

    fn send(&self, agent: Agent, event: &str, payload: &Value) {
        self.send_in(agent, event, payload, &HookEnvironment::default());
    }

    fn send_in(&self, agent: Agent, event: &str, payload: &Value, env: &HookEnvironment) {
        let outcome = handle_ingest(
            &IngestContext {
                store: &self.store,
                consumers: Some(&self.consumers),
                liveness: &self.owner,
                http_client: &self.sink,
            },
            agent,
            event,
            &payload.to_string(),
            env,
            100,
        );
        assert!(
            outcome.problem.is_none(),
            "{agent:?} {event}: {:?}",
            outcome.problem
        );
    }

    fn ids(&self) -> Vec<String> {
        let mut ids: Vec<_> = self
            .store
            .running(&self.owner)
            .unwrap()
            .into_iter()
            .map(|s| s.session.id)
            .collect();
        ids.sort();
        ids
    }

    fn bodies(&self) -> Vec<Value> {
        self.sink.0.lock().unwrap().clone()
    }

    fn records(&self) -> Vec<(PathBuf, Value)> {
        std::fs::read_dir(self.root.join("status"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                (path, value)
            })
            .collect()
    }

    fn record(&self, id: &str) -> Value {
        self.records()
            .into_iter()
            .find(|(_, value)| value["session"]["id"] == id)
            .unwrap()
            .1
    }

    fn seed(&self, agent: Agent, id: &str, metadata: &Value) {
        let event = normalize(
            agent,
            events(agent).1,
            &json!({"session_id": id}).to_string(),
            &HookEnvironment::default(),
            Some(self.owner.identity.clone()),
        )
        .unwrap()
        .into_event()
        .unwrap();
        let mut stored = serde_json::to_value(&event).unwrap();
        stored
            .as_object_mut()
            .unwrap()
            .extend(metadata.as_object().unwrap().clone());
        std::fs::write(
            self.root
                .join("status")
                .join(format!("{}.json", event.binding_id)),
            serde_json::to_vec(&stored).unwrap(),
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

const fn events(agent: Agent) -> (&'static str, &'static str, &'static str) {
    match agent {
        Agent::Pi => ("session_start", "agent_start", "session_shutdown"),
        Agent::Opencode => (
            "tui.session.select",
            "session.status.busy",
            "session.deleted",
        ),
        _ => ("SessionStart", "PreToolUse", "SessionEnd"),
    }
}

fn start(agent: Agent, id: &str, source: &str) -> Value {
    match agent {
        Agent::Pi => json!({"session_id": id, "reason": source}),
        Agent::Opencode => json!({"session_id": id}),
        _ => json!({"session_id": id, "source": source}),
    }
}

#[test]
fn exclusive_agents_replace_foreground_sessions_and_can_resume_them() {
    for agent in Agent::ALL
        .into_iter()
        .filter(|agent| !matches!(agent, Agent::Opencode | Agent::Droid))
    {
        let mut fixture = Fixture::new();
        let (begin, activity, end) = events(agent);
        fixture.send(agent, begin, &start(agent, "a", "startup"));
        fixture.send(agent, activity, &json!({"session_id": "a"}));
        fixture.send(agent, begin, &start(agent, "b", "new"));
        fixture.send(agent, activity, &json!({"session_id": "b"}));
        assert_eq!(fixture.ids(), ["b"], "{agent:?}");
        let bodies = fixture.bodies();
        assert_eq!(bodies.len(), 5, "{agent:?}");
        assert_eq!(bodies[3]["event"], "swept");
        assert_eq!(bodies[3]["session"]["id"], "a");
        assert_eq!(bodies[3]["running"], false);

        fixture.store = StatusStore::open(&fixture.root).unwrap();
        fixture.send(agent, activity, &json!({"session_id": "a"}));
        fixture.send(agent, end, &json!({"session_id": "a"}));
        fixture.send(agent, begin, &start(agent, "a", "startup"));
        assert_eq!(fixture.bodies(), bodies, "{agent:?}");
        assert_eq!(fixture.ids(), ["b"]);

        fixture.send(agent, begin, &start(agent, "a", "resume"));
        fixture.send(agent, activity, &json!({"session_id": "a"}));
        let before = fixture.bodies();
        fixture.send(agent, activity, &json!({"session_id": "b"}));
        assert_eq!(fixture.bodies(), before);
        assert_eq!(fixture.ids(), ["a"], "{agent:?}");
        assert_eq!(
            fixture.store.running(&fixture.owner).unwrap()[0].phase,
            Phase::Working
        );
    }
}

#[test]
fn every_agent_ignores_activity_after_its_session_ends() {
    for agent in Agent::ALL {
        let fixture = Fixture::new();
        let (begin, activity, end) = events(agent);
        fixture.send(agent, begin, &start(agent, "a", "startup"));
        fixture.send(agent, end, &json!({"session_id": "a"}));
        let before = fixture.bodies();
        assert_eq!(before.len(), 2);
        assert_eq!(before[1]["running"], false);
        fixture.send(agent, activity, &json!({"session_id": "a"}));
        assert_eq!(fixture.bodies(), before, "{agent:?}");
        assert_eq!(fixture.ids(), Vec::<String>::new());
    }
}

#[test]
fn foreground_activity_replaces_legacy_records_when_start_was_missed() {
    for agent in Agent::ALL
        .into_iter()
        .filter(|agent| !matches!(agent, Agent::Opencode | Agent::Droid))
    {
        let fixture = Fixture::new();
        fixture.seed(agent, "a", &json!({}));
        fixture.send(agent, events(agent).1, &json!({"session_id": "b"}));
        assert_eq!(fixture.ids(), ["b"], "{agent:?}");
        assert_eq!(fixture.bodies()[1]["session"]["id"], "a");
        assert_eq!(fixture.bodies()[1]["running"], false);
    }
}

#[test]
fn agents_with_the_same_process_and_session_ids_stay_independent() {
    let fixture = Fixture::new();
    for agent in Agent::ALL {
        fixture.send(agent, events(agent).0, &start(agent, "a", "startup"));
    }
    let mut expected_count = Agent::ALL.len();
    for agent in Agent::ALL {
        fixture.send(agent, events(agent).0, &start(agent, "b", "new"));
        let sessions = fixture.store.running(&fixture.owner).unwrap();
        let expected = if matches!(agent, Agent::Opencode | Agent::Droid) {
            expected_count += 1;
            vec!["a", "b"]
        } else {
            vec!["b"]
        };
        assert_eq!(sessions.len(), expected_count);
        let mut ids: Vec<_> = sessions
            .iter()
            .filter(|session| session.agent == agent)
            .map(|session| session.session.id.as_str())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, expected, "{agent:?}");
    }
}

#[test]
fn codex_children_cannot_replace_or_reclassify_the_parent() {
    let fixture = Fixture::new();
    fixture.send(
        Agent::Codex,
        "SessionStart",
        &start(Agent::Codex, "a", "startup"),
    );
    fixture.send(
        Agent::Codex,
        "PreToolUse",
        &json!({"session_id": "a", "agent_id": "worker"}),
    );
    fixture.send(
        Agent::Codex,
        "PreToolUse",
        &json!({"session_id": "child", "agent_id": "worker"}),
    );
    fixture.send(
        Agent::Codex,
        "SessionStart",
        &start(Agent::Codex, "b", "fork"),
    );
    fixture.send(Agent::Codex, "PreToolUse", &json!({"session_id": "child"}));
    assert_eq!(fixture.ids(), ["b", "child"]);
    let before = fixture.bodies();
    fixture.send(
        Agent::Codex,
        "SessionStart",
        &json!({"session_id": "a", "source": "resume", "agent_id": "worker"}),
    );
    assert_eq!(fixture.bodies(), before);
}

#[test]
fn pi_forks_and_qwen_branches_replace_the_foreground() {
    for (agent, source) in [(Agent::Pi, "fork"), (Agent::Qwen, "branch")] {
        let fixture = Fixture::new();
        fixture.send(agent, events(agent).0, &start(agent, "a", "startup"));
        fixture.send(agent, events(agent).0, &start(agent, "b", source));
        assert_eq!(fixture.ids(), ["b"], "{agent:?}");
    }
}

#[test]
fn opencode_preserves_independent_sessions_and_selection_preserves_phase() {
    let fixture = Fixture::new();
    fixture.seed(Agent::Opencode, "legacy", &json!({}));
    for id in ["root", "child", "selected-a"] {
        fixture.send(
            Agent::Opencode,
            "session.status.busy",
            &json!({"session_id": id}),
        );
    }
    fixture.send(
        Agent::Opencode,
        "permission.asked",
        &json!({"session_id": "selected-a"}),
    );
    fixture.send(
        Agent::Opencode,
        "tui.session.select",
        &json!({"session_id": "selected-a"}),
    );
    let sessions = fixture.store.running(&fixture.owner).unwrap();
    assert_eq!(
        sessions
            .iter()
            .find(|s| s.session.id == "selected-a")
            .unwrap()
            .phase,
        Phase::Permission
    );
    assert_eq!(fixture.bodies().last().unwrap()["phase"], "permission");
    fixture.send(
        Agent::Opencode,
        "tui.session.select",
        &json!({"session_id": "selected-b"}),
    );
    fixture.send(
        Agent::Opencode,
        "session.status.busy",
        &json!({"session_id": "selected-a"}),
    );
    assert_eq!(
        fixture.ids(),
        ["child", "legacy", "root", "selected-a", "selected-b"]
    );
    assert_eq!(
        fixture.bodies().last().unwrap()["session"]["id"],
        "selected-a"
    );
    assert_eq!(fixture.bodies().last().unwrap()["phase"], "working");
    fixture.send(
        Agent::Opencode,
        "permission.asked",
        &json!({"session_id": "selected-a"}),
    );
    let bodies = fixture.bodies();
    assert_eq!(bodies.len(), 8);
    assert!(bodies.iter().all(|body| body["running"] == true));
    assert_eq!(bodies.last().unwrap()["session"]["id"], "selected-a");
    assert_eq!(bodies.last().unwrap()["phase"], "permission");
    fixture.send(
        Agent::Opencode,
        "session.deleted",
        &json!({"session_id": "selected-a"}),
    );
    assert_eq!(fixture.ids(), ["child", "legacy", "root", "selected-b"]);
    assert_eq!(
        fixture.bodies().last().unwrap()["session"]["id"],
        "selected-a"
    );
    assert_eq!(fixture.bodies().last().unwrap()["running"], false);
}

#[test]
fn shared_processes_keep_root_sessions_independent_including_on_resume() {
    for agent in [Agent::Codex, Agent::Qwen, Agent::Kimi] {
        let mut fixture = Fixture::new();
        fixture.owner.shared = true;
        for id in ["a", "b"] {
            fixture.send(agent, events(agent).0, &start(agent, id, "startup"));
        }
        fixture.send(agent, events(agent).2, &json!({"session_id": "a"}));
        fixture.send(agent, events(agent).0, &start(agent, "a", "resume"));
        assert_eq!(fixture.ids(), ["a", "b"], "{agent:?}");
    }
}

#[test]
fn qwen_attributed_sessions_preserve_other_channels() {
    let fixture = Fixture::new();
    for id in ["a", "b"] {
        fixture.send(Agent::Qwen, "SessionStart", &json!({"session_id": id, "source": "startup", "source_type": "channel", "source_id": id}));
    }
    fixture.send(Agent::Qwen, "SessionEnd", &json!({"session_id": "a"}));
    fixture.send(
        Agent::Qwen,
        "SessionStart",
        &json!({"session_id": "a", "source": "resume", "source_type": "channel", "source_id": "a"}),
    );
    assert_eq!(fixture.ids(), ["a", "b"]);
}

#[test]
fn legacy_claude_lifecycle_markers_survive_the_upgrade() {
    let fixture = Fixture::new();
    fixture.seed(
        Agent::Claude,
        "retired",
        &json!({"claudeRetired": true, "running": false}),
    );
    fixture.seed(Agent::Claude, "fork", &json!({"claudeParallel": true}));
    fixture.send(
        Agent::Claude,
        "PreToolUse",
        &json!({"session_id": "retired"}),
    );
    assert_eq!(fixture.bodies(), Vec::<Value>::new());
    fixture.send(
        Agent::Claude,
        "SessionStart",
        &start(Agent::Claude, "foreground", "startup"),
    );
    assert_eq!(fixture.ids(), ["foreground", "fork"]);
    fixture.send(
        Agent::Claude,
        "SessionStart",
        &start(Agent::Claude, "retired", "resume"),
    );
    assert_eq!(fixture.ids(), ["fork", "retired"]);
}

#[test]
fn a_wrongly_typed_lifecycle_hint_still_records_the_session() {
    for (agent, field) in [(Agent::Codex, "source"), (Agent::Pi, "reason")] {
        let fixture = Fixture::new();
        fixture.send(agent, events(agent).0, &start(agent, "a", "startup"));
        let before = fixture.bodies();
        let mut payload = json!({"session_id": "b"});
        payload[field] = json!({"private": "payload"});
        let outcome = handle_ingest(
            &IngestContext {
                store: &fixture.store,
                consumers: Some(&fixture.consumers),
                liveness: &fixture.owner,
                http_client: &fixture.sink,
            },
            agent,
            events(agent).0,
            &payload.to_string(),
            &HookEnvironment::default(),
            100,
        );
        // The hint only classifies the session's role, so a wrong type must not discard a
        // status event normalize already accepted.
        assert_eq!(outcome.problem, None, "{agent:?}");
        assert_eq!(fixture.ids(), ["b"], "{agent:?}");
        let delivered = fixture.bodies();
        assert!(delivered.len() > before.len(), "{agent:?}");
        for body in &delivered {
            let text = body.to_string();
            assert!(
                !text.contains("private"),
                "{agent:?} leaked a hint value: {text}"
            );
            assert!(
                !text.contains("payload"),
                "{agent:?} leaked a hint value: {text}"
            );
        }
    }
}

#[test]
fn lifecycle_metadata_stays_out_of_the_wire_protocol() {
    let fixture = Fixture::new();
    fixture.send(
        Agent::Codex,
        "PreToolUse",
        &json!({"session_id": "child", "agent_id": "worker"}),
    );
    let wire = fixture.bodies().pop().unwrap();
    let mut keys: Vec<_> = wire
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "agent",
            "bindingId",
            "event",
            "git",
            "observedAt",
            "phase",
            "process",
            "protocol",
            "remoteHost",
            "running",
            "session",
            "terminal",
            "tmux"
        ]
    );
    let _: StatusEvent = serde_json::from_value(wire).unwrap();
}

const WIRE_KEYS: [&str; 13] = [
    "agent",
    "bindingId",
    "event",
    "git",
    "observedAt",
    "phase",
    "process",
    "protocol",
    "remoteHost",
    "running",
    "session",
    "terminal",
    "tmux",
];

fn codex(fixture: &Fixture, event: &str, payload: &Value) {
    fixture.send(Agent::Codex, event, payload);
}

fn codex_started(fixture: &Fixture) {
    codex(
        fixture,
        "SessionStart",
        &start(Agent::Codex, "a", "startup"),
    );
}

fn last_body(fixture: &Fixture) -> Value {
    fixture.bodies().pop().unwrap()
}

#[test]
fn codex_child_activity_keeps_the_root_working_after_its_stop() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(&fixture, "UserPromptSubmit", &json!({"session_id": "a"}));
    codex(
        &fixture,
        "SubagentStart",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    codex(
        &fixture,
        "PreToolUse",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    codex(&fixture, "Stop", &json!({"session_id": "a"}));
    let last = last_body(&fixture);
    assert_eq!(last["session"]["id"], "a");
    assert_eq!(last["event"], "Stop");
    assert_eq!(last["phase"], "working");
    assert_eq!(fixture.ids(), ["a"]);
    for body in fixture.bodies() {
        let mut keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
        keys.sort_unstable();
        assert_eq!(keys, WIRE_KEYS);
    }
    assert_eq!(
        fixture.store.running(&fixture.owner).unwrap()[0].phase,
        Phase::Working
    );
}

#[test]
fn codex_child_events_without_a_start_change_nothing() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(&fixture, "Stop", &json!({"session_id": "a"}));
    let before = fixture.bodies();
    codex(
        &fixture,
        "PreToolUse",
        &json!({"session_id": "a", "agent_id": "g"}),
    );
    assert_eq!(fixture.bodies(), before);
    let record = fixture.record("a");
    assert_eq!(record["event"], "Stop");
    assert!(record.get("children").is_none());
}

#[test]
fn codex_child_permission_surfaces_on_the_root() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(
        &fixture,
        "SubagentStart",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    codex(
        &fixture,
        "PermissionRequest",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    let last = last_body(&fixture);
    assert_eq!(last["phase"], "permission");
    assert_eq!(last["event"], "child:PermissionRequest");
    assert_eq!(last["session"]["id"], "a");
}

#[test]
fn codex_untracked_child_permission_surfaces_and_clears() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(
        &fixture,
        "PermissionRequest",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    let permission = last_body(&fixture);
    assert_eq!(permission["phase"], "permission");
    assert_eq!(permission["event"], "child:PermissionRequest");
    assert_eq!(permission["session"]["id"], "a");

    codex(
        &fixture,
        "PostToolUse",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    let cleared = last_body(&fixture);
    assert_eq!(cleared["phase"], "idle");
    assert_eq!(cleared["event"], "child:PostToolUse");

    let record = fixture.record("a");
    assert!(record.get("children").is_none());
}

#[test]
fn codex_child_awaiting_user_surfaces_as_permission() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(
        &fixture,
        "SubagentStart",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    codex(
        &fixture,
        "PreToolUse",
        &json!({"session_id": "a", "agent_id": "w", "tool_name": "request_user_input"}),
    );
    let last = last_body(&fixture);
    assert_eq!(last["phase"], "permission");
    assert_eq!(last["event"], "child:PreToolUse");
}

#[test]
fn codex_root_interrupt_releases_children() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(
        &fixture,
        "SubagentStart",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    codex(&fixture, "Stop", &json!({"session_id": "a"}));
    assert_eq!(last_body(&fixture)["phase"], "working");
    codex(&fixture, "Interrupt", &json!({"session_id": "a"}));
    let last = last_body(&fixture);
    assert_eq!(last["event"], "Interrupt");
    assert_eq!(last["phase"], "idle");
    assert_eq!(
        fixture.record("a")["children"],
        json!({ crate::children::child_key("w"): "idle" })
    );
}

#[test]
fn root_session_start_clears_children() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(
        &fixture,
        "SubagentStart",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    codex(
        &fixture,
        "SessionStart",
        &start(Agent::Codex, "a", "resume"),
    );
    let last = last_body(&fixture);
    assert_eq!(last["event"], "SessionStart");
    assert_eq!(last["phase"], "idle");
    let record = fixture.record("a");
    assert_eq!(record["ownPhase"], "idle");
    assert!(record.get("children").is_none());
}

#[test]
fn retired_roots_keep_children_idle() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(
        &fixture,
        "SubagentStart",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    codex(&fixture, "SessionEnd", &json!({"session_id": "a"}));
    let last = last_body(&fixture);
    assert_eq!(last["running"], false);
    assert_eq!(last["phase"], "idle");
    let record = fixture.record("a");
    assert_eq!(record["retired"], true);
    assert_eq!(
        record["children"],
        json!({ crate::children::child_key("w"): "idle" })
    );

    let before = fixture.bodies();
    codex(
        &fixture,
        "PreToolUse",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    assert_eq!(fixture.bodies(), before);
}

#[test]
fn child_updates_that_change_nothing_are_not_forwarded() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(
        &fixture,
        "SubagentStart",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    let after_start = fixture.bodies();
    assert_eq!(after_start.len(), 2);
    assert_eq!(after_start[1]["event"], "child:SubagentStart");
    assert_eq!(after_start[1]["phase"], "working");
    for _ in 0..2 {
        codex(
            &fixture,
            "PreToolUse",
            &json!({"session_id": "a", "agent_id": "w"}),
        );
    }
    assert_eq!(fixture.bodies(), after_start);
}

#[test]
fn child_events_refresh_the_root_terminal_in_the_same_process() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(&fixture, "UserPromptSubmit", &json!({"session_id": "a"}));
    codex(
        &fixture,
        "SubagentStart",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    let env = HookEnvironment {
        terminal: Some(TerminalIdentity {
            session_id: Some("w1t0p0:moved".into()),
            terminal_type: Some("iterm2".into()),
            kitty_listen_on: None,
            kitty_pid: None,
        }),
        ..HookEnvironment::default()
    };
    fixture.send_in(
        Agent::Codex,
        "PreToolUse",
        &json!({"session_id": "a", "agent_id": "w"}),
        &env,
    );
    let last = last_body(&fixture);
    assert_eq!(last["event"], "child:PreToolUse");
    assert_eq!(last["phase"], "working");
    assert_eq!(last["terminal"]["sessionId"], "w1t0p0:moved");
    assert_eq!(fixture.record("a")["terminal"]["sessionId"], "w1t0p0:moved");
}

#[test]
fn delivery_converges_on_the_stored_aggregate() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(&fixture, "UserPromptSubmit", &json!({"session_id": "a"}));
    let (path, _) = fixture
        .records()
        .into_iter()
        .find(|(_, value)| value["session"]["id"] == "a")
        .unwrap();
    // Stands in for a concurrent child start that stored a newer aggregate during our fan-out.
    *fixture.sink.1.lock().unwrap() = Some(Box::new(move || {
        let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record["event"] = json!("child:SubagentStart");
        record["phase"] = json!("working");
        record["children"] = json!({ crate::children::child_key("w"): "working" });
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    }));
    codex(&fixture, "Stop", &json!({"session_id": "a"}));
    let bodies = fixture.bodies();
    assert_eq!(bodies.len(), 4);
    assert_eq!(bodies[2]["event"], "Stop");
    assert_eq!(bodies[2]["phase"], "idle");
    assert_eq!(bodies[3]["event"], "resend:child:SubagentStart");
    assert_eq!(bodies[3]["phase"], "working");
    assert_eq!(bodies[3]["bindingId"], bodies[2]["bindingId"]);
}

#[test]
fn convergence_resends_a_removal_when_the_record_vanishes() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    let (path, _) = fixture
        .records()
        .into_iter()
        .find(|(_, value)| value["session"]["id"] == "a")
        .unwrap();
    // Stands in for a concurrent SessionEnd with no process that removed the record during our fan-out.
    *fixture.sink.1.lock().unwrap() = Some(Box::new(move || {
        std::fs::remove_file(&path).unwrap();
    }));
    codex(&fixture, "UserPromptSubmit", &json!({"session_id": "a"}));
    let bodies = fixture.bodies();
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies[1]["event"], "UserPromptSubmit");
    assert_eq!(bodies[1]["running"], true);
    assert_eq!(bodies[2]["event"], "UserPromptSubmit");
    assert_eq!(bodies[2]["running"], false);
    assert_eq!(bodies[2]["bindingId"], bodies[1]["bindingId"]);
}

#[test]
fn convergence_resends_when_only_running_differs() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(&fixture, "UserPromptSubmit", &json!({"session_id": "a"}));
    let (path, _) = fixture
        .records()
        .into_iter()
        .find(|(_, value)| value["session"]["id"] == "a")
        .unwrap();
    // Stands in for a concurrent retirement that stored the same phase but stopped running.
    *fixture.sink.1.lock().unwrap() = Some(Box::new(move || {
        let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record["running"] = json!(false);
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    }));
    codex(&fixture, "PreToolUse", &json!({"session_id": "a"}));
    let bodies = fixture.bodies();
    assert_eq!(bodies.len(), 4);
    assert_eq!(bodies[2]["event"], "PreToolUse");
    assert_eq!(bodies[2]["phase"], "working");
    assert_eq!(bodies[2]["running"], true);
    assert_eq!(bodies[3]["event"], "resend:PreToolUse");
    assert_eq!(bodies[3]["phase"], "working");
    assert_eq!(bodies[3]["running"], false);
    assert_eq!(bodies[3]["bindingId"], bodies[2]["bindingId"]);
}

#[test]
fn convergence_resend_of_a_user_prompt_submit_is_prefixed() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(&fixture, "UserPromptSubmit", &json!({"session_id": "a"}));
    let (path, _) = fixture
        .records()
        .into_iter()
        .find(|(_, value)| value["session"]["id"] == "a")
        .unwrap();
    // Stands in for a concurrent retirement that stored the same phase but stopped running.
    *fixture.sink.1.lock().unwrap() = Some(Box::new(move || {
        let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record["running"] = json!(false);
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    }));
    codex(&fixture, "UserPromptSubmit", &json!({"session_id": "a"}));
    let bodies = fixture.bodies();
    assert_eq!(bodies.len(), 4);
    assert_eq!(bodies[2]["event"], "UserPromptSubmit");
    assert_eq!(bodies[2]["running"], true);
    assert_eq!(bodies[3]["event"], "resend:UserPromptSubmit");
    assert_eq!(bodies[3]["running"], false);
    assert_eq!(bodies[3]["bindingId"], bodies[2]["bindingId"]);
}

#[test]
fn codex_subagent_lifecycle_tracks_each_child() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(&fixture, "UserPromptSubmit", &json!({"session_id": "a"}));
    codex(
        &fixture,
        "SubagentStart",
        &json!({"session_id": "a", "agent_id": "A", "agent_type": "worker"}),
    );
    assert_eq!(
        fixture.record("a")["children"][crate::children::child_key("A")],
        json!("working")
    );

    codex(&fixture, "Stop", &json!({"session_id": "a"}));
    assert_eq!(last_body(&fixture)["phase"], "working");

    codex(
        &fixture,
        "SubagentStop",
        &json!({"session_id": "a", "agent_id": "A"}),
    );
    assert_eq!(last_body(&fixture)["phase"], "idle");

    codex(
        &fixture,
        "PreToolUse",
        &json!({"session_id": "a", "agent_id": "A"}),
    );
    assert_eq!(last_body(&fixture)["phase"], "working");

    codex(
        &fixture,
        "SubagentStart",
        &json!({"session_id": "a", "agent_id": "B"}),
    );
    codex(
        &fixture,
        "PermissionRequest",
        &json!({"session_id": "a", "agent_id": "B"}),
    );
    assert_eq!(last_body(&fixture)["phase"], "permission");

    codex(&fixture, "Interrupt", &json!({"session_id": "a"}));
    assert_eq!(last_body(&fixture)["phase"], "idle");

    assert_eq!(fixture.ids(), ["a"]);
    for body in fixture.bodies() {
        assert_eq!(body["session"]["id"], "a");
    }

    codex(&fixture, "SessionEnd", &json!({"session_id": "a"}));
    let ended = last_body(&fixture);
    assert_eq!(ended["running"], false);
    assert_eq!(ended["session"]["id"], "a");
    assert!(fixture.ids().is_empty());
}

#[test]
fn codex_last_subagent_stop_under_an_idle_root_settles_idle() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(&fixture, "UserPromptSubmit", &json!({"session_id": "a"}));
    codex(
        &fixture,
        "SubagentStart",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    codex(&fixture, "Stop", &json!({"session_id": "a"}));
    assert_eq!(last_body(&fixture)["phase"], "working");
    codex(
        &fixture,
        "SubagentStop",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    let last = last_body(&fixture);
    assert_eq!(last["event"], "child:SubagentStop");
    assert_eq!(last["phase"], "idle");
    assert_eq!(fixture.record("a")["held"], Value::Null);
}

#[test]
fn codex_compaction_restart_keeps_children() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(&fixture, "UserPromptSubmit", &json!({"session_id": "a"}));
    codex(
        &fixture,
        "SubagentStart",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    codex(
        &fixture,
        "SessionStart",
        &start(Agent::Codex, "a", "compact"),
    );
    codex(&fixture, "Stop", &json!({"session_id": "a"}));
    let last = last_body(&fixture);
    assert_eq!(last["event"], "Stop");
    assert_eq!(last["phase"], "working");
    assert_eq!(
        fixture.record("a")["children"],
        json!({ crate::children::child_key("w"): "working" })
    );
}

#[test]
fn superseded_roots_retire_with_their_children_idle() {
    let fixture = Fixture::new();
    codex_started(&fixture);
    codex(
        &fixture,
        "SubagentStart",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    assert_eq!(last_body(&fixture)["phase"], "working");
    codex(&fixture, "SessionStart", &start(Agent::Codex, "b", "fork"));
    let retired = fixture
        .bodies()
        .into_iter()
        .find(|body| body["session"]["id"] == "a" && body["running"] == false)
        .expect("the superseded root is reported retired");
    assert_eq!(retired["event"], "swept");
    assert_eq!(retired["phase"], "idle");
    let record = fixture.record("a");
    assert_eq!(record["retired"], true);
    assert_eq!(record["phase"], "idle");
    assert_eq!(
        record["children"],
        json!({ crate::children::child_key("w"): "idle" })
    );
}

#[test]
fn codex_child_without_a_root_record_tracks_that_child() {
    let fixture = Fixture::new();
    codex(
        &fixture,
        "SubagentStart",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    let started = last_body(&fixture);
    assert_eq!(started["session"]["id"], "a");
    assert_eq!(started["phase"], "working");
    assert_eq!(fixture.record("a")["ownPhase"], "idle");

    codex(
        &fixture,
        "PermissionRequest",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    assert_eq!(last_body(&fixture)["phase"], "permission");

    codex(
        &fixture,
        "SubagentStop",
        &json!({"session_id": "a", "agent_id": "w"}),
    );
    let stopped = last_body(&fixture);
    assert_eq!(stopped["event"], "child:SubagentStop");
    assert_eq!(stopped["phase"], "idle");
    assert_eq!(fixture.ids(), ["a"]);
}

fn droid(fixture: &Fixture, event: &str, payload: &Value) {
    fixture.send(Agent::Droid, event, payload);
}

fn droid_spawn(id: &str, calling_session_id: &str, source: &str) -> Value {
    json!({"session_id": id, "source": source, "calling_session_id": calling_session_id})
}

#[test]
fn droid_task_children_report_through_the_parent() {
    let fixture = Fixture::new();
    droid(
        &fixture,
        "SessionStart",
        &start(Agent::Droid, "P", "startup"),
    );
    assert_eq!(last_body(&fixture)["phase"], "idle");

    droid(&fixture, "UserPromptSubmit", &json!({"session_id": "P"}));
    assert_eq!(last_body(&fixture)["phase"], "working");

    droid(&fixture, "SessionStart", &droid_spawn("C1", "P", "startup"));
    droid(&fixture, "SessionStart", &droid_spawn("C2", "P", "startup"));
    droid(&fixture, "SessionStart", &droid_spawn("C2", "P", "resume"));
    assert_eq!(fixture.record("P")["phase"], "working");
    assert_eq!(
        fixture.record("P")["children"].as_object().unwrap().len(),
        2
    );

    droid(&fixture, "UserPromptSubmit", &json!({"session_id": "C1"}));
    assert_eq!(
        fixture.record("P")["children"][crate::children::child_key("C1")],
        json!("working")
    );

    droid(&fixture, "Stop", &json!({"session_id": "P"}));
    let stop_p = last_body(&fixture);
    assert_eq!(stop_p["event"], "Stop");
    assert_eq!(stop_p["phase"], "working");

    droid(&fixture, "Stop", &json!({"session_id": "C1"}));
    assert_eq!(
        fixture.record("P")["children"][crate::children::child_key("C1")],
        json!("idle")
    );
    assert_eq!(fixture.record("P")["phase"], "working");

    let before = fixture.bodies().len();
    droid(&fixture, "Stop", &json!({"session_id": "C2"}));
    assert_eq!(fixture.bodies().len(), before);
    let both_stopped = fixture.record("P");
    assert_eq!(both_stopped["event"], "child:Stop");
    assert_eq!(both_stopped["phase"], "working");
    assert_eq!(both_stopped["held"], true);

    droid(
        &fixture,
        "PreToolUse",
        &json!({"session_id": "C1", "tool_name": "x"}),
    );
    assert_eq!(fixture.bodies().len(), before);
    assert_eq!(
        fixture.record("P")["children"][crate::children::child_key("C1")],
        json!("working")
    );

    droid(
        &fixture,
        "Notification",
        &json!({"session_id": "C1", "notification_type": "permission_prompt"}),
    );
    let permission = last_body(&fixture);
    assert_eq!(permission["event"], "child:Notification");
    assert_eq!(permission["phase"], "permission");

    for body in fixture.bodies() {
        assert_eq!(body["session"]["id"], "P");
    }
    assert_eq!(fixture.ids(), ["P"]);

    droid(&fixture, "SessionEnd", &json!({"session_id": "P"}));
    let ended = last_body(&fixture);
    assert_eq!(ended["running"], false);
    assert_eq!(ended["session"]["id"], "P");
    assert!(fixture.ids().is_empty());
}

fn attributed(id: &str, source: Option<&str>) -> Value {
    json!({"session_id": id, "source": source, "source_type": "cli"})
}

fn hold_droid_parent(fixture: &Fixture) {
    droid(fixture, "SessionStart", &attributed("P", Some("startup")));
    droid(fixture, "UserPromptSubmit", &attributed("P", None));
    droid(fixture, "SessionStart", &droid_spawn("C1", "P", "startup"));
    droid(fixture, "SessionStart", &droid_spawn("C2", "P", "startup"));
    droid(fixture, "Stop", &attributed("P", None));
    droid(fixture, "Stop", &json!({"session_id": "C1"}));
    droid(fixture, "Stop", &json!({"session_id": "C2"}));
    assert_eq!(fixture.record("P")["held"], true);
}

#[test]
fn droid_background_completion_holds_the_parent_until_its_wake() {
    let fixture = Fixture::new();
    droid(
        &fixture,
        "SessionStart",
        &start(Agent::Droid, "P", "startup"),
    );
    droid(&fixture, "UserPromptSubmit", &json!({"session_id": "P"}));
    droid(&fixture, "SessionStart", &droid_spawn("C1", "P", "startup"));
    droid(&fixture, "SessionStart", &droid_spawn("C2", "P", "startup"));
    droid(&fixture, "Stop", &json!({"session_id": "P"}));
    let parent_stop = last_body(&fixture);
    assert_eq!(parent_stop["event"], "Stop");
    assert_eq!(parent_stop["phase"], "working");

    droid(&fixture, "Stop", &json!({"session_id": "C1"}));
    assert_eq!(fixture.record("P")["held"], Value::Null);

    let before = fixture.bodies().len();
    droid(&fixture, "Stop", &json!({"session_id": "C2"}));
    assert_eq!(fixture.bodies().len(), before);
    let held = fixture.record("P");
    assert_eq!(held["held"], true);
    assert_eq!(held["ownPhase"], "idle");
    assert_eq!(held["phase"], "working");
    let listed = serde_json::to_value(
        fixture
            .store
            .sessions_envelope(&fixture.owner)
            .sessions
            .pop()
            .unwrap(),
    )
    .unwrap();
    let mut keys: Vec<_> = listed.as_object().unwrap().keys().cloned().collect();
    keys.sort_unstable();
    assert_eq!(keys, WIRE_KEYS);
    assert_eq!(listed["phase"], "working");

    droid(
        &fixture,
        "UserPromptSubmit",
        &json!({"session_id": "P", "prompt": "Background task completed"}),
    );
    let woken = last_body(&fixture);
    assert_eq!(woken["event"], "UserPromptSubmit");
    assert_eq!(woken["phase"], "working");
    assert_eq!(fixture.record("P")["held"], Value::Null);

    droid(&fixture, "Stop", &json!({"session_id": "P"}));
    let settled = last_body(&fixture);
    assert_eq!(settled["event"], "Stop");
    assert_eq!(settled["phase"], "idle");

    for body in fixture.bodies() {
        let mut keys: Vec<_> = body.as_object().unwrap().keys().cloned().collect();
        keys.sort_unstable();
        assert_eq!(keys, WIRE_KEYS);
    }
}

#[test]
fn a_droid_child_that_ends_without_its_stop_releases_the_parent() {
    for (event, payload) in [
        ("SessionEnd", json!({"session_id": "C2"})),
        (
            "Notification",
            json!({"session_id": "C2", "notification_type": "idle_prompt"}),
        ),
    ] {
        let fixture = Fixture::new();
        droid(&fixture, "SessionStart", &attributed("P", Some("startup")));
        droid(&fixture, "UserPromptSubmit", &attributed("P", None));
        droid(&fixture, "SessionStart", &droid_spawn("C1", "P", "startup"));
        droid(&fixture, "SessionStart", &droid_spawn("C2", "P", "startup"));
        droid(&fixture, "Stop", &attributed("P", None));
        droid(&fixture, "Stop", &json!({"session_id": "C1"}));
        droid(&fixture, event, &payload);
        let released = last_body(&fixture);
        assert_eq!(released["event"], format!("child:{event}"));
        assert_eq!(released["phase"], "idle", "{event}");
    }
}

#[test]
fn a_droid_child_finishing_mid_turn_lets_the_parents_stop_settle() {
    let fixture = Fixture::new();
    droid(&fixture, "SessionStart", &attributed("P", Some("startup")));
    droid(&fixture, "UserPromptSubmit", &attributed("P", None));
    droid(&fixture, "SessionStart", &droid_spawn("C1", "P", "startup"));
    droid(&fixture, "UserPromptSubmit", &json!({"session_id": "C1"}));
    droid(&fixture, "Stop", &json!({"session_id": "C1"}));
    let mid_turn = fixture.record("P");
    assert_eq!(mid_turn["event"], "child:Stop");
    assert_eq!(mid_turn["phase"], "working");
    assert_eq!(mid_turn["held"], Value::Null);

    droid(&fixture, "Stop", &attributed("P", None));
    let settled = last_body(&fixture);
    assert_eq!(settled["event"], "Stop");
    assert_eq!(settled["phase"], "idle");
    assert_eq!(fixture.record("P")["held"], Value::Null);
}

#[test]
fn a_child_permission_outranks_a_held_parent() {
    let fixture = Fixture::new();
    hold_droid_parent(&fixture);
    droid(
        &fixture,
        "Notification",
        &json!({"session_id": "C1", "notification_type": "permission_prompt"}),
    );
    let permission = last_body(&fixture);
    assert_eq!(permission["event"], "child:Notification");
    assert_eq!(permission["phase"], "permission");

    droid(&fixture, "Stop", &json!({"session_id": "C1"}));
    let answered = last_body(&fixture);
    assert_eq!(answered["event"], "child:Stop");
    assert_eq!(answered["phase"], "working");
    assert_eq!(fixture.record("P")["held"], true);
}

#[test]
fn the_parents_own_lifecycle_events_release_its_hold() {
    let restarted = Fixture::new();
    hold_droid_parent(&restarted);
    droid(&restarted, "SessionStart", &attributed("P", Some("resume")));
    assert_eq!(last_body(&restarted)["event"], "SessionStart");
    assert_eq!(last_body(&restarted)["phase"], "idle");
    assert_eq!(restarted.record("P")["held"], Value::Null);

    let ended = Fixture::new();
    hold_droid_parent(&ended);
    droid(&ended, "SessionEnd", &attributed("P", None));
    let removal = last_body(&ended);
    assert_eq!(removal["running"], false);
    assert_eq!(removal["phase"], "idle");
    let record = ended.record("P");
    assert_eq!(record["retired"], true);
    assert_eq!(record["held"], Value::Null);

    let superseded = Fixture::new();
    hold_droid_parent(&superseded);
    droid(
        &superseded,
        "SessionStart",
        &attributed("Q", Some("startup")),
    );
    let retired = superseded
        .bodies()
        .into_iter()
        .find(|body| body["session"]["id"] == "P" && body["running"] == false)
        .expect("the superseded parent is reported retired");
    assert_eq!(retired["phase"], "idle");
    let record = superseded.record("P");
    assert_eq!(record["retired"], true);
    assert_eq!(record["held"], Value::Null);
}

#[test]
fn droid_child_without_a_known_parent_keeps_its_own_session() {
    let fixture = Fixture::new();
    droid(
        &fixture,
        "SessionStart",
        &droid_spawn("c", "ghost", "startup"),
    );
    assert_eq!(fixture.ids(), ["c"]);
    assert_eq!(last_body(&fixture)["phase"], "idle");
}

#[test]
fn droid_session_start_without_calling_session_is_a_root() {
    let fixture = Fixture::new();
    droid(
        &fixture,
        "SessionStart",
        &start(Agent::Droid, "P", "startup"),
    );
    droid(&fixture, "SessionStart", &droid_spawn("C", "P", "startup"));
    droid(
        &fixture,
        "SessionStart",
        &start(Agent::Droid, "C", "resume"),
    );
    assert_eq!(fixture.ids(), ["C", "P"]);

    droid(
        &fixture,
        "PreToolUse",
        &json!({"session_id": "C", "tool_name": "x"}),
    );
    assert_eq!(fixture.record("C")["phase"], "working");
    assert_eq!(last_body(&fixture)["session"]["id"], "C");

    droid(&fixture, "UserPromptSubmit", &json!({"session_id": "P"}));
    let before = fixture.bodies().len();
    droid(&fixture, "SessionStart", &droid_spawn("C", "P", "startup"));
    let bodies = fixture.bodies().split_off(before);
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["session"]["id"], "P");
    assert_eq!(bodies[0]["event"], "child:SessionStart");
    assert_eq!(bodies[0]["running"], true);
    assert_eq!(bodies[1]["session"]["id"], "C");
    assert_eq!(bodies[1]["event"], "swept");
    assert_eq!(fixture.ids(), ["P"]);
}

#[test]
fn droid_child_of_an_ended_parent_is_ignored() {
    let fixture = Fixture::new();
    droid(
        &fixture,
        "SessionStart",
        &start(Agent::Droid, "P", "startup"),
    );
    droid(&fixture, "SessionStart", &droid_spawn("C", "P", "startup"));
    droid(&fixture, "SessionEnd", &json!({"session_id": "P"}));
    let before = fixture.bodies();
    droid(
        &fixture,
        "PreToolUse",
        &json!({"session_id": "C", "tool_name": "x"}),
    );
    assert_eq!(fixture.bodies(), before);
    assert!(
        fixture
            .records()
            .into_iter()
            .all(|(_, value)| value["session"]["id"] != "C")
    );
}

#[test]
fn droid_grandchildren_attach_to_the_root() {
    let fixture = Fixture::new();
    droid(
        &fixture,
        "SessionStart",
        &start(Agent::Droid, "P", "startup"),
    );
    droid(&fixture, "SessionStart", &droid_spawn("C", "P", "startup"));
    assert_eq!(last_body(&fixture)["event"], "child:SessionStart");
    assert_eq!(last_body(&fixture)["session"]["id"], "P");
    droid(&fixture, "SessionStart", &droid_spawn("G", "C", "startup"));
    let children = fixture.record("P")["children"].clone();
    assert_eq!(children[crate::children::child_key("C")], json!("working"));
    assert_eq!(children[crate::children::child_key("G")], json!("working"));
    assert_eq!(fixture.ids(), ["P"]);
}

#[test]
fn droid_child_event_before_its_session_start_leaves_no_stray_row() {
    let fixture = Fixture::new();
    droid(
        &fixture,
        "SessionStart",
        &start(Agent::Droid, "P", "startup"),
    );
    droid(
        &fixture,
        "PreToolUse",
        &json!({"session_id": "C", "tool_name": "x"}),
    );
    assert_eq!(fixture.ids(), ["C", "P"]);
    droid(&fixture, "SessionStart", &droid_spawn("C", "P", "startup"));
    assert_eq!(fixture.ids(), ["P"]);
    assert_eq!(
        fixture.record("P")["children"][crate::children::child_key("C")],
        json!("working")
    );
    let removal = fixture
        .bodies()
        .into_iter()
        .find(|body| body["session"]["id"] == "C" && body["running"] == false)
        .expect("the stray C row is reported removed to sinks");
    assert_eq!(removal["event"], "swept");
}

#[test]
fn droid_stray_sweep_under_a_working_parent_reclaims_the_row_first() {
    let fixture = Fixture::new();
    droid(
        &fixture,
        "SessionStart",
        &start(Agent::Droid, "P", "startup"),
    );
    droid(&fixture, "UserPromptSubmit", &json!({"session_id": "P"}));
    droid(
        &fixture,
        "PreToolUse",
        &json!({"session_id": "C", "tool_name": "x"}),
    );
    let before = fixture.bodies().len();
    droid(&fixture, "SessionStart", &droid_spawn("C", "P", "startup"));
    let bodies = fixture.bodies().split_off(before);
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["session"]["id"], "P");
    assert_eq!(bodies[0]["running"], true);
    assert_eq!(bodies[1]["session"]["id"], "C");
    assert_eq!(bodies[1]["event"], "swept");
    assert_eq!(bodies[1]["running"], false);
}

fn terminal_env(session_id: &str) -> HookEnvironment {
    HookEnvironment {
        terminal: Some(TerminalIdentity {
            session_id: Some(session_id.into()),
            terminal_type: Some("iterm2".into()),
            kitty_listen_on: None,
            kitty_pid: None,
        }),
        ..HookEnvironment::default()
    }
}

#[test]
fn droid_child_from_another_process_keeps_the_root_terminal() {
    let mut fixture = Fixture::new();
    fixture.send_in(
        Agent::Droid,
        "SessionStart",
        &start(Agent::Droid, "P", "startup"),
        &terminal_env("w1t0p0:root"),
    );
    fixture.owner.identity.pid = 43;
    fixture.send_in(
        Agent::Droid,
        "SessionStart",
        &droid_spawn("C", "P", "startup"),
        &terminal_env("w1t0p0:elsewhere"),
    );
    let last = last_body(&fixture);
    assert_eq!(last["event"], "child:SessionStart");
    assert_eq!(last["phase"], "working");
    assert_eq!(last["terminal"]["sessionId"], "w1t0p0:root");
    assert_eq!(fixture.record("P")["terminal"]["sessionId"], "w1t0p0:root");
}

#[test]
fn convergence_reclaims_the_row_for_the_parent_that_adopted_a_stray() {
    let fixture = Fixture::new();
    droid(
        &fixture,
        "SessionStart",
        &start(Agent::Droid, "P", "startup"),
    );
    let status_dir = fixture.root.join("status");
    // Stands in for a concurrent child SessionStart that folded the stray into P during our fan-out.
    *fixture.sink.1.lock().unwrap() = Some(Box::new(move || {
        for entry in std::fs::read_dir(&status_dir).unwrap() {
            let path = entry.unwrap().path();
            let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            if record["session"]["id"] == "C" {
                std::fs::remove_file(&path).unwrap();
            } else {
                record["event"] = json!("child:SessionStart");
                record["phase"] = json!("working");
                record["children"] = json!({ crate::children::child_key("C"): "working" });
                std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
            }
        }
    }));
    droid(
        &fixture,
        "PreToolUse",
        &json!({"session_id": "C", "tool_name": "x"}),
    );
    let bodies = fixture.bodies().split_off(1);
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies[0]["session"]["id"], "C");
    assert_eq!(bodies[0]["running"], true);
    assert_eq!(bodies[1]["session"]["id"], "P");
    assert_eq!(bodies[1]["event"], "resend:child:SessionStart");
    assert_eq!(bodies[1]["phase"], "working");
    assert_eq!(bodies[1]["running"], true);
    assert_eq!(bodies[2]["session"]["id"], "C");
    assert_eq!(bodies[2]["event"], "swept");
    assert_eq!(bodies[2]["running"], false);
}
