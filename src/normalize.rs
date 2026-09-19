use crate::events::{EventAction, EventLookup, native_event_lookup};
use crate::processes::now_rfc3339;
use crate::protocol::{
    Agent, GitIdentity, PROTOCOL_VERSION, Phase, ProcessIdentity, SessionIdentity, StatusEvent,
    TerminalIdentity, TmuxIdentity,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::io;

#[derive(Clone, Debug, Default)]
pub struct HookEnvironment {
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
    tool_name: Option<String>,
    cwd: Option<String>,
    notification_type: Option<String>,
}

enum MappedAction {
    Update(Phase),
    Remove,
    Ignore,
    Unrecognized,
}

// Empty session IDs reach terminal-keyed sinks but cannot be paired with a conversation's
// end event, so they are forwarded without ever entering the ledger. Keeping that in the
// return type means a second ingestion path cannot record one by omission.
#[derive(Debug)]
pub enum Normalized {
    Recordable(StatusEvent),
    ForwardOnly(StatusEvent),
    Ignored,
    Unrecognized,
}

impl Normalized {
    // The event to forward to sinks, if this maps to one. Whether it may also be recorded
    // stays in the variant.
    pub fn into_event(self) -> Option<StatusEvent> {
        match self {
            Self::Recordable(event) | Self::ForwardOnly(event) => Some(event),
            Self::Ignored | Self::Unrecognized => None,
        }
    }
}

fn map_event(
    agent: Agent,
    event: &str,
    tool_name: Option<&str>,
    notification_type: Option<&str>,
) -> MappedAction {
    use MappedAction::{Ignore, Remove, Unrecognized, Update};
    match agent {
        Agent::Pi => match event {
            "session_start" | "agent_settled" | "session_compact_idle" => Update(Phase::Idle),
            "agent_start" | "session_compact_working" | "permission_resolved" => {
                Update(Phase::Working)
            }
            "permission_prompt" => Update(Phase::Permission),
            "session_before_compact" => Update(Phase::Compacting),
            "session_shutdown" => Remove,
            _ => Unrecognized,
        },
        Agent::Opencode => match event {
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
        },
        Agent::Claude | Agent::Codex | Agent::Droid | Agent::Qwen | Agent::Kimi => {
            match native_event_lookup(agent, event, tool_name, notification_type) {
                EventLookup::Mapped(EventAction::Update(phase)) => Update(phase),
                EventLookup::Mapped(EventAction::Remove) => Remove,
                EventLookup::Unmapped => Ignore,
                EventLookup::Unrecognized => Unrecognized,
            }
        }
    }
}

pub fn normalize(
    agent: Agent,
    event: &str,
    input: &str,
    env: &HookEnvironment,
    process: Option<ProcessIdentity>,
) -> io::Result<Normalized> {
    let native: NativeEvent = serde_json::from_str(input).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "invalid native event JSON at line {} column {}",
                e.line(),
                e.column()
            ),
        )
    })?;

    let (phase, running) = match map_event(
        agent,
        event,
        native.tool_name.as_deref(),
        native.notification_type.as_deref(),
    ) {
        MappedAction::Ignore => return Ok(Normalized::Ignored),
        MappedAction::Unrecognized => return Ok(Normalized::Unrecognized),
        MappedAction::Update(phase) => (phase, true),
        MappedAction::Remove => (Phase::Idle, false),
    };

    let session_id = native.session_id.unwrap_or_default();
    let cwd = native.cwd.unwrap_or_else(|| env.cwd.clone());
    let host = process
        .as_ref()
        .map_or_else(|| env.host.clone(), |p| p.host.clone());
    let binding_id = binding_id(agent, &session_id, &host, process.as_ref());

    let recordable = !session_id.is_empty();
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
            transcript_path: native.transcript_path,
        },
        process,
        terminal: env.terminal.clone(),
        tmux: env.tmux.clone(),
        git: env.git.clone(),
        remote_host: env.remote_host.clone(),
    };
    Ok(if recordable {
        Normalized::Recordable(status)
    } else {
        Normalized::ForwardOnly(status)
    })
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
}
