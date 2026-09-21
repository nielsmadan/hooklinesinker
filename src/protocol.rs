use clap::ValueEnum;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Status,
}

impl Capability {
    pub const ALL: [Self; 1] = [Self::Status];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Consumer {
    pub name: String,
    pub protocol: u16,
    pub capabilities: Vec<Capability>,
    pub sink: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct VersionResponse {
    pub protocol: u16,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionsResponse<Problem> {
    pub protocol: u16,
    pub sessions: Vec<StatusEvent>,
    pub problems: Vec<Problem>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConsumersResponse {
    pub protocol: u16,
    pub consumers: Vec<Consumer>,
    pub problems: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProtocolResponse<Payload> {
    pub protocol: u16,
    #[serde(flatten)]
    pub payload: Payload,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DoctorCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct DoctorResponse {
    pub protocol: u16,
    pub version: String,
    pub checks: Vec<DoctorCheck>,
    pub exit_code: i32,
}

// Unknown agents lack hook and lifecycle mappings; accepting them requires a protocol-major bump.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    Claude,
    Codex,
    Opencode,
    Pi,
    Droid,
    Qwen,
    Kimi,
}

impl Agent {
    pub const ALL: [Self; 7] = [
        Self::Claude,
        Self::Codex,
        Self::Opencode,
        Self::Pi,
        Self::Droid,
        Self::Qwen,
        Self::Kimi,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
            Self::Pi => "pi",
            Self::Droid => "droid",
            Self::Qwen => "qwen",
            Self::Kimi => "kimi",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Phase {
    Idle,
    Working,
    Permission,
    Compacting,
    // A phase string from a newer protocol minor deserializes here rather than failing.
    #[serde(other)]
    Unknown,
}

impl Phase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Permission => "permission",
            Self::Compacting => "compacting",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct StatusEvent {
    pub protocol: u16,
    pub binding_id: String,
    pub agent: Agent,
    pub event: String,
    pub phase: Phase,
    pub running: bool,
    pub observed_at: String,
    pub session: SessionIdentity,
    pub process: Option<ProcessIdentity>,
    pub terminal: Option<TerminalIdentity>,
    pub tmux: Option<TmuxIdentity>,
    pub git: Option<GitIdentity>,
    pub remote_host: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct SessionIdentity {
    pub id: String,
    pub cwd: String,
    pub transcript_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started_at: String,
    pub host: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TerminalIdentity {
    pub session_id: Option<String>,
    pub terminal_type: Option<String>,
    pub kitty_listen_on: Option<String>,
    pub kitty_pid: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TmuxIdentity {
    pub pane: Option<String>,
    pub session_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct GitIdentity {
    pub branch: Option<String>,
    pub repo: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_spellings_are_kebab_case() {
        assert_eq!(serde_json::to_value(Agent::Claude).unwrap(), "claude");
        assert_eq!(serde_json::to_value(Agent::Codex).unwrap(), "codex");
        assert_eq!(serde_json::to_value(Agent::Opencode).unwrap(), "opencode");
        assert_eq!(serde_json::to_value(Agent::Pi).unwrap(), "pi");
        assert_eq!(serde_json::to_value(Agent::Droid).unwrap(), "droid");
        assert_eq!(serde_json::to_value(Agent::Qwen).unwrap(), "qwen");
        assert_eq!(serde_json::to_value(Agent::Kimi).unwrap(), "kimi");
    }

    #[test]
    fn phase_spellings_are_snake_case() {
        assert_eq!(serde_json::to_value(Phase::Idle).unwrap(), "idle");
        assert_eq!(serde_json::to_value(Phase::Working).unwrap(), "working");
        assert_eq!(
            serde_json::to_value(Phase::Permission).unwrap(),
            "permission"
        );
        assert_eq!(
            serde_json::to_value(Phase::Compacting).unwrap(),
            "compacting"
        );
        assert_eq!(serde_json::to_value(Phase::Unknown).unwrap(), "unknown");
    }

    #[test]
    fn capability_spellings_are_snake_case() {
        assert_eq!(serde_json::to_value(Capability::Status).unwrap(), "status");
    }

    fn sample_event() -> StatusEvent {
        StatusEvent {
            protocol: PROTOCOL_VERSION,
            binding_id: "binding-1".into(),
            agent: Agent::Claude,
            event: "SessionStart".into(),
            phase: Phase::Idle,
            running: true,
            observed_at: "2026-09-04T00:00:00Z".into(),
            session: SessionIdentity {
                id: "session-1".into(),
                cwd: "/tmp".into(),
                transcript_path: None,
            },
            process: Some(ProcessIdentity {
                pid: 42,
                started_at: "2026-09-04T00:00:00Z".into(),
                host: "localhost".into(),
            }),
            terminal: Some(TerminalIdentity {
                session_id: Some("terminal-1".into()),
                terminal_type: Some("iterm".into()),
                kitty_listen_on: None,
                kitty_pid: None,
            }),
            tmux: Some(TmuxIdentity {
                pane: Some("%1".into()),
                session_name: Some("main".into()),
            }),
            git: Some(GitIdentity {
                branch: Some("main".into()),
                repo: Some("hooklinesinker".into()),
            }),
            remote_host: None,
        }
    }

    #[test]
    fn status_event_fields_are_camel_case() {
        let value = serde_json::to_value(sample_event()).unwrap();

        assert_eq!(value["protocol"], 1);
        assert_eq!(value["bindingId"], "binding-1");
        assert_eq!(value["observedAt"], "2026-09-04T00:00:00Z");
        assert_eq!(value["session"]["transcriptPath"], serde_json::Value::Null);
        assert_eq!(value["process"]["startedAt"], "2026-09-04T00:00:00Z");
        assert_eq!(value["terminal"]["sessionId"], "terminal-1");
        assert_eq!(value["terminal"]["terminalType"], "iterm");
        assert_eq!(value["terminal"]["kittyListenOn"], serde_json::Value::Null);
        assert_eq!(value["terminal"]["kittyPid"], serde_json::Value::Null);
        assert_eq!(value["tmux"]["sessionName"], "main");
        assert_eq!(value["git"]["repo"], "hooklinesinker");
        assert_eq!(value["remoteHost"], serde_json::Value::Null);
    }

    #[test]
    fn status_event_round_trips_through_json() {
        let event = sample_event();
        let value = serde_json::to_value(&event).unwrap();
        let round_tripped: StatusEvent = serde_json::from_value(value).unwrap();

        assert_eq!(round_tripped, event);
    }

    #[test]
    fn response_envelopes_are_deserializable() {
        let version: VersionResponse =
            serde_json::from_value(serde_json::json!({"protocol": 1, "version": "1.2.3"})).unwrap();
        assert_eq!(version.version, "1.2.3");

        let sessions: SessionsResponse<String> = serde_json::from_value(serde_json::json!({
            "protocol": 1,
            "sessions": [],
            "problems": ["damaged record"]
        }))
        .unwrap();
        assert_eq!(sessions.problems, ["damaged record"]);

        let consumers: ConsumersResponse = serde_json::from_value(serde_json::json!({
            "protocol": 1,
            "consumers": [],
            "problems": []
        }))
        .unwrap();
        assert!(consumers.consumers.is_empty());

        let doctor: DoctorResponse = serde_json::from_value(serde_json::json!({
            "protocol": 1,
            "version": "1.2.3",
            "checks": [{"name": "state", "ok": true, "detail": "healthy"}],
            "exitCode": 0
        }))
        .unwrap();
        assert_eq!(doctor.checks[0].name, "state");
    }

    #[test]
    fn an_unrecognized_phase_string_deserializes_to_unknown() {
        let phase: Phase = serde_json::from_value(serde_json::json!("teleporting")).unwrap();
        assert_eq!(phase, Phase::Unknown);
    }

    #[test]
    fn phase_display_spellings_match_the_wire_spellings() {
        for phase in [
            Phase::Idle,
            Phase::Working,
            Phase::Permission,
            Phase::Compacting,
            Phase::Unknown,
        ] {
            assert_eq!(serde_json::to_value(phase).unwrap(), phase.as_str());
        }
    }
}
