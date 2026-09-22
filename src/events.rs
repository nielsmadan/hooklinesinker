use crate::protocol::{Agent, Capability, Phase};
use std::time::Duration;

#[derive(Clone, Copy)]
pub(crate) enum EventAction {
    Update(Phase),
    Remove,
}

// Unmapped is a known hook without a phase; Unrecognized is outside the agent's event vocabulary.
#[derive(Clone, Copy)]
pub(crate) enum EventLookup {
    Mapped(EventAction),
    Unmapped,
    Unrecognized,
}

#[derive(Clone, Copy)]
pub(crate) struct EventSpec {
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

pub(crate) const CLAUDE: &[EventSpec] = &[
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

pub(crate) const CODEX: &[EventSpec] = &[
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

pub(crate) const DROID: &[EventSpec] = &[
    event!("SessionStart", None, 5, Update(Phase::Idle)),
    event!("UserPromptSubmit", None, 5, Update(Phase::Working)),
    event!("PreToolUse", Some("*"), 5, Update(Phase::Working)),
    EventSpec {
        name: "PostToolUse",
        matcher: Some("*"),
        timeout: Duration::from_secs(5),
        action: None,
    },
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

pub(crate) const QWEN: &[EventSpec] = &[
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

pub(crate) const KIMI: &[EventSpec] = &[
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

pub(crate) fn native_event_specs(agent: Agent) -> &'static [EventSpec] {
    crate::agents::profile(agent).status_events
}

pub(crate) fn subscribed_event_specs(agent: Agent, capabilities: &[Capability]) -> Vec<EventSpec> {
    let mut events = Vec::new();
    for capability in capabilities {
        match capability {
            Capability::Status => events.extend_from_slice(native_event_specs(agent)),
            Capability::Unknown => {}
        }
    }
    events
}

pub(crate) fn native_event_lookup(
    agent: Agent,
    event: &str,
    tool_name: Option<&str>,
    notification_type: Option<&str>,
) -> EventLookup {
    use EventLookup::{Mapped, Unmapped, Unrecognized};
    match (agent, event) {
        (Agent::Codex, "PreToolUse") if tool_name == Some("request_user_input") => {
            return Mapped(Update(Phase::Idle));
        }
        (Agent::Droid, "Notification") => {
            return match notification_type {
                Some("permission_prompt" | "elicitation_dialog") => {
                    Mapped(Update(Phase::Permission))
                }
                Some("idle_prompt") => Mapped(Update(Phase::Idle)),
                _ => Unmapped,
            };
        }
        (Agent::Qwen, "Notification") => {
            return match notification_type {
                Some("permission_prompt") => Mapped(Update(Phase::Permission)),
                Some("idle_prompt") => Mapped(Update(Phase::Idle)),
                _ => Unmapped,
            };
        }
        _ => {}
    }

    native_event_specs(agent)
        .iter()
        .find(|spec| spec.name == event)
        .map_or(Unrecognized, |spec| spec.action.map_or(Unmapped, Mapped))
}
