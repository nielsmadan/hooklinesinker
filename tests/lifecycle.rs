use hooklinesinker::consumers::ConsumerStore;
use hooklinesinker::ingest::{IngestContext, handle_ingest};
use hooklinesinker::normalize::{HookEnvironment, normalize};
use hooklinesinker::processes::{ProcessLiveness, ProcessLookup};
use hooklinesinker::protocol::{Agent, Capability, Consumer, Phase, ProcessIdentity, StatusEvent};
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

#[derive(Default)]
struct Sink(Mutex<Vec<Value>>);

impl HttpClient for Sink {
    fn post_json(&self, _: &str, body: &[u8]) -> Result<u16, String> {
        self.0
            .lock()
            .unwrap()
            .push(serde_json::from_slice(body).unwrap());
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
            &HookEnvironment::default(),
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
        .filter(|agent| *agent != Agent::Opencode)
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
        .filter(|agent| *agent != Agent::Opencode)
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
        let expected = if agent == Agent::Opencode {
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
fn invalid_lifecycle_metadata_preserves_the_current_session() {
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
        let problem = outcome.problem.unwrap();
        assert!(
            problem.starts_with("invalid lifecycle metadata at line "),
            "{problem}"
        );
        // The offending value never reaches the diagnostic.
        assert!(!problem.contains("private"), "{problem}");
        assert!(!problem.contains("payload"), "{problem}");
        assert_eq!(fixture.ids(), ["a"]);
        assert_eq!(fixture.bodies(), before);
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
