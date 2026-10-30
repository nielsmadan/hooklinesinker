use crate::agents::{EventSource, SubagentSource, profile};
use crate::details::{
    DetailKind, Details, EventTraits, Field, effort_field, percent_field, problem_token, text_field,
};
use crate::events::{DetailRole, EventAction, EventLookup, native_event_lookup, native_event_role};
use crate::lifecycle::subagent_id;
use crate::processes::now_rfc3339;
use crate::protocol::{
    Activity, Agent, GitIdentity, PROTOCOL_VERSION, Phase, ProcessIdentity, SessionIdentity,
    StatusEvent, TerminalIdentity, TmuxIdentity,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::io;

#[derive(Clone, Debug, Default)]
pub(crate) struct HookEnvironment {
    pub cwd: String,
    pub host: String,
    pub terminal: Option<TerminalIdentity>,
    pub tmux: Option<TmuxIdentity>,
    pub git: Option<GitIdentity>,
    pub remote_host: Option<String>,
}

#[derive(Deserialize, Default)]
struct NativeEvent {
    session_id: Option<String>,
    transcript_path: Option<String>,
    #[serde(default, deserialize_with = "lenient_text")]
    tool_name: Option<String>,
    cwd: Option<String>,
    notification_type: Option<String>,
    #[serde(
        default,
        rename = "background_tasks",
        deserialize_with = "background_work"
    )]
    background_work: bool,
}

const CHILD_WORK_TYPES: &[&str] = &[
    "subagent",
    "workflow",
    "teammate",
    "MCP task",
    "cloud session",
];

fn background_work<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(live_child_count(&value) > 0)
}

fn lenient_text<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(text) => Some(text),
        _ => None,
    })
}

fn live_child_count(value: &serde_json::Value) -> usize {
    let Some(tasks) = value.as_array() else {
        return 0;
    };
    tasks
        .iter()
        .filter(|task| {
            let Some(task) = task.as_object() else {
                return false;
            };
            let is_child_type = task
                .get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|t| CHILD_WORK_TYPES.contains(&t));
            if !is_child_type {
                return false;
            }
            match task.get("status") {
                None => true,
                Some(serde_json::Value::String(status)) => {
                    status == "running" || status == "pending"
                }
                Some(_) => false,
            }
        })
        .count()
}

fn compacts(phase: Phase, event: &str, native: &serde_json::Value) -> bool {
    phase == Phase::Compacting
        || event == "SessionStart"
            && matches!(
                native.get("source").and_then(serde_json::Value::as_str),
                Some("clear" | "compact")
            )
}

fn tool_field(role: DetailRole, native: &serde_json::Value) -> Field<String> {
    match role {
        DetailRole::ToolStart => text_field(native.get("tool_name")),
        DetailRole::ToolEnd => Field::Clear,
        DetailRole::TurnStart | DetailRole::Plain => Field::Keep,
    }
}

fn agent_details(agent: Agent, event: &str, native: &serde_json::Value) -> Details {
    let text = |key: &str| text_field(native.get(key));
    let failure = |key: &str| {
        (event == "StopFailure")
            .then(|| problem_token(native.get(key)))
            .flatten()
    };
    let interrupted = || (event == "Interrupt").then(|| "interrupted".to_string());
    match agent {
        Agent::Claude => Details {
            model: match event {
                "SessionStart" => text("model"),
                "PostModelSwitch" => text("to_model"),
                _ => Field::Keep,
            },
            effort: effort_field(native.get("effort")),
            permission_mode: text("permission_mode"),
            title: text("session_title"),
            problem: failure("error"),
            ..Details::default()
        },
        Agent::Codex => Details {
            model: text("model"),
            permission_mode: text("permission_mode"),
            problem: interrupted(),
            ..Details::default()
        },
        Agent::Droid => Details {
            permission_mode: text("permission_mode"),
            ..Details::default()
        },
        Agent::Qwen => Details {
            model: text("model"),
            permission_mode: text("permission_mode"),
            problem: failure("error"),
            context_percent: if event == "Stop" {
                percent_field(native.get("context_usage"), 100.0)
            } else {
                Field::Keep
            },
            ..Details::default()
        },
        Agent::Kimi => Details {
            model: text("model"),
            title: text("session_title"),
            problem: interrupted().or_else(|| failure("error_type")),
            ..Details::default()
        },
        Agent::Pi => Details {
            model: text("model"),
            effort: text("effort"),
            title: text("session_title"),
            context_percent: percent_field(native.get("context_percent"), 1.0),
            problem: problem_token(native.get("problem")),
            ..Details::default()
        },
        Agent::Opencode => Details::default(),
    }
}

fn read_details(
    agent: Agent,
    event: &str,
    native: &serde_json::Value,
    phase: Phase,
    role: DetailRole,
) -> Details {
    let details = agent_details(agent, event, native);
    Details {
        tool: tool_field(role, native),
        context_percent: if compacts(phase, event, native) {
            Field::Clear
        } else {
            details.context_percent
        },
        ..details
    }
}

fn read_traits(
    agent: Agent,
    native: &serde_json::Value,
    kind: DetailKind,
    role: DetailRole,
) -> EventTraits {
    let reports_tasks = profile(agent).subagent_source == SubagentSource::BackgroundTasks;
    EventTraits {
        kind,
        turn_start: role == DetailRole::TurnStart,
        subagent_origin: subagent_id(native).is_some(),
        live_background_children: native
            .get("background_tasks")
            .filter(|tasks| reports_tasks && tasks.is_array())
            .map(|tasks| u32::try_from(live_child_count(tasks)).unwrap_or(u32::MAX)),
    }
}

enum MappedAction {
    Update(Phase),
    Remove,
    Annotate,
    Refresh,
    Ignore,
    Unrecognized,
}

// Sessionless events may reach terminal-keyed sinks but cannot enter the ledger; the variant
// enforces this across ingestion paths.
#[derive(Debug)]
pub(crate) enum Normalized {
    Recordable(StatusEvent, Details, EventTraits),
    ForwardOnly(StatusEvent),
    Ignored,
    Unrecognized,
}

impl Normalized {
    #[cfg(test)]
    pub(crate) fn into_event(self) -> Option<StatusEvent> {
        match self {
            Self::Recordable(event, ..) | Self::ForwardOnly(event) => Some(event),
            Self::Ignored | Self::Unrecognized => None,
        }
    }
}

const fn native_action(lookup: EventLookup) -> MappedAction {
    match lookup {
        EventLookup::Mapped(EventAction::Update(phase)) => MappedAction::Update(phase),
        EventLookup::Mapped(EventAction::Remove) => MappedAction::Remove,
        EventLookup::Mapped(EventAction::Annotate) => MappedAction::Annotate,
        EventLookup::Mapped(EventAction::Refresh) => MappedAction::Refresh,
        EventLookup::Unmapped => MappedAction::Ignore,
        EventLookup::Unrecognized => MappedAction::Unrecognized,
    }
}

fn map_event(
    agent: Agent,
    event: &str,
    tool_name: Option<&str>,
    notification_type: Option<&str>,
    background_work: bool,
) -> (MappedAction, DetailRole) {
    use MappedAction::{Annotate, Remove, Unrecognized, Update};
    match profile(agent).event_source {
        EventSource::Pi => {
            let action = match event {
                "session_start" | "agent_settled" | "session_compact_idle" => Update(Phase::Idle),
                "agent_start" | "session_compact_working" | "permission_resolved" => {
                    Update(Phase::Working)
                }
                "permission_prompt" => Update(Phase::Permission),
                "session_before_compact" => Update(Phase::Compacting),
                "session_info_changed" | "model_select" | "thinking_level_select" => Annotate,
                "session_shutdown" => Remove,
                _ => Unrecognized,
            };
            let role = if event == "agent_start" {
                DetailRole::TurnStart
            } else {
                DetailRole::Plain
            };
            (action, role)
        }
        EventSource::OpenCode => {
            let action = match event {
                "session.created"
                | "session.status.idle"
                | "session.idle"
                | "session.error"
                | "tui.session.select" => Update(Phase::Idle),
                "session.status.busy" | "session.status.retry" => Update(Phase::Working),
                "permission.asked" => Update(Phase::Permission),
                "session.compacted" => Update(Phase::Compacting),
                "session.deleted" | "server.instance.disposed" => Remove,
                _ => Unrecognized,
            };
            (action, DetailRole::Plain)
        }
        EventSource::Native => (
            native_action(native_event_lookup(
                agent,
                event,
                tool_name,
                notification_type,
                background_work,
            )),
            native_event_role(agent, event),
        ),
    }
}

fn invalid_native_event(detail: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid native event JSON{detail}"),
    )
}

pub(crate) fn normalize(
    agent: Agent,
    event: &str,
    input: &str,
    env: &HookEnvironment,
    process: Option<ProcessIdentity>,
) -> io::Result<Normalized> {
    let input: serde_json::Value = serde_json::from_str(input).map_err(|e| {
        invalid_native_event(&format!(" at line {} column {}", e.line(), e.column()))
    })?;
    let native = NativeEvent::deserialize(&input)
        .map_err(|_| invalid_native_event(": a known field has the wrong type"))?;

    let (action, role) = map_event(
        agent,
        event,
        native.tool_name.as_deref(),
        native.notification_type.as_deref(),
        native.background_work,
    );
    let (phase, running, kind) = match action {
        MappedAction::Ignore => return Ok(Normalized::Ignored),
        MappedAction::Unrecognized => return Ok(Normalized::Unrecognized),
        MappedAction::Update(phase) => (phase, true, DetailKind::Status),
        MappedAction::Annotate => (Phase::Unknown, true, DetailKind::Annotation),
        MappedAction::Refresh => (Phase::Unknown, true, DetailKind::Refresh),
        MappedAction::Remove => (Phase::Idle, false, DetailKind::Status),
    };
    let details = read_details(agent, event, &input, phase, role);
    let traits = read_traits(agent, &input, kind, role);

    let session_id = capped(native.session_id.unwrap_or_default());
    let cwd = capped(native.cwd.unwrap_or_else(|| env.cwd.clone()));
    let host = process
        .as_ref()
        .map_or_else(|| env.host.clone(), |p| p.host.clone());
    let binding_id = binding_id(agent, &session_id, &host, process.as_ref());

    let status = StatusEvent {
        protocol: PROTOCOL_VERSION,
        binding_id,
        agent,
        event: event.to_string(),
        phase,
        running,
        observed_at: now_rfc3339(),
        session: SessionIdentity {
            id: session_id,
            cwd,
            transcript_path: native.transcript_path.map(capped),
            title: None,
            model: None,
            effort: None,
            permission_mode: None,
        },
        process,
        terminal: env.terminal.clone(),
        tmux: env.tmux.clone(),
        git: env.git.clone(),
        remote_host: env.remote_host.clone(),
        activity: Activity::default(),
    };
    Ok(classified(status, details, traits))
}

fn classified(status: StatusEvent, details: Details, traits: EventTraits) -> Normalized {
    if !status.session.id.is_empty() {
        Normalized::Recordable(status, details, traits)
    } else if traits.kind != DetailKind::Status {
        Normalized::Ignored
    } else {
        Normalized::ForwardOnly(status)
    }
}

// Every distinct session id becomes its own ledger file, and each record is re-parsed under
// the store lock on every ingest, so an agent must not be able to grow either without bound.
const MAX_FIELD_BYTES: usize = 4096;

pub(crate) fn capped(mut value: String) -> String {
    if value.len() > MAX_FIELD_BYTES {
        let mut end = MAX_FIELD_BYTES;
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
    }
    value
}

fn binding_id(
    agent: Agent,
    session_id: &str,
    host: &str,
    process: Option<&ProcessIdentity>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(agent.as_str().as_bytes());
    hasher.update([0u8]);
    hasher.update(session_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(host.as_bytes());
    hasher.update([0u8]);
    if let Some(p) = process {
        hasher.update(p.pid.to_string().as_bytes());
        hasher.update([0u8]);
        hasher.update(p.started_at.as_bytes());
    }
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(hex, "{byte:02x}").expect("writing to String cannot fail");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> HookEnvironment {
        HookEnvironment {
            cwd: "/tmp/project".into(),
            host: "test-host".into(),
            ..Default::default()
        }
    }

    fn process(pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            started_at: "2026-09-04T00:00:00Z".into(),
            host: "test-host".into(),
        }
    }

    #[test]
    fn binding_id_differs_by_process_but_matches_for_identical_identity() {
        let a = binding_id(Agent::Claude, "s", "host", Some(&process(1)));
        let b = binding_id(Agent::Claude, "s", "host", Some(&process(2)));
        let c = binding_id(Agent::Claude, "s", "host", Some(&process(1)));
        assert_ne!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn events_outside_the_agent_vocabulary_are_reported_as_unrecognized() {
        let result = normalize(
            Agent::Claude,
            "SubagentStop",
            "{}",
            &env(),
            Some(process(1)),
        )
        .unwrap();
        assert!(matches!(result, Normalized::Unrecognized));
    }

    #[test]
    fn invalid_json_is_an_error() {
        let result = normalize(
            Agent::Claude,
            "PreToolUse",
            "{not json",
            &env(),
            Some(process(1)),
        );
        assert!(result.is_err());
    }

    #[test]
    fn cwd_falls_back_to_the_environment_when_native_json_omits_it() {
        let normalized = normalize(
            Agent::Claude,
            "SessionStart",
            "{}",
            &env(),
            Some(process(1)),
        )
        .unwrap();
        let Normalized::ForwardOnly(event) = normalized else {
            panic!("an event without a session id is forward-only");
        };
        assert_eq!(event.session.cwd, "/tmp/project");
    }

    #[test]
    fn an_installed_hook_with_no_mapped_phase_is_ignored_not_unrecognized() {
        let result = normalize(
            Agent::Droid,
            "Notification",
            r#"{"session_id":"s","notification_type":"something_new"}"#,
            &env(),
            Some(process(1)),
        )
        .unwrap();
        assert!(matches!(result, Normalized::Ignored));
    }

    #[test]
    fn binding_id_is_stable_across_binary_versions() {
        // Binding IDs name persisted ledger files; a change here orphans every live record.
        assert_eq!(
            binding_id(Agent::Claude, "session-1", "host-1", Some(&process(42))),
            "7ce3e1e5c3d5d9269f5d961538ec85a5edee83eb521793c129d3aba09f1a150a"
        );
    }

    #[test]
    fn compaction_events_clear_the_context_percent() {
        for (agent, event, input) in [
            (Agent::Claude, "PreCompact", r#"{"session_id":"s"}"#),
            (
                Agent::Claude,
                "SessionStart",
                r#"{"session_id":"s","source":"clear"}"#,
            ),
            (
                Agent::Codex,
                "SessionStart",
                r#"{"session_id":"s","source":"compact"}"#,
            ),
            (Agent::Pi, "session_before_compact", r#"{"session_id":"s"}"#),
        ] {
            let Normalized::Recordable(_, details, _) =
                normalize(agent, event, input, &env(), Some(process(1))).unwrap()
            else {
                panic!("{event} is recordable");
            };
            assert_eq!(details.context_percent, Field::Clear, "{agent:?} {event}");
        }
        let Normalized::Recordable(_, details, _) = normalize(
            Agent::Claude,
            "SessionStart",
            r#"{"session_id":"s","source":"startup"}"#,
            &env(),
            Some(process(1)),
        )
        .unwrap() else {
            panic!("SessionStart is recordable");
        };
        assert_eq!(details, Details::default());
    }

    #[test]
    fn annotations_map_to_their_own_action() {
        assert!(matches!(
            native_action(EventLookup::Mapped(EventAction::Annotate)),
            MappedAction::Annotate
        ));
    }

    #[test]
    fn sessionless_annotations_are_ignored() {
        let Normalized::ForwardOnly(status) = normalize(
            Agent::Claude,
            "SessionStart",
            "{}",
            &env(),
            Some(process(1)),
        )
        .unwrap() else {
            panic!("a sessionless status is forward-only");
        };
        let traits = |kind| EventTraits {
            kind,
            ..EventTraits::default()
        };
        for kind in [DetailKind::Annotation, DetailKind::Refresh] {
            assert!(matches!(
                classified(status.clone(), Details::default(), traits(kind)),
                Normalized::Ignored
            ));
        }
        assert!(matches!(
            classified(
                status.clone(),
                Details::default(),
                traits(DetailKind::Status)
            ),
            Normalized::ForwardOnly(_)
        ));
        let session = SessionIdentity {
            id: "s".into(),
            ..status.session.clone()
        };
        assert!(matches!(
            classified(
                StatusEvent { session, ..status },
                Details::default(),
                traits(DetailKind::Annotation)
            ),
            Normalized::Recordable(
                _,
                _,
                EventTraits {
                    kind: DetailKind::Annotation,
                    ..
                }
            )
        ));
    }

    fn read_of(agent: Agent, event: &str, input: &serde_json::Value) -> (Details, EventTraits) {
        let Normalized::Recordable(_, details, traits) =
            normalize(agent, event, &input.to_string(), &env(), Some(process(1))).unwrap()
        else {
            panic!("{agent:?} {event} is recordable");
        };
        (details, traits)
    }

    fn details_of(agent: Agent, event: &str, input: &serde_json::Value) -> Details {
        read_of(agent, event, input).0
    }

    type Reset = fn(&mut Details, &mut EventTraits);
    type WrongTypeCase = (
        Agent,
        &'static str,
        serde_json::Value,
        Vec<(&'static str, Reset)>,
    );

    fn claude_wrong_type_cases() -> Vec<WrongTypeCase> {
        vec![
            (
                Agent::Claude,
                "SessionStart",
                serde_json::json!({"session_id": "s", "model": "m", "effort": {"level": "high"},
                    "permission_mode": "plan", "session_title": "t", "agent_id": "a",
                    "background_tasks": []}),
                vec![
                    ("model", |d, _| d.model = Field::Keep),
                    ("effort", |d, _| d.effort = Field::Keep),
                    ("permission_mode", |d, _| d.permission_mode = Field::Keep),
                    ("session_title", |d, _| d.title = Field::Keep),
                    ("agent_id", |_, t| t.subagent_origin = false),
                    ("background_tasks", |_, t| t.live_background_children = None),
                ],
            ),
            (
                Agent::Claude,
                "PreToolUse",
                serde_json::json!({"session_id": "s", "tool_name": "Bash", "effort": "low"}),
                vec![
                    ("tool_name", |d, _| d.tool = Field::Keep),
                    ("effort", |d, _| d.effort = Field::Keep),
                ],
            ),
            (
                Agent::Claude,
                "PostModelSwitch",
                serde_json::json!({"session_id": "s", "to_model": "m", "effort": "low"}),
                vec![
                    ("to_model", |d, _| d.model = Field::Keep),
                    ("effort", |d, _| d.effort = Field::Keep),
                ],
            ),
            (
                Agent::Claude,
                "StopFailure",
                serde_json::json!({"session_id": "s", "error": "rate_limit", "effort": "low"}),
                vec![
                    ("error", |d, _| d.problem = None),
                    ("effort", |d, _| d.effort = Field::Keep),
                ],
            ),
        ]
    }

    fn other_wrong_type_cases() -> Vec<WrongTypeCase> {
        vec![
            (
                Agent::Codex,
                "PreToolUse",
                serde_json::json!({"session_id": "s", "model": "m", "permission_mode": "default",
                    "tool_name": "Bash"}),
                vec![
                    ("model", |d, _| d.model = Field::Keep),
                    ("permission_mode", |d, _| d.permission_mode = Field::Keep),
                    ("tool_name", |d, _| d.tool = Field::Keep),
                ],
            ),
            (
                Agent::Qwen,
                "PreToolUse",
                serde_json::json!({"session_id": "s", "tool_name": "run_shell_command"}),
                vec![("tool_name", |d, _| d.tool = Field::Keep)],
            ),
            (
                Agent::Droid,
                "PreToolUse",
                serde_json::json!({"session_id": "s", "permission_mode": "auto", "tool_name": "Execute"}),
                vec![
                    ("permission_mode", |d, _| d.permission_mode = Field::Keep),
                    ("tool_name", |d, _| d.tool = Field::Keep),
                ],
            ),
            (
                Agent::Qwen,
                "Stop",
                serde_json::json!({"session_id": "s", "model": "m", "permission_mode": "yolo",
                    "context_usage": 0.5}),
                vec![
                    ("model", |d, _| d.model = Field::Keep),
                    ("permission_mode", |d, _| d.permission_mode = Field::Keep),
                    ("context_usage", |d, _| d.context_percent = Field::Keep),
                ],
            ),
            (
                Agent::Qwen,
                "StopFailure",
                serde_json::json!({"session_id": "s", "model": "m", "error": "Boom"}),
                vec![
                    ("model", |d, _| d.model = Field::Keep),
                    ("error", |d, _| d.problem = None),
                ],
            ),
            (
                Agent::Kimi,
                "StopFailure",
                serde_json::json!({"session_id": "s", "model": "m", "session_title": "t",
                    "error_type": "Boom"}),
                vec![
                    ("model", |d, _| d.model = Field::Keep),
                    ("session_title", |d, _| d.title = Field::Keep),
                    ("error_type", |d, _| d.problem = None),
                ],
            ),
            (
                Agent::Kimi,
                "PreToolUse",
                serde_json::json!({"session_id": "s", "tool_name": "Shell", "model": "m"}),
                vec![
                    ("tool_name", |d, _| d.tool = Field::Keep),
                    ("model", |d, _| d.model = Field::Keep),
                ],
            ),
            (
                Agent::Pi,
                "agent_settled",
                serde_json::json!({"session_id": "s", "model": "m", "effort": "high",
                    "session_title": "t", "context_percent": 40, "problem": "interrupted"}),
                vec![
                    ("model", |d, _| d.model = Field::Keep),
                    ("effort", |d, _| d.effort = Field::Keep),
                    ("session_title", |d, _| d.title = Field::Keep),
                    ("context_percent", |d, _| d.context_percent = Field::Keep),
                    ("problem", |d, _| d.problem = None),
                ],
            ),
        ]
    }

    #[test]
    fn a_wrong_typed_detail_is_skipped_and_the_other_details_survive() {
        let cases = claude_wrong_type_cases()
            .into_iter()
            .chain(other_wrong_type_cases());
        for (agent, event, valid, keys) in cases {
            let baseline = read_of(agent, event, &valid);
            for (key, reset) in keys {
                let numeric = matches!(key, "context_usage" | "context_percent");
                for wrong in [
                    serde_json::json!(true),
                    if numeric {
                        serde_json::json!("7")
                    } else {
                        serde_json::json!(7)
                    },
                    serde_json::json!({"level": 3}),
                    if key == "background_tasks" {
                        serde_json::json!("x")
                    } else {
                        serde_json::json!([1])
                    },
                    if numeric {
                        serde_json::json!("1.5")
                    } else {
                        serde_json::json!(1.5)
                    },
                ] {
                    let mut input = valid.clone();
                    input[key] = wrong;
                    let mut expected = baseline.clone();
                    reset(&mut expected.0, &mut expected.1);
                    assert_eq!(
                        read_of(agent, event, &input),
                        expected,
                        "{agent:?} {event} {key}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_pi_context_percent_on_a_post_compaction_event_is_kept() {
        let details = details_of(
            Agent::Pi,
            "session_compact_idle",
            &serde_json::json!({"session_id": "s", "context_percent": 12}),
        );
        assert_eq!(details.context_percent, Field::Set(12));
    }

    #[test]
    fn a_wrong_typed_tool_name_still_records_the_phase() {
        let normalized = normalize(
            Agent::Claude,
            "PreToolUse",
            r#"{"session_id":"s","tool_name":{"a":1}}"#,
            &env(),
            Some(process(1)),
        )
        .unwrap();
        let Normalized::Recordable(event, details, _) = normalized else {
            panic!("recordable");
        };
        assert_eq!(event.phase, Phase::Working);
        assert_eq!(details.tool, Field::Keep);
    }

    #[test]
    fn turn_starts_follow_each_agents_prompt_event() {
        for (agent, event, expected) in [
            (Agent::Claude, "UserPromptSubmit", true),
            (Agent::Codex, "UserPromptSubmit", true),
            (Agent::Droid, "UserPromptSubmit", true),
            (Agent::Qwen, "UserPromptSubmit", true),
            (Agent::Kimi, "TurnStarted", true),
            (Agent::Kimi, "UserPromptSubmit", false),
            (Agent::Pi, "agent_start", true),
            (Agent::Pi, "session_compact_working", false),
            (Agent::Claude, "PreToolUse", false),
            (Agent::Claude, "Stop", false),
        ] {
            let (_, traits) = read_of(agent, event, &serde_json::json!({"session_id": "s"}));
            assert_eq!(traits.turn_start, expected, "{agent:?} {event}");
        }
    }

    #[test]
    fn a_droid_tool_end_refreshes_the_tool_without_a_phase() {
        let Normalized::Recordable(event, details, traits) = normalize(
            Agent::Droid,
            "PostToolUse",
            r#"{"session_id":"s","tool_name":"Execute"}"#,
            &env(),
            Some(process(1)),
        )
        .unwrap() else {
            panic!("a Droid PostToolUse with a session is recordable");
        };
        assert_eq!(event.phase, Phase::Unknown);
        assert_eq!(details.tool, Field::Clear);
        assert_eq!(traits.kind, DetailKind::Refresh);
    }

    #[test]
    fn tool_roles_come_from_each_agents_event_table() {
        for (agent, event, expected) in [
            (
                Agent::Claude,
                "PermissionRequest",
                Field::Set("Bash".into()),
            ),
            (Agent::Qwen, "PostToolUseFailure", Field::Clear),
            (Agent::Kimi, "PreToolUse", Field::Set("Bash".into())),
            (Agent::Codex, "Stop", Field::Keep),
            (Agent::Pi, "agent_start", Field::Keep),
        ] {
            let details = details_of(
                agent,
                event,
                &serde_json::json!({"session_id": "s", "tool_name": "Bash"}),
            );
            assert_eq!(details.tool, expected, "{agent:?} {event}");
        }
    }
}
