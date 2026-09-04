use crate::consumers::ConsumerStore;
use crate::normalize::{HookEnvironment, normalize};
use crate::processes::{ProcessLookup, now_rfc3339};
use crate::protocol::{Agent, StatusEvent};
use crate::sinks::{HttpClient, SinkFanout};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const MAX_HEALTH_PROBLEMS: usize = 50;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthProblem {
    pub observed_at: String,
    pub message: String,
}

pub struct StatusStore {
    bindings_dir: PathBuf,
    lock_path: PathBuf,
    health_path: PathBuf,
}

impl StatusStore {
    pub fn open(root: PathBuf) -> io::Result<Self> {
        let bindings_dir = root.join("status");
        crate::paths::ensure_private_dir(&root)?;
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

    fn read_all(&self) -> io::Result<Vec<(PathBuf, StatusEvent)>> {
        let mut out = Vec::new();
        let entries = match fs::read_dir(&self.bindings_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = fs::read(&path)
                && let Ok(event) = serde_json::from_slice::<StatusEvent>(&bytes)
            {
                out.push((path, event));
            }
        }
        Ok(out)
    }

    pub fn running(&self, liveness: &dyn ProcessLookup) -> io::Result<Vec<StatusEvent>> {
        let _guard = self.lock()?;
        let mut alive = Vec::new();
        for (path, event) in self.read_all()? {
            match &event.process {
                Some(identity) if liveness.is_alive(identity) => alive.push(event),
                Some(_) => {
                    fs::remove_file(&path)?;
                }
                None => {}
            }
        }
        Ok(alive)
    }

    pub fn sweep(&self, liveness: &dyn ProcessLookup) -> io::Result<Vec<StatusEvent>> {
        let _guard = self.lock()?;
        let mut swept = Vec::new();
        for (path, mut event) in self.read_all()? {
            if let Some(identity) = &event.process
                && !liveness.is_alive(identity)
            {
                fs::remove_file(&path)?;
                event.running = false;
                swept.push(event);
            }
        }
        Ok(swept)
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
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub fn parse_problems(&self) -> io::Result<Vec<String>> {
        let _guard = self.lock()?;
        let mut problems = Vec::new();
        let entries = match fs::read_dir(&self.bindings_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(problems),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            if serde_json::from_slice::<StatusEvent>(&bytes).is_err() {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("<unknown>");
                problems.push(format!("failed to parse status record {name}"));
            }
        }
        Ok(problems)
    }

    pub fn sessions_envelope(&self, liveness: &dyn ProcessLookup) -> SessionsEnvelope {
        let mut problems = Vec::new();
        let sessions = match self.running(liveness) {
            Ok(sessions) => sessions,
            Err(e) => {
                problems.push(HealthProblem {
                    observed_at: now_rfc3339(),
                    message: format!("failed to read running sessions: {e}"),
                });
                Vec::new()
            }
        };
        match self.health_problems() {
            Ok(recorded) => problems.extend(recorded),
            Err(e) => problems.push(HealthProblem {
                observed_at: now_rfc3339(),
                message: format!("failed to read health problems: {e}"),
            }),
        }
        SessionsEnvelope { sessions, problems }
    }
}

pub struct SessionsEnvelope {
    pub sessions: Vec<StatusEvent>,
    pub problems: Vec<HealthProblem>,
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
    let mut recorded_event: Option<StatusEvent> = None;
    let outcome =
        normalize(agent, event, input, env, process).and_then(|maybe_event| match maybe_event {
            Some(status_event) if status_event.running => {
                store.record(&status_event)?;
                recorded_event = Some(status_event);
                Ok(())
            }
            Some(status_event) => {
                store.end(&status_event.binding_id)?;
                recorded_event = Some(status_event);
                Ok(())
            }
            None => Ok(()),
        });

    let mut problem = outcome.err().map(|e| e.to_string());

    let swept = match store.sweep(ctx.liveness) {
        Ok(swept) => swept,
        Err(e) => {
            let message = format!("sweep failed: {e}");
            problem = Some(match problem {
                Some(existing) => format!("{existing}; {message}"),
                None => message,
            });
            Vec::new()
        }
    };

    if let Some(message) = &problem {
        let _ = store.record_health(message);
    }

    fan_out_to_sinks(
        store,
        ctx.consumers,
        ctx.http_client,
        recorded_event.as_ref(),
        &swept,
    );

    IngestOutcome { problem }
}

fn fan_out_to_sinks(
    store: &StatusStore,
    consumers: Option<&ConsumerStore>,
    http_client: &dyn HttpClient,
    recorded_event: Option<&StatusEvent>,
    swept: &[StatusEvent],
) {
    if recorded_event.is_none() && swept.is_empty() {
        return;
    }
    let Some(consumers) = consumers else {
        return;
    };

    let status_consumers = match consumers.requested_capabilities("status") {
        Ok(status_consumers) => status_consumers,
        Err(e) => {
            let _ = store.record_health(&format!("sink consumer lookup failed: {e}"));
            return;
        }
    };
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

    if let Some(event) = recorded_event {
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
