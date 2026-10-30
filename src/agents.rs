use crate::events::{CLAUDE, CODEX, DROID, EventSpec, KIMI, QWEN};
use crate::protocol::Agent;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventSource {
    Native,
    OpenCode,
    Pi,
}

#[derive(Clone, Copy)]
pub(crate) enum HookTarget {
    ClaudeJson,
    CodexJson,
    OpenCodeTypeScript,
    PiTypeScript,
    DroidJson,
    QwenJson,
    KimiToml,
}

impl HookTarget {
    pub(crate) fn timeout_value(self, timeout: Duration) -> u64 {
        match self {
            Self::QwenJson => {
                u64::try_from(timeout.as_millis()).expect("hook timeout fits u64 milliseconds")
            }
            _ => timeout.as_secs(),
        }
    }

    // True only for hosts known to run hook commands through a POSIX shell, which expands `~/`
    // and `$HOME/`. Kimi keeps exact-match ownership; the TypeScript hosts have no commands.
    pub(crate) const fn accepts_home_relative_commands(self) -> bool {
        match self {
            Self::ClaudeJson | Self::CodexJson | Self::DroidJson | Self::QwenJson => true,
            Self::OpenCodeTypeScript | Self::PiTypeScript | Self::KimiToml => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChildTracking {
    Untracked,
    SharedSession {
        child_start: &'static str,
        child_stop: &'static str,
        root_interrupt: &'static str,
    },
    LinkedSessions {
        child_stop: &'static str,
        completion_wakes_root: bool,
    },
}

impl ChildTracking {
    pub(crate) const fn child_stop(self) -> Option<&'static str> {
        match self {
            Self::Untracked => None,
            Self::SharedSession { child_stop, .. } | Self::LinkedSessions { child_stop, .. } => {
                Some(child_stop)
            }
        }
    }

    pub(crate) const fn root_interrupt(self) -> Option<&'static str> {
        match self {
            Self::Untracked | Self::LinkedSessions { .. } => None,
            Self::SharedSession { root_interrupt, .. } => Some(root_interrupt),
        }
    }

    pub(crate) const fn completion_wakes_root(self) -> bool {
        matches!(
            self,
            Self::LinkedSessions {
                completion_wakes_root: true,
                ..
            }
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubagentSource {
    Unreported,
    BackgroundTasks,
    TrackedChildren,
}

#[allow(
    clippy::struct_excessive_bools,
    reason = "a declarative per-agent table, not an argument list"
)]
pub(crate) struct AgentProfile {
    pub agent: Agent,
    pub event_source: EventSource,
    pub hook_target: HookTarget,
    pub status_events: &'static [EventSpec],
    pub executable_names: &'static [&'static str],
    pub shared_host_markers: &'static [&'static str],
    pub fork_starts_parallel: bool,
    pub retains_phase_on_selection: bool,
    pub attributed_sessions_share_process: bool,
    pub ambiguous_sessions_share_process: bool,
    pub child_tracking: ChildTracking,
    pub subagent_source: SubagentSource,
}

// An agent whose phase depends on a payload field rather than the event name also needs an
// arm in events::native_event_lookup; nothing here forces that edit.
pub(crate) const PROFILES: [AgentProfile; 7] = [
    AgentProfile {
        agent: Agent::Claude,
        event_source: EventSource::Native,
        hook_target: HookTarget::ClaudeJson,
        status_events: CLAUDE,
        executable_names: &["claude"],
        shared_host_markers: &[],
        fork_starts_parallel: true,
        retains_phase_on_selection: false,
        attributed_sessions_share_process: false,
        ambiguous_sessions_share_process: false,
        child_tracking: ChildTracking::Untracked,
        subagent_source: SubagentSource::BackgroundTasks,
    },
    AgentProfile {
        agent: Agent::Codex,
        event_source: EventSource::Native,
        hook_target: HookTarget::CodexJson,
        status_events: CODEX,
        executable_names: &["codex"],
        shared_host_markers: &["app-server", "mcp-server"],
        fork_starts_parallel: false,
        retains_phase_on_selection: false,
        attributed_sessions_share_process: false,
        ambiguous_sessions_share_process: false,
        child_tracking: ChildTracking::SharedSession {
            child_start: "SubagentStart",
            child_stop: "SubagentStop",
            root_interrupt: "Interrupt",
        },
        subagent_source: SubagentSource::TrackedChildren,
    },
    AgentProfile {
        agent: Agent::Opencode,
        event_source: EventSource::OpenCode,
        hook_target: HookTarget::OpenCodeTypeScript,
        status_events: &[],
        executable_names: &["opencode"],
        shared_host_markers: &[],
        fork_starts_parallel: false,
        retains_phase_on_selection: true,
        attributed_sessions_share_process: false,
        ambiguous_sessions_share_process: false,
        child_tracking: ChildTracking::Untracked,
        subagent_source: SubagentSource::Unreported,
    },
    AgentProfile {
        agent: Agent::Pi,
        event_source: EventSource::Pi,
        hook_target: HookTarget::PiTypeScript,
        status_events: &[],
        executable_names: &["pi"],
        shared_host_markers: &[],
        fork_starts_parallel: false,
        retains_phase_on_selection: false,
        attributed_sessions_share_process: false,
        ambiguous_sessions_share_process: false,
        child_tracking: ChildTracking::Untracked,
        subagent_source: SubagentSource::Unreported,
    },
    AgentProfile {
        agent: Agent::Droid,
        event_source: EventSource::Native,
        hook_target: HookTarget::DroidJson,
        status_events: DROID,
        executable_names: &["droid"],
        shared_host_markers: &[],
        fork_starts_parallel: false,
        retains_phase_on_selection: false,
        attributed_sessions_share_process: false,
        ambiguous_sessions_share_process: true,
        child_tracking: ChildTracking::LinkedSessions {
            child_stop: "Stop",
            completion_wakes_root: true,
        },
        subagent_source: SubagentSource::TrackedChildren,
    },
    AgentProfile {
        agent: Agent::Qwen,
        event_source: EventSource::Native,
        hook_target: HookTarget::QwenJson,
        status_events: QWEN,
        executable_names: &["qwen"],
        shared_host_markers: &["--acp", "--experimental-acp", "serve"],
        fork_starts_parallel: false,
        retains_phase_on_selection: false,
        attributed_sessions_share_process: true,
        ambiguous_sessions_share_process: false,
        child_tracking: ChildTracking::Untracked,
        subagent_source: SubagentSource::Unreported,
    },
    AgentProfile {
        agent: Agent::Kimi,
        event_source: EventSource::Native,
        hook_target: HookTarget::KimiToml,
        status_events: KIMI,
        executable_names: &["kimi", "kimi-code"],
        shared_host_markers: &["acp", "--acp", "web", "--wire"],
        fork_starts_parallel: false,
        retains_phase_on_selection: false,
        attributed_sessions_share_process: false,
        ambiguous_sessions_share_process: false,
        child_tracking: ChildTracking::Untracked,
        subagent_source: SubagentSource::Unreported,
    },
];

pub(crate) fn profile(agent: Agent) -> &'static AgentProfile {
    PROFILES
        .iter()
        .find(|profile| profile.agent == agent)
        .expect("every protocol agent has a profile")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_all_lists_every_enum_variant() {
        // PROFILES and ALL are both hand-maintained; clap derives this one from the enum, so
        // it is the only list that cannot silently go stale when a variant is added.
        use clap::ValueEnum;
        let derived = Agent::value_variants();
        assert_eq!(
            Agent::ALL.len(),
            derived.len(),
            "Agent::ALL is missing a variant"
        );
        for agent in derived {
            assert!(
                Agent::ALL.contains(agent),
                "{} is missing from Agent::ALL",
                agent.as_str()
            );
        }
    }

    #[test]
    fn only_tracked_agents_count_their_children() {
        for profile in &PROFILES {
            assert_eq!(
                profile.subagent_source == SubagentSource::TrackedChildren,
                profile.child_tracking != ChildTracking::Untracked,
                "{}",
                profile.agent.as_str()
            );
        }
    }

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
