use crate::agents::{ChildTracking, EventSource, profile};
use crate::children::child_key;
use crate::normalize::capped;
use crate::protocol::Agent;
use std::io;

#[derive(Clone, Copy)]
pub(crate) enum SessionContext {
    Foreground,
    Background,
    Parallel,
    Selected,
    SelectedParallel,
}

// These fields only hint at a session's role. Reading them leniently keeps a wrongly typed
// hint from discarding a status event that normalize already accepted.
struct Hints(serde_json::Value);

impl Hints {
    fn parse(input: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(input).map(Self)
    }

    fn present(&self, name: &str) -> bool {
        self.0.get(name).is_some_and(|value| !value.is_null())
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.0.get(name).and_then(serde_json::Value::as_str)
    }
}

fn invalid_metadata(e: &serde_json::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "invalid lifecycle metadata at line {} column {}",
            e.line(),
            e.column()
        ),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChildClaim {
    Root,
    Agent {
        child_key: String,
        starts: bool,
    },
    Spawned {
        child_key: String,
        parent_session_id: String,
    },
    MaybeLinked {
        child_key: String,
    },
}

impl ChildClaim {
    pub(crate) fn parse(
        agent: Agent,
        event: &str,
        input: &str,
        session_id: &str,
    ) -> io::Result<Self> {
        match profile(agent).child_tracking {
            ChildTracking::Untracked => Ok(Self::Root),
            ChildTracking::SharedSession { child_start, .. } => {
                let hints = Hints::parse(input).map_err(|e| invalid_metadata(&e))?;
                Ok(match hints.text("agent_id") {
                    Some(agent_id) if !agent_id.is_empty() => Self::Agent {
                        child_key: child_key(agent_id),
                        starts: event == child_start,
                    },
                    _ => Self::Root,
                })
            }
            ChildTracking::LinkedSessions { .. } => {
                if event != "SessionStart" {
                    return Ok(Self::MaybeLinked {
                        child_key: child_key(session_id),
                    });
                }
                let hints = Hints::parse(input).map_err(|e| invalid_metadata(&e))?;
                Ok(
                    match hints
                        .text("calling_session_id")
                        .map(|value| capped(value.to_string()))
                    {
                        Some(parent) if !parent.is_empty() && parent != session_id => {
                            Self::Spawned {
                                child_key: child_key(session_id),
                                parent_session_id: parent,
                            }
                        }
                        _ => Self::Root,
                    },
                )
            }
        }
    }
}

pub(crate) fn awaits_user(agent: Agent, event: &str, input: &str) -> bool {
    agent == Agent::Codex
        && event == "PreToolUse"
        && Hints::parse(input)
            .is_ok_and(|hints| hints.text("tool_name") == Some("request_user_input"))
}

// Codex runs a `compact` SessionStart mid-turn, while the root's children keep running.
pub(crate) fn clears_children(event: &str, input: &str) -> bool {
    event == "SessionStart"
        && !Hints::parse(input).is_ok_and(|hints| hints.text("source") == Some("compact"))
}

impl SessionContext {
    pub(crate) const fn in_shared_process(self) -> Self {
        match self {
            Self::Selected | Self::SelectedParallel => Self::SelectedParallel,
            _ => Self::Parallel,
        }
    }

    pub(crate) fn parse(agent: Agent, event: &str, input: &str) -> io::Result<Self> {
        let invalid = |e: serde_json::Error| invalid_metadata(&e);
        let profile = profile(agent);
        match profile.event_source {
            EventSource::Pi => {
                let hints = Hints::parse(input).map_err(invalid)?;
                Ok(
                    if event == "session_start" && hints.text("reason") == Some("resume") {
                        Self::Selected
                    } else {
                        Self::Foreground
                    },
                )
            }
            EventSource::OpenCode => Ok(if event == "tui.session.select" {
                Self::SelectedParallel
            } else {
                Self::Background
            }),
            EventSource::Native => {
                let hints = Hints::parse(input).map_err(invalid)?;
                let role = if hints.present("agent_id") {
                    Self::Background
                } else if event == "SessionStart" && hints.text("source") == Some("resume") {
                    Self::Selected
                } else if profile.fork_starts_parallel
                    && event == "SessionStart"
                    && hints.text("source") == Some("fork")
                {
                    Self::Parallel
                } else {
                    Self::Foreground
                };
                let attributed = hints.present("source_type") || hints.present("source_id");
                Ok(
                    if profile.attributed_sessions_share_process && attributed
                        || profile.ambiguous_sessions_share_process && !attributed
                    {
                        role.in_shared_process()
                    } else {
                        role
                    },
                )
            }
        }
    }
}
