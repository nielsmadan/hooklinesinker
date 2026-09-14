use crate::protocol::{Agent, Phase};
use std::time::Duration;

#[derive(Clone, Copy)]
pub enum EventAction {
    Update(Phase),
    Remove,
}

pub struct EventSpec {
    pub name: &'static str,
    pub matcher: Option<&'static str>,
    pub timeout: Duration,
    pub action: Option<EventAction>,
}

macro_rules! event {
    ($name:literal, $matcher:expr, $seconds:literal, $action:expr) => {
        EventSpec {
            name: $name,
            matcher: $matcher,
            timeout: Duration::from_secs($seconds),
            action: Some($action),
        }
    };
}

use EventAction::{Remove, Update};

const CLAUDE: &[EventSpec] = &[
    event!("SessionStart", None, 5, Update(Phase::Idle)),
    event!("SessionEnd", None, 5, Remove),
    event!("UserPromptSubmit", None, 5, Update(Phase::Working)),
    event!("PreToolUse", Some("*"), 5, Update(Phase::Working)),
    event!("PostToolUse", Some("*"), 5, Update(Phase::Working)),
    event!("PostToolUseFailure", Some("*"), 5, Update(Phase::Working)),
    event!("PermissionRequest", Some("*"), 5, Update(Phase::Permission)),
    event!("SubagentStart", None, 5, Update(Phase::Working)),
    event!("Stop", None, 5, Update(Phase::Idle)),
    event!("StopFailure", None, 5, Update(Phase::Idle)),
    event!("PreCompact", Some("*"), 5, Update(Phase::Compacting)),
];

const CODEX: &[EventSpec] = &[
    event!("SessionStart", None, 5, Update(Phase::Idle)),
    event!("UserPromptSubmit", None, 5, Update(Phase::Working)),
    event!("PreToolUse", None, 5, Update(Phase::Working)),
    event!("PostToolUse", None, 5, Update(Phase::Working)),
    event!("PreCompact", None, 5, Update(Phase::Compacting)),
    event!("PostCompact", None, 5, Update(Phase::Working)),
    event!("PermissionRequest", None, 5, Update(Phase::Permission)),
    event!("Stop", None, 5, Update(Phase::Idle)),
    event!("Interrupt", None, 3, Update(Phase::Idle)),
    event!("SessionEnd", None, 3, Remove),
];

const DROID: &[EventSpec] = &[
    event!("SessionStart", None, 5, Update(Phase::Idle)),
    event!("UserPromptSubmit", None, 5, Update(Phase::Working)),
    event!("PreToolUse", Some("*"), 5, Update(Phase::Working)),
    event!("PostToolUse", Some("*"), 5, Update(Phase::Working)),
    event!("Stop", None, 5, Update(Phase::Idle)),
    EventSpec {
        name: "Notification",
        matcher: Some("*"),
        timeout: Duration::from_secs(5),
        action: None,
    },
    event!("PreCompact", Some("*"), 5, Update(Phase::Compacting)),
    event!("SessionEnd", None, 3, Remove),
];

const QWEN: &[EventSpec] = &[
    event!("SessionStart", None, 5, Update(Phase::Idle)),
    event!("UserPromptSubmit", None, 5, Update(Phase::Working)),
    event!("PreToolUse", Some("*"), 5, Update(Phase::Working)),
    event!("PostToolUse", Some("*"), 5, Update(Phase::Working)),
    event!("PostToolUseFailure", Some("*"), 5, Update(Phase::Working)),
    event!("PermissionRequest", Some("*"), 5, Update(Phase::Permission)),
    event!("PermissionDenied", Some("*"), 5, Update(Phase::Working)),
    event!("Stop", None, 5, Update(Phase::Idle)),
    event!("StopFailure", None, 5, Update(Phase::Idle)),
    EventSpec {
        name: "Notification",
        matcher: Some("*"),
        timeout: Duration::from_secs(5),
        action: None,
    },
    event!("PreCompact", Some("*"), 5, Update(Phase::Compacting)),
    event!("PostCompact", Some("*"), 5, Update(Phase::Working)),
    event!("SessionEnd", None, 3, Remove),
];

const KIMI: &[EventSpec] = &[
    event!("SessionStart", None, 5, Update(Phase::Idle)),
    event!("TurnStarted", None, 5, Update(Phase::Working)),
    event!("UserPromptSubmit", None, 5, Update(Phase::Working)),
    event!("PreToolUse", None, 5, Update(Phase::Working)),
    event!("PostToolUse", None, 5, Update(Phase::Working)),
    event!("PostToolUseFailure", None, 5, Update(Phase::Working)),
    event!("PermissionRequest", None, 5, Update(Phase::Permission)),
    event!("PermissionResult", None, 5, Update(Phase::Working)),
    event!("Stop", None, 5, Update(Phase::Idle)),
    event!("StopFailure", None, 5, Update(Phase::Idle)),
    event!("Interrupt", None, 5, Update(Phase::Idle)),
    event!("PreCompact", None, 5, Update(Phase::Compacting)),
    event!("PostCompact", None, 5, Update(Phase::Working)),
    event!("SessionEnd", None, 3, Remove),
];

pub const fn native_event_specs(agent: Agent) -> &'static [EventSpec] {
    match agent {
        Agent::Claude => CLAUDE,
        Agent::Codex => CODEX,
        Agent::Droid => DROID,
        Agent::Qwen => QWEN,
        Agent::Kimi => KIMI,
        Agent::Opencode | Agent::Pi => &[],
    }
}

pub fn native_event_action(
    agent: Agent,
    event: &str,
    tool_name: Option<&str>,
    notification_type: Option<&str>,
) -> Option<EventAction> {
    match (agent, event) {
        (Agent::Codex, "PreToolUse") if tool_name == Some("request_user_input") => {
            return Some(Update(Phase::Idle));
        }
        (Agent::Droid, "Notification") => {
            return match notification_type {
                Some("permission_prompt" | "elicitation_dialog") => Some(Update(Phase::Permission)),
                Some("idle_prompt") => Some(Update(Phase::Idle)),
                _ => None,
            };
        }
        (Agent::Qwen, "Notification") => {
            return match notification_type {
                Some("permission_prompt") => Some(Update(Phase::Permission)),
                Some("idle_prompt") => Some(Update(Phase::Idle)),
                _ => None,
            };
        }
        _ => {}
    }

    native_event_specs(agent)
        .iter()
        .find(|spec| spec.name == event)?
        .action
}
