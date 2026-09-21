use crate::lifecycle::SessionContext;
use crate::persistence::{LockGuard, write_private_atomic};
use crate::processes::{ProcessLiveness, Timestamp};
use crate::protocol::{Agent, StatusEvent};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const MAX_HEALTH_PROBLEMS: usize = 50;
// Expired diagnostics must not replay as current faults on every read.
const HEALTH_PROBLEM_TTL_SECS: u64 = 600;
const UNVERIFIABLE_RECORD_TTL_SECS: u64 = 24 * 60 * 60;

// Doctor recovers the last sink failure from the health log, so the failing subsystem is
// part of the record rather than a prefix on its message.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum HealthKind {
    Sink {
        consumer: String,
    },
    Ingest,
    Sweep,
    #[default]
    #[serde(other)]
    Other,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthProblem {
    pub observed_at: Timestamp,
    pub message: String,
    // Protocol 1 records predate the field and read back as `Other`.
    #[serde(default)]
    pub kind: HealthKind,
}

impl HealthProblem {
    pub fn new(kind: HealthKind, message: impl Into<String>) -> Self {
        Self {
            observed_at: Timestamp::now(),
            message: message.into(),
            kind,
        }
    }

    pub const fn is_sink_failure(&self) -> bool {
        matches!(self.kind, HealthKind::Sink { .. })
    }

    const fn is_recent(&self, now: Timestamp) -> bool {
        now.seconds_since(self.observed_at) <= HEALTH_PROBLEM_TTL_SECS
    }
}

struct StatusSnapshot {
    records: Vec<(PathBuf, StoredStatus)>,
    problems: Vec<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredStatusWire {
    #[serde(flatten)]
    status: StatusEvent,
    #[serde(default, alias = "claudeParallel")]
    parallel: Option<bool>,
    #[serde(
        default,
        alias = "claudeRetired",
        skip_serializing_if = "std::ops::Not::not"
    )]
    retired: bool,
}

// `parallel` is resolved once, at the read boundary: a legacy record without the field
// infers it from the agent there, so nothing downstream carries the tri-state.
struct StoredStatus {
    status: StatusEvent,
    parallel: bool,
    retired: bool,
}

impl From<StoredStatusWire> for StoredStatus {
    fn from(wire: StoredStatusWire) -> Self {
        let parallel = wire
            .parallel
            .unwrap_or(wire.status.agent == Agent::Opencode);
        Self {
            status: wire.status,
            parallel,
            retired: wire.retired,
        }
    }
}

impl StoredStatus {
    fn write(&self, path: &Path) -> io::Result<()> {
        let wire = StoredStatusWire {
            status: self.status.clone(),
            parallel: Some(self.parallel),
            retired: self.retired,
        };
        let bytes = serde_json::to_vec_pretty(&wire)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_private_atomic(path, &bytes)
    }
}

#[derive(Default)]
pub struct SweepOutcome {
    pub events: Vec<StatusEvent>,
    pub problems: Vec<String>,
}

pub(crate) struct IngestSessionOutcome {
    pub(crate) forward: bool,
    pub(crate) retired: SweepOutcome,
}

enum SessionDisposition {
    Ignore,
    Store(StoreMode),
}

// Retirement is exclusive-mode business only, so a parallel session cannot carry the flag.
enum StoreMode {
    Parallel,
    Exclusive { retire_superseded: bool },
}

impl StoreMode {
    const fn is_parallel(&self) -> bool {
        matches!(self, Self::Parallel)
    }

    const fn retires_superseded(&self) -> bool {
        matches!(
            self,
            Self::Exclusive {
                retire_superseded: true
            }
        )
    }
}

pub struct StatusStore {
    bindings_dir: PathBuf,
    lock_path: PathBuf,
    health_path: PathBuf,
}

impl StatusStore {
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref();
        let bindings_dir = root.join("status");
        crate::paths::ensure_private_dir(root)?;
        crate::paths::ensure_private_dir(&bindings_dir)?;
        Ok(Self {
            lock_path: root.join("status.lock"),
            health_path: root.join("health.json"),
            bindings_dir,
        })
    }

    fn lock(&self) -> io::Result<LockGuard> {
        LockGuard::acquire(&self.lock_path)
    }

    fn binding_path(&self, binding_id: &str) -> io::Result<PathBuf> {
        if binding_id.len() != 64
            || !binding_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "binding ID must be 64 lowercase hexadecimal characters",
            ));
        }
        Ok(self.bindings_dir.join(format!("{binding_id}.json")))
    }

    pub fn record(&self, event: &StatusEvent) -> io::Result<()> {
        let _guard = self.lock()?;
        let path = self.binding_path(&event.binding_id)?;
        let bytes = serde_json::to_vec_pretty(event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_private_atomic(&path, &bytes)
    }

    pub(crate) fn ingest_session(
        &self,
        event: &mut StatusEvent,
        context: SessionContext,
    ) -> io::Result<IngestSessionOutcome> {
        let _guard = self.lock()?;
        let snapshot = self.read_all()?;
        let previous = snapshot.records.iter().find_map(|(_, stored)| {
            (stored.status.binding_id == event.binding_id).then_some(stored)
        });
        let SessionDisposition::Store(mode) = session_disposition(event, previous, context) else {
            return Ok(IngestSessionOutcome {
                forward: false,
                retired: SweepOutcome {
                    events: Vec::new(),
                    problems: snapshot.problems,
                },
            });
        };
        if matches!(
            context,
            SessionContext::Selected | SessionContext::SelectedParallel
        ) && event.agent == Agent::Opencode
            && let Some(previous) = previous
        {
            event.phase = previous.status.phase;
        }
        self.persist_session(event, mode.is_parallel())?;
        let retired = if mode.retires_superseded() {
            Self::retire_superseded(event, snapshot.records, snapshot.problems)
        } else {
            SweepOutcome {
                events: Vec::new(),
                problems: snapshot.problems,
            }
        };
        Ok(IngestSessionOutcome {
            forward: true,
            retired,
        })
    }

    fn persist_session(&self, event: &StatusEvent, parallel: bool) -> io::Result<()> {
        let stored = StoredStatus {
            status: event.clone(),
            parallel,
            retired: !event.running && event.process.is_some(),
        };
        if !event.running && event.process.is_none() {
            self.remove_binding(&event.binding_id)
        } else {
            stored.write(&self.binding_path(&event.binding_id)?)
        }
    }

    fn retire_superseded(
        event: &StatusEvent,
        records: Vec<(PathBuf, StoredStatus)>,
        problems: Vec<String>,
    ) -> SweepOutcome {
        let mut outcome = SweepOutcome {
            events: Vec::new(),
            problems,
        };
        for (path, mut stored) in records {
            let previous = &stored.status;
            if !stored.parallel
                && !stored.retired
                && previous.agent == event.agent
                && previous.session.id != event.session.id
                && previous.process == event.process
                && previous.terminal == event.terminal
                && previous.tmux.as_ref().and_then(|tmux| tmux.pane.as_deref())
                    == event.tmux.as_ref().and_then(|tmux| tmux.pane.as_deref())
                && previous.remote_host == event.remote_host
            {
                stored.status.running = false;
                stored.retired = true;
                match stored.write(&path) {
                    Ok(()) => {
                        outcome.events.push(stored.status);
                    }
                    Err(e) => outcome.problems.push(format!(
                        "failed to retire superseded status record {}: {e}",
                        path.display()
                    )),
                }
            }
        }
        outcome
    }

    pub fn end(&self, binding_id: &str) -> io::Result<()> {
        let _guard = self.lock()?;
        self.remove_binding(binding_id)
    }

    fn remove_binding(&self, binding_id: &str) -> io::Result<()> {
        match fs::remove_file(self.binding_path(binding_id)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn read_all(&self) -> io::Result<StatusSnapshot> {
        let mut snapshot = StatusSnapshot {
            records: Vec::new(),
            problems: Vec::new(),
        };
        let entries = match fs::read_dir(&self.bindings_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(snapshot),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let path = match entry {
                Ok(entry) => entry.path(),
                Err(e) => {
                    snapshot
                        .problems
                        .push(format!("failed to read status directory entry: {e}"));
                    continue;
                }
            };
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(e) => {
                    snapshot.problems.push(format!(
                        "failed to read status record {}: {e}",
                        path.display()
                    ));
                    continue;
                }
            };
            match serde_json::from_slice::<StoredStatusWire>(&bytes) {
                Ok(wire) => snapshot.records.push((path, wire.into())),
                Err(e) => snapshot.problems.push(format!(
                    "failed to parse status record {}: {e}",
                    path.display()
                )),
            }
        }
        Ok(snapshot)
    }

    // Deliberately stricter than `sessions_envelope`: `running()` and `dead_records()` back
    // doctor's counts, where a number computed from a partial read would read as healthy.
    fn read_complete(&self) -> io::Result<Vec<(PathBuf, StatusEvent)>> {
        let snapshot = self.read_all()?;
        if !snapshot.problems.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                snapshot.problems.join("; "),
            ));
        }
        Ok(snapshot
            .records
            .into_iter()
            .filter(|(_, stored)| !stored.retired)
            .map(|(path, stored)| (path, stored.status))
            .collect())
    }

    // Leave deletion to ingest so polling cannot swallow a sink's removal event.
    pub fn running<L: ProcessLiveness + ?Sized>(
        &self,
        liveness: &L,
    ) -> io::Result<Vec<StatusEvent>> {
        let _guard = self.lock()?;
        Ok(self
            .read_complete()?
            .into_iter()
            .filter(|(_, event)| is_live(event, liveness))
            .map(|(_, event)| event)
            .collect())
    }

    pub fn dead_records<L: ProcessLiveness + ?Sized>(&self, liveness: &L) -> io::Result<usize> {
        let _guard = self.lock()?;
        let now = Timestamp::now();
        Ok(self
            .read_complete()?
            .iter()
            .filter(|(_, event)| is_sweepable(event, liveness, now))
            .count())
    }

    pub fn sweep<L: ProcessLiveness + ?Sized>(&self, liveness: &L) -> io::Result<SweepOutcome> {
        let _guard = self.lock()?;
        let snapshot = self.read_all()?;
        let mut outcome = SweepOutcome {
            events: Vec::new(),
            problems: snapshot.problems,
        };
        let now = Timestamp::now();
        for (path, stored) in snapshot.records {
            let mut event = stored.status;
            if is_sweepable(&event, liveness, now) {
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => {
                        outcome.problems.push(format!(
                            "failed to remove status record {}: {e}",
                            path.display()
                        ));
                        continue;
                    }
                }
                if event.process.is_some() && !stored.retired {
                    event.running = false;
                    outcome.events.push(event);
                }
            }
        }
        Ok(outcome)
    }

    pub fn record_health(&self, problem: HealthProblem) -> io::Result<()> {
        let _guard = self.lock()?;
        let mut problems = self.read_health_locked()?;
        // A repeating fault refreshes its entry instead of evicting every other diagnostic.
        problems.retain(|p| p.kind != problem.kind || p.message != problem.message);
        problems.push(problem);
        if problems.len() > MAX_HEALTH_PROBLEMS {
            let excess = problems.len() - MAX_HEALTH_PROBLEMS;
            problems.drain(0..excess);
        }
        let bytes = serde_json::to_vec_pretty(&problems)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_private_atomic(&self.health_path, &bytes)
    }

    pub fn health_problems(&self) -> io::Result<Vec<HealthProblem>> {
        let _guard = self.lock()?;
        self.read_health_locked()
    }

    fn read_health_locked(&self) -> io::Result<Vec<HealthProblem>> {
        match fs::read(&self.health_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("failed to parse {}: {e}", self.health_path.display()),
                )
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub fn parse_problems(&self) -> io::Result<Vec<String>> {
        let _guard = self.lock()?;
        Ok(self.read_all()?.problems)
    }

    pub fn sessions_envelope<L: ProcessLiveness + ?Sized>(&self, liveness: &L) -> SessionsEnvelope {
        match self.read_sessions_envelope(liveness) {
            Ok(envelope) => envelope,
            Err(e) => SessionsEnvelope {
                sessions: Vec::new(),
                problems: vec![HealthProblem::new(
                    HealthKind::Other,
                    format!("failed to read running sessions: {e}"),
                )],
            },
        }
    }

    fn read_sessions_envelope<L: ProcessLiveness + ?Sized>(
        &self,
        liveness: &L,
    ) -> io::Result<SessionsEnvelope> {
        let _guard = self.lock()?;
        let snapshot = self.read_all()?;
        let sessions = snapshot
            .records
            .into_iter()
            .filter(|(_, stored)| !stored.retired)
            .map(|(path, stored)| (path, stored.status))
            .filter(|(_, event)| is_live(event, liveness))
            .map(|(_, event)| event)
            .collect();
        let mut problems: Vec<_> = snapshot
            .problems
            .into_iter()
            .map(|message| HealthProblem::new(HealthKind::Other, message))
            .collect();
        // Doctor reads the full health log for its last-known sink error.
        let now = Timestamp::now();
        match self.read_health_locked() {
            Ok(recorded) => {
                problems.extend(recorded.into_iter().filter(|p| p.is_recent(now)));
            }
            Err(e) => problems.push(HealthProblem::new(
                HealthKind::Other,
                format!("failed to read health problems: {e}"),
            )),
        }
        Ok(SessionsEnvelope { sessions, problems })
    }
}

fn session_disposition(
    event: &StatusEvent,
    previous: Option<&StoredStatus>,
    context: SessionContext,
) -> SessionDisposition {
    let resume = matches!(
        context,
        SessionContext::Selected | SessionContext::SelectedParallel
    );
    if previous.is_some_and(|stored| stored.retired) && !resume {
        return SessionDisposition::Ignore;
    }
    let parallel = matches!(context, SessionContext::SelectedParallel)
        || !resume
            && (previous.is_some_and(|stored| stored.parallel)
                || (previous.is_none() && matches!(context, SessionContext::Background))
                || matches!(context, SessionContext::Parallel));
    SessionDisposition::Store(if parallel {
        StoreMode::Parallel
    } else {
        StoreMode::Exclusive {
            retire_superseded: !matches!(context, SessionContext::Background)
                && event.running
                && event.process.is_some(),
        }
    })
}

pub struct SessionsEnvelope {
    pub sessions: Vec<StatusEvent>,
    pub problems: Vec<HealthProblem>,
}

fn is_live(event: &StatusEvent, liveness: &(impl ProcessLiveness + ?Sized)) -> bool {
    event
        .process
        .as_ref()
        .is_some_and(|identity| liveness.process_is_alive(identity))
}

fn is_dead(event: &StatusEvent, liveness: &(impl ProcessLiveness + ?Sized)) -> bool {
    event
        .process
        .as_ref()
        .is_some_and(|identity| !liveness.process_is_alive(identity))
}

fn is_sweepable(
    event: &StatusEvent,
    liveness: &(impl ProcessLiveness + ?Sized),
    now: Timestamp,
) -> bool {
    is_dead(event, liveness)
        || event.process.is_none()
            && Timestamp::parse(&event.observed_at).is_none_or(|observed| {
                observed > now || now.seconds_since(observed) > UNVERIFIABLE_RECORD_TTL_SECS
            })
}
