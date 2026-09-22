use crate::agents::{EventSource, profile};
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

impl SessionContext {
    pub(crate) const fn in_shared_process(self) -> Self {
        match self {
            Self::Selected | Self::SelectedParallel => Self::SelectedParallel,
            _ => Self::Parallel,
        }
    }

    pub(crate) fn parse(agent: Agent, event: &str, input: &str) -> io::Result<Self> {
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
