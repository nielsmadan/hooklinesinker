use crate::consumers::ConsumerStore;
use crate::normalize::{HookEnvironment, normalize};
use crate::processes::{ProcessLookup, epoch_now, now_rfc3339, parse_epoch_seconds};
use crate::protocol::{Agent, StatusEvent};
use crate::sinks::{HttpClient, SinkFanout};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const MAX_HEALTH_PROBLEMS: usize = 50;
// Problems are transient diagnostics (a consumer's sink briefly down, a one-off parse fault).
// Past this age they are stale history, not a current fault, and must not replay on every read.
const HEALTH_PROBLEM_TTL_SECS: u64 = 600;

fn is_recent_problem(problem: &HealthProblem, now: u64) -> bool {
    parse_epoch_seconds(&problem.observed_at)
        .is_none_or(|observed| now.saturating_sub(observed) <= HEALTH_PROBLEM_TTL_SECS)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthProblem {
    pub observed_at: String,
    pub message: String,
}

struct StatusSnapshot {
    records: Vec<(PathBuf, StatusEvent)>,
    problems: Vec<String>,
}

#[derive(Default)]
pub struct SweepOutcome {
    pub events: Vec<StatusEvent>,
    pub problems: Vec<String>,
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

    fn binding_path(&self, binding_id: &str) -> PathBuf {
        self.bindings_dir.join(format!("{binding_id}.json"))
    }

    pub fn record(&self, event: &StatusEvent) -> io::Result<()> {
        let _guard = self.lock()?;
        let path = self.binding_path(&event.binding_id);
        let bytes = serde_json::to_vec_pretty(event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_private_atomic(&path, &bytes)
    }

    pub fn end(&self, binding_id: &str) -> io::Result<()> {
        let _guard = self.lock()?;
        match fs::remove_file(self.binding_path(binding_id)) {
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
            match serde_json::from_slice::<StatusEvent>(&bytes) {
                Ok(event) => snapshot.records.push((path, event)),
                Err(e) => snapshot.problems.push(format!(
                    "failed to parse status record {}: {e}",
                    path.display()
                )),
            }
        }
        Ok(snapshot)
    }

    fn read_complete(&self) -> io::Result<Vec<(PathBuf, StatusEvent)>> {
        let snapshot = self.read_all()?;
        if !snapshot.problems.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                snapshot.problems.join("; "),
            ));
        }
        Ok(snapshot.records)
    }

    // Reads never delete: only ingest's sweep removes a dead binding, so it can
    // always fan the synthetic running:false event out to sinks.
    pub fn running(&self, liveness: &dyn ProcessLookup) -> io::Result<Vec<StatusEvent>> {
        let _guard = self.lock()?;
        Ok(self
            .read_complete()?
            .into_iter()
            .filter(|(_, event)| is_live(event, liveness))
            .map(|(_, event)| event)
            .collect())
    }

    pub fn dead_records(&self, liveness: &dyn ProcessLookup) -> io::Result<usize> {
        let _guard = self.lock()?;
        Ok(self
            .read_complete()?
            .iter()
            .filter(|(_, event)| is_dead(event, liveness))
            .count())
    }

    pub fn sweep(&self, liveness: &dyn ProcessLookup) -> io::Result<SweepOutcome> {
        let _guard = self.lock()?;
        let snapshot = self.read_all()?;
        let mut outcome = SweepOutcome {
            events: Vec::new(),
            problems: snapshot.problems,
        };
        for (path, mut event) in snapshot.records {
            if is_dead(&event, liveness) {
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
                event.running = false;
                outcome.events.push(event);
            }
        }
        Ok(outcome)
    }

    pub fn record_health(&self, message: &str) -> io::Result<()> {
        let _guard = self.lock()?;
        let mut problems = self.read_health_locked()?;
        problems.push(HealthProblem {
            observed_at: now_rfc3339(),
            message: message.to_string(),
        });
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

    pub fn sessions_envelope(&self, liveness: &dyn ProcessLookup) -> SessionsEnvelope {
        match self.read_sessions_envelope(liveness) {
            Ok(envelope) => envelope,
            Err(e) => SessionsEnvelope {
                sessions: Vec::new(),
                problems: vec![health_problem(format!(
                    "failed to read running sessions: {e}"
                ))],
            },
        }
    }

    fn read_sessions_envelope(&self, liveness: &dyn ProcessLookup) -> io::Result<SessionsEnvelope> {
        let _guard = self.lock()?;
        let snapshot = self.read_all()?;
        let sessions = snapshot
            .records
            .into_iter()
            .filter(|(_, event)| is_live(event, liveness))
            .map(|(_, event)| event)
            .collect();
        let mut problems: Vec<_> = snapshot.problems.into_iter().map(health_problem).collect();
        // Doctor reads the full health log for its last-known sink error.
        let now = epoch_now();
        match self.read_health_locked() {
            Ok(recorded) => {
                problems.extend(recorded.into_iter().filter(|p| is_recent_problem(p, now)));
            }
            Err(e) => problems.push(health_problem(format!(
                "failed to read health problems: {e}"
            ))),
        }
        Ok(SessionsEnvelope { sessions, problems })
    }
}

fn health_problem(message: String) -> HealthProblem {
    HealthProblem {
        observed_at: now_rfc3339(),
        message,
    }
}

pub struct SessionsEnvelope {
    pub sessions: Vec<StatusEvent>,
    pub problems: Vec<HealthProblem>,
}

// A record without a process identity is unverifiable: neither live nor dead,
// so no read reports it and no sweep removes it.
fn is_live(event: &StatusEvent, liveness: &dyn ProcessLookup) -> bool {
    event
        .process
        .as_ref()
        .is_some_and(|identity| liveness.is_alive(identity))
}

fn is_dead(event: &StatusEvent, liveness: &dyn ProcessLookup) -> bool {
    event
        .process
        .as_ref()
        .is_some_and(|identity| !liveness.is_alive(identity))
}

pub(crate) struct LockGuard {
    file: File,
}

impl LockGuard {
    pub(crate) fn acquire(path: &Path) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.create(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        file.lock_exclusive()?;
        Ok(Self { file })
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp_path = path.with_extension("tmp");
    {
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp_path, path)?;
    Ok(())
}

pub fn read_capped(reader: &mut impl Read, limit: u64) -> io::Result<String> {
    let mut buf = Vec::new();
    reader.take(limit + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("stdin exceeded the {limit}-byte cap"),
        ));
    }
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub struct IngestOutcome {
    pub problem: Option<String>,
}

pub struct IngestContext<'a> {
    pub store: &'a StatusStore,
    pub liveness: &'a dyn ProcessLookup,
    pub consumers: Option<&'a ConsumerStore>,
    pub http_client: &'a dyn HttpClient,
}

pub fn handle_ingest(
    ctx: &IngestContext,
    agent: Agent,
    event: &str,
    input: &str,
    env: &HookEnvironment,
    hook_pid: u32,
) -> IngestOutcome {
    let store = ctx.store;
    let process = ctx.liveness.owner_of(hook_pid, agent);
    let mut normalized_event: Option<StatusEvent> = None;
    let outcome =
        normalize(agent, event, input, env, process).and_then(|maybe_event| match maybe_event {
            // An event carrying no agent session id reaches sinks (consumers key
            // rows on terminal identity) but never the ledger: its binding would
            // outlive every real session in the same still-running process.
            Some(status_event) if status_event.session.id.is_empty() => {
                normalized_event = Some(status_event);
                Ok(())
            }
            Some(status_event) if status_event.running => {
                store.record(&status_event)?;
                normalized_event = Some(status_event);
                Ok(())
            }
            Some(status_event) => {
                store.end(&status_event.binding_id)?;
                normalized_event = Some(status_event);
                Ok(())
            }
            None => Ok(()),
        });

    let mut problems: Vec<String> = outcome.err().map(|e| e.to_string()).into_iter().collect();
    let swept = match store.sweep(ctx.liveness) {
        Ok(outcome) => {
            problems.extend(outcome.problems);
            outcome.events
        }
        Err(e) => {
            problems.push(format!("sweep failed: {e}"));
            Vec::new()
        }
    };
    for message in &problems {
        let _ = store.record_health(message);
    }
    let problem = (!problems.is_empty()).then(|| problems.join("; "));

    fan_out_to_sinks(
        store,
        ctx.consumers,
        ctx.http_client,
        normalized_event.as_ref(),
        &swept,
    );

    IngestOutcome { problem }
}

fn fan_out_to_sinks(
    store: &StatusStore,
    consumers: Option<&ConsumerStore>,
    http_client: &dyn HttpClient,
    normalized_event: Option<&StatusEvent>,
    swept: &[StatusEvent],
) {
    if normalized_event.is_none() && swept.is_empty() {
        return;
    }
    let Some(consumers) = consumers else {
        return;
    };

    let snapshot = match consumers.snapshot() {
        Ok(snapshot) => snapshot,
        Err(e) => {
            let _ = store.record_health(&format!("sink consumer lookup failed: {e}"));
            return;
        }
    };
    for problem in snapshot.problems {
        let _ = store.record_health(&format!("sink consumer lookup failed: {problem}"));
    }
    let status_consumers = snapshot.consumers;
    if status_consumers.is_empty() {
        return;
    }

    let fanout = SinkFanout::new(http_client);
    let record_problems = |problems: Vec<crate::sinks::SinkProblem>| {
        for problem in problems {
            let _ = store.record_health(&format!(
                "sink {} failed: {}",
                problem.consumer, problem.message
            ));
        }
    };

    if let Some(event) = normalized_event {
        record_problems(fanout.send(event, &status_consumers));
    }
    for swept_event in swept {
        let synthetic = synthetic_swept_event(swept_event);
        record_problems(fanout.send(&synthetic, &status_consumers));
    }
}

fn synthetic_swept_event(swept: &StatusEvent) -> StatusEvent {
    let mut synthetic = swept.clone();
    synthetic.event = "swept".to_string();
    synthetic.running = false;
    synthetic.observed_at = now_rfc3339();
    synthetic
}
