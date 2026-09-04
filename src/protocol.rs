use clap::ValueEnum;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, ValueEnum)]
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

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Idle,
    Working,
    Permission,
    Compacting,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIdentity {
    pub id: String,
    pub cwd: String,
    pub transcript_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started_at: String,
    pub host: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalIdentity {
    pub session_id: Option<String>,
    pub terminal_type: Option<String>,
    pub kitty_listen_on: Option<String>,
    pub kitty_pid: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TmuxIdentity {
    pub pane: Option<String>,
    pub session_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
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
        let value = serde_json::to_value(sample_event()).unwrap();
        let round_tripped: StatusEvent = serde_json::from_value(value).unwrap();

        assert_eq!(round_tripped.session.id, "session-1");
        assert_eq!(round_tripped.session.transcript_path, None);
        assert!(round_tripped.process.is_some());
    }
}
