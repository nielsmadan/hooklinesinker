use crate::events::{
    ANTIGRAVITY, CLAUDE, CODEX, COPILOT, DEVIN, DROID, EventSpec, GOOSE, KIMI, QWEN,
};
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
    KiloTypeScript,
    PiTypeScript,
    DroidJson,
    QwenJson,
    KimiToml,
    DevinJson,
    GooseJson,
    CopilotJson,
    AntigravityJson,
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
            Self::ClaudeJson
            | Self::CodexJson
            | Self::DroidJson
            | Self::QwenJson
            | Self::DevinJson
            | Self::GooseJson
            | Self::CopilotJson
            | Self::AntigravityJson => true,
            Self::OpenCodeTypeScript
            | Self::KiloTypeScript
            | Self::PiTypeScript
            | Self::KimiToml => false,
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
pub(crate) const PROFILES: [AgentProfile; 12] = [
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
        agent: Agent::Kilo,
        event_source: EventSource::OpenCode,
        hook_target: HookTarget::KiloTypeScript,
        status_events: &[],
        executable_names: &["kilo", "kilocode"],
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
    AgentProfile {
        agent: Agent::Devin,
        event_source: EventSource::Native,
        hook_target: HookTarget::DevinJson,
        status_events: DEVIN,
        executable_names: &["devin"],
        shared_host_markers: &[],
        fork_starts_parallel: false,
        retains_phase_on_selection: false,
        attributed_sessions_share_process: false,
        ambiguous_sessions_share_process: true,
        child_tracking: ChildTracking::Untracked,
        subagent_source: SubagentSource::Unreported,
    },
    AgentProfile {
        agent: Agent::Goose,
        event_source: EventSource::Native,
        hook_target: HookTarget::GooseJson,
        status_events: GOOSE,
        executable_names: &["goose"],
        shared_host_markers: &["acp", "serve", "gateway"],
        fork_starts_parallel: false,
        retains_phase_on_selection: false,
        attributed_sessions_share_process: false,
        ambiguous_sessions_share_process: false,
        child_tracking: ChildTracking::Untracked,
        subagent_source: SubagentSource::Unreported,
    },
    AgentProfile {
        agent: Agent::Copilot,
        event_source: EventSource::Native,
        hook_target: HookTarget::CopilotJson,
        status_events: COPILOT,
        executable_names: &["copilot"],
        shared_host_markers: &["--acp", "--host-environment"],
        fork_starts_parallel: false,
        retains_phase_on_selection: false,
        attributed_sessions_share_process: false,
        ambiguous_sessions_share_process: true,
        child_tracking: ChildTracking::Untracked,
        subagent_source: SubagentSource::Unreported,
    },
    AgentProfile {
        agent: Agent::Antigravity,
        event_source: EventSource::Native,
        hook_target: HookTarget::AntigravityJson,
        status_events: ANTIGRAVITY,
        executable_names: &["agy", "antigravity"],
        shared_host_markers: &[],
        fork_starts_parallel: false,
        retains_phase_on_selection: false,
        attributed_sessions_share_process: false,
        ambiguous_sessions_share_process: true,
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
    fn kilo_profile_uses_shared_plugin_lifecycle() {
        let kilo = profile(Agent::Kilo);
        assert!(kilo.event_source == EventSource::OpenCode);
        assert!(matches!(kilo.hook_target, HookTarget::KiloTypeScript));
        assert_eq!(kilo.executable_names, &["kilo", "kilocode"]);
        assert_eq!(kilo.shared_host_markers, &[] as &[&str]);
        assert_eq!(kilo.child_tracking, ChildTracking::Untracked);
        assert_eq!(kilo.subagent_source, SubagentSource::Unreported);
        assert_eq!(
            (
                kilo.fork_starts_parallel,
                kilo.retains_phase_on_selection,
                kilo.attributed_sessions_share_process,
                kilo.ambiguous_sessions_share_process
            ),
            (false, true, false, false)
        );
        assert!(!kilo.hook_target.accepts_home_relative_commands());
        assert_eq!(kilo.hook_target.timeout_value(Duration::from_secs(5)), 5);
    }

    #[test]
    fn antigravity_profile_is_conservative_and_does_not_subscribe_policy_hooks() {
        let agent = profile(Agent::Antigravity);
        assert_eq!(agent.child_tracking, ChildTracking::Untracked);
        assert_eq!(agent.subagent_source, SubagentSource::Unreported);
        assert_eq!(agent.executable_names, &["agy", "antigravity"]);
        assert_eq!(agent.shared_host_markers, &[] as &[&str]);
        assert_eq!(
            (
                agent.fork_starts_parallel,
                agent.retains_phase_on_selection,
                agent.attributed_sessions_share_process,
                agent.ambiguous_sessions_share_process
            ),
            (false, false, false, true)
        );
        assert_eq!(
            agent
                .status_events
                .iter()
                .map(|event| event.name)
                .collect::<Vec<_>>(),
            ["SessionStart", "PreInvocation", "PostToolUse", "Stop"]
        );
        assert_eq!(agent.hook_target.timeout_value(Duration::from_secs(5)), 5);
        assert!(agent.hook_target.accepts_home_relative_commands());
    }

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
    #[test]
    fn devin_profile_keeps_unreported_children_and_ambiguous_sessions() {
        let devin = profile(Agent::Devin);
        assert_eq!(devin.child_tracking, ChildTracking::Untracked);
        assert_eq!(devin.subagent_source, SubagentSource::Unreported);
        assert_eq!(devin.executable_names, &["devin"]);
        assert_eq!(devin.shared_host_markers, &[] as &[&str]);
        assert_eq!(
            (
                devin.fork_starts_parallel,
                devin.retains_phase_on_selection,
                devin.attributed_sessions_share_process,
                devin.ambiguous_sessions_share_process
            ),
            (false, false, false, true)
        );
        assert_eq!(devin.hook_target.timeout_value(Duration::from_secs(5)), 5);
        assert!(devin.hook_target.accepts_home_relative_commands());
    }
    #[test]
    fn goose_profile_has_native_cli_boundaries() {
        let goose = profile(Agent::Goose);
        assert_eq!(goose.child_tracking, ChildTracking::Untracked);
        assert_eq!(goose.subagent_source, SubagentSource::Unreported);
        assert_eq!(goose.executable_names, &["goose"]);
        assert_eq!(
            (
                goose.fork_starts_parallel,
                goose.retains_phase_on_selection,
                goose.attributed_sessions_share_process,
                goose.ambiguous_sessions_share_process
            ),
            (false, false, false, false)
        );
        assert_eq!(goose.hook_target.timeout_value(Duration::from_secs(5)), 5);
        assert!(goose.hook_target.accepts_home_relative_commands());
    }
    #[test]
    fn copilot_profile_preserves_shared_sessions_and_unreported_children() {
        let copilot = profile(Agent::Copilot);
        assert_eq!(copilot.child_tracking, ChildTracking::Untracked);
        assert_eq!(copilot.subagent_source, SubagentSource::Unreported);
        assert_eq!(copilot.executable_names, &["copilot"]);
        assert_eq!(
            (
                copilot.fork_starts_parallel,
                copilot.retains_phase_on_selection,
                copilot.attributed_sessions_share_process,
                copilot.ambiguous_sessions_share_process
            ),
            (false, false, false, true)
        );
        assert_eq!(copilot.hook_target.timeout_value(Duration::from_secs(5)), 5);
        assert!(copilot.hook_target.accepts_home_relative_commands());
    }
}
