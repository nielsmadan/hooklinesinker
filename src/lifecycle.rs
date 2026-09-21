use crate::agents::{EventSource, profile};
use crate::protocol::Agent;
use serde::Deserialize;
use std::io;

#[derive(Clone, Copy)]
pub enum SessionContext {
    Foreground,
    Background,
    Parallel,
    Selected,
    SelectedParallel,
}

#[derive(Deserialize)]
struct NativeContext {
    source: Option<String>,
    agent_id: Option<String>,
    source_type: Option<String>,
    source_id: Option<String>,
}

#[derive(Deserialize)]
struct PiContext {
    reason: Option<String>,
}

impl SessionContext {
    pub const fn in_shared_process(self) -> Self {
        match self {
            Self::Selected | Self::SelectedParallel => Self::SelectedParallel,
            _ => Self::Parallel,
        }
    }

    pub fn parse(agent: Agent, event: &str, input: &str) -> io::Result<Self> {
        let invalid = |e: serde_json::Error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid lifecycle metadata at line {} column {}",
                    e.line(),
                    e.column()
                ),
            )
        };
        let profile = profile(agent);
        match profile.event_source {
            EventSource::Pi => {
                let context: PiContext = serde_json::from_str(input).map_err(invalid)?;
                Ok(
                    if event == "session_start" && context.reason.as_deref() == Some("resume") {
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
                let context: NativeContext = serde_json::from_str(input).map_err(invalid)?;
                let role = if context.agent_id.is_some() {
                    Self::Background
                } else if event == "SessionStart" && context.source.as_deref() == Some("resume") {
                    Self::Selected
                } else if profile.fork_starts_parallel
                    && event == "SessionStart"
                    && context.source.as_deref() == Some("fork")
                {
                    Self::Parallel
                } else {
                    Self::Foreground
                };
                Ok(
                    if profile.attributed_sessions_share_process
                        && (context.source_type.is_some() || context.source_id.is_some())
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
