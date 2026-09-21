use crate::events::{CLAUDE, CODEX, DROID, EventSpec, KIMI, QWEN};
use crate::protocol::Agent;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    Native,
    OpenCode,
    Pi,
}

#[derive(Clone, Copy)]
pub enum HookTarget {
    ClaudeJson,
    CodexJson,
    OpenCodeTypeScript,
    PiTypeScript,
    DroidJson,
    QwenJson,
    KimiToml,
}

pub struct AgentProfile {
    pub agent: Agent,
    pub event_source: EventSource,
    pub hook_target: HookTarget,
    pub status_events: &'static [EventSpec],
    pub executable_names: &'static [&'static str],
    pub shared_host_markers: &'static [&'static str],
    pub fork_starts_parallel: bool,
    pub attributed_sessions_share_process: bool,
    pub timeout_is_milliseconds: bool,
}

pub const PROFILES: [AgentProfile; 7] = [
    AgentProfile {
        agent: Agent::Claude,
        event_source: EventSource::Native,
        hook_target: HookTarget::ClaudeJson,
        status_events: CLAUDE,
        executable_names: &["claude"],
        shared_host_markers: &[],
        fork_starts_parallel: true,
        attributed_sessions_share_process: false,
        timeout_is_milliseconds: false,
    },
    AgentProfile {
        agent: Agent::Codex,
        event_source: EventSource::Native,
        hook_target: HookTarget::CodexJson,
        status_events: CODEX,
        executable_names: &["codex"],
        shared_host_markers: &["app-server", "mcp-server"],
        fork_starts_parallel: false,
        attributed_sessions_share_process: false,
        timeout_is_milliseconds: false,
    },
    AgentProfile {
        agent: Agent::Opencode,
        event_source: EventSource::OpenCode,
        hook_target: HookTarget::OpenCodeTypeScript,
        status_events: &[],
        executable_names: &["opencode"],
        shared_host_markers: &[],
        fork_starts_parallel: false,
        attributed_sessions_share_process: false,
        timeout_is_milliseconds: false,
    },
    AgentProfile {
        agent: Agent::Pi,
        event_source: EventSource::Pi,
        hook_target: HookTarget::PiTypeScript,
        status_events: &[],
        executable_names: &["pi"],
        shared_host_markers: &[],
        fork_starts_parallel: false,
        attributed_sessions_share_process: false,
        timeout_is_milliseconds: false,
    },
    AgentProfile {
        agent: Agent::Droid,
        event_source: EventSource::Native,
        hook_target: HookTarget::DroidJson,
        status_events: DROID,
        executable_names: &["droid"],
        shared_host_markers: &[],
        fork_starts_parallel: false,
        attributed_sessions_share_process: false,
        timeout_is_milliseconds: false,
    },
    AgentProfile {
        agent: Agent::Qwen,
        event_source: EventSource::Native,
        hook_target: HookTarget::QwenJson,
        status_events: QWEN,
        executable_names: &["qwen"],
        shared_host_markers: &["--acp", "--experimental-acp", "serve"],
        fork_starts_parallel: false,
        attributed_sessions_share_process: true,
        timeout_is_milliseconds: true,
    },
    AgentProfile {
        agent: Agent::Kimi,
        event_source: EventSource::Native,
        hook_target: HookTarget::KimiToml,
        status_events: KIMI,
        executable_names: &["kimi", "kimi-code"],
        shared_host_markers: &["acp", "--acp", "web", "--wire"],
        fork_starts_parallel: false,
        attributed_sessions_share_process: false,
        timeout_is_milliseconds: false,
    },
];

pub fn profile(agent: Agent) -> &'static AgentProfile {
    PROFILES
        .iter()
        .find(|profile| profile.agent == agent)
        .expect("every protocol agent has a profile")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_protocol_agent_has_one_profile() {
        assert_eq!(PROFILES.len(), Agent::ALL.len());
        for agent in Agent::ALL {
            assert_eq!(
                PROFILES
                    .iter()
                    .filter(|profile| profile.agent == agent)
                    .count(),
                1
            );
        }
    }
}
