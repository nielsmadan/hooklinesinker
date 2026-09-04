use crate::processes::now_rfc3339;
use crate::protocol::{
    Agent, GitIdentity, PROTOCOL_VERSION, Phase, ProcessIdentity, SessionIdentity, StatusEvent,
    TerminalIdentity, TmuxIdentity,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
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
}

fn map_event(
    agent: Agent,
    event: &str,
    tool_name: Option<&str>,
    notification_type: Option<&str>,
) -> MappedAction {
    use MappedAction::{Ignore, Remove, Update};
    match agent {
        Agent::Claude => match event {
            "SessionStart" | "Stop" | "StopFailure" => Update(Phase::Idle),
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure"
            | "SubagentStart" => Update(Phase::Working),
            "SubagentStop" => Ignore,
            "PermissionRequest" => Update(Phase::Permission),
            "PreCompact" => Update(Phase::Compacting),
            "SessionEnd" => Remove,
            _ => Ignore,
        },
        Agent::Codex => match event {
            "SessionStart" | "Stop" => Update(Phase::Idle),
            "PreToolUse" if tool_name == Some("request_user_input") => Update(Phase::Idle),
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostCompact" => {
                Update(Phase::Working)
            }
            "PermissionRequest" => Update(Phase::Permission),
            "PreCompact" => Update(Phase::Compacting),
            "SessionEnd" => Remove,
            _ => Ignore,
        },
        Agent::Pi => match event {
            "session_start" | "agent_settled" | "session_compact_idle" => Update(Phase::Idle),
            "agent_start" | "session_compact_working" | "permission_resolved" => {
                Update(Phase::Working)
            }
            "permission_prompt" => Update(Phase::Permission),
            "session_before_compact" => Update(Phase::Compacting),
            "session_shutdown" => Remove,
            _ => Ignore,
        },
        Agent::Opencode => match event {
            "session.created" | "session.status.idle" | "session.idle" | "session.error" => {
                Update(Phase::Idle)
            }
            "session.status.busy" | "session.status.retry" => Update(Phase::Working),
            "permission.asked" => Update(Phase::Permission),
            "session.compacted" => Update(Phase::Compacting),
            "session.deleted" | "server.instance.disposed" => Remove,
            _ => Ignore,
        },
        Agent::Droid => match event {
            "SessionStart" | "Stop" => Update(Phase::Idle),
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => Update(Phase::Working),
            "Notification" => match notification_type {
                Some("permission_prompt") | Some("elicitation_dialog") => Update(Phase::Permission),
                Some("idle_prompt") => Update(Phase::Idle),
                _ => Ignore,
            },
            "PreCompact" => Update(Phase::Compacting),
            "SessionEnd" => Remove,
            "SubagentStop" => Ignore,
            _ => Ignore,
        },
        Agent::Qwen => match event {
            "SessionStart" | "Stop" | "StopFailure" => Update(Phase::Idle),
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure"
            | "PermissionDenied" | "PostCompact" => Update(Phase::Working),
            "PermissionRequest" => Update(Phase::Permission),
            "Notification" => match notification_type {
                Some("permission_prompt") => Update(Phase::Permission),
                Some("idle_prompt") => Update(Phase::Idle),
                _ => Ignore,
            },
            "PreCompact" => Update(Phase::Compacting),
            "SessionEnd" => Remove,
            "SessionDelete" => Ignore,
            _ => Ignore,
        },
        Agent::Kimi => match event {
            "SessionStart" | "Stop" | "StopFailure" | "Interrupt" => Update(Phase::Idle),
            "TurnStarted" | "UserPromptSubmit" | "PreToolUse" | "PostToolUse"
            | "PostToolUseFailure" | "PermissionResult" | "PostCompact" => Update(Phase::Working),
            "PermissionRequest" => Update(Phase::Permission),
            "PreCompact" => Update(Phase::Compacting),
            "SessionEnd" => Remove,
            _ => Ignore,
        },
    }
}

pub fn normalize(
    agent: Agent,
    event: &str,
    input: &str,
    env: &HookEnvironment,
    process: Option<ProcessIdentity>,
) -> io::Result<Option<StatusEvent>> {
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
        MappedAction::Ignore => return Ok(None),
        MappedAction::Update(phase) => (phase, true),
        MappedAction::Remove => (Phase::Idle, false),
    };

    let session_id = native.session_id.unwrap_or_default();
    let cwd = native.cwd.unwrap_or_else(|| env.cwd.clone());
    let host = process
        .as_ref()
        .map(|p| p.host.clone())
        .unwrap_or_else(|| env.host.clone());
    let binding_id = binding_id(agent, &session_id, &host, process.as_ref());

    Ok(Some(StatusEvent {
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
    }))
}

fn agent_wire_name(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
        Agent::Opencode => "opencode",
        Agent::Pi => "pi",
        Agent::Droid => "droid",
        Agent::Qwen => "qwen",
        Agent::Kimi => "kimi",
    }
}

fn binding_id(
    agent: Agent,
    session_id: &str,
    host: &str,
    process: Option<&ProcessIdentity>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(agent_wire_name(agent).as_bytes());
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
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

    fn process(pid: u32) -> Option<ProcessIdentity> {
        Some(ProcessIdentity {
            pid,
            started_at: "2026-09-04T00:00:00Z".into(),
            host: "test-host".into(),
        })
    }

    #[test]
    fn binding_id_differs_by_process_but_matches_for_identical_identity() {
        let a = binding_id(Agent::Claude, "s", "host", process(1).as_ref());
        let b = binding_id(Agent::Claude, "s", "host", process(2).as_ref());
        let c = binding_id(Agent::Claude, "s", "host", process(1).as_ref());
        assert_ne!(a, b);
        assert_eq!(a, c);
    }

    #[test]
    fn ignored_events_return_none() {
        let result = normalize(Agent::Claude, "SubagentStop", "{}", &env(), process(1)).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn invalid_json_is_an_error() {
        let result = normalize(Agent::Claude, "PreToolUse", "{not json", &env(), process(1));
        assert!(result.is_err());
    }

    #[test]
    fn cwd_falls_back_to_the_environment_when_native_json_omits_it() {
        let event = normalize(Agent::Claude, "SessionStart", "{}", &env(), process(1))
            .unwrap()
            .unwrap();
        assert_eq!(event.session.cwd, "/tmp/project");
    }
}
