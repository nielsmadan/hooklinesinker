use crate::agents::{ChildTracking, profile};
use crate::children::{
    ChildEvent, Children, aggregate_phase, all_idle, child_key, child_phase, completion_holds_root,
    with_child,
};
use crate::details::{
    DetailKind, Details, EventTraits, Transition, child_activity, kept_session, merged_activity,
    merged_session, running_children, subagent_count,
};
use crate::lifecycle::{ChildClaim, SessionContext};
use crate::persistence::{LockGuard, json_record_paths, read_json_records, write_private_atomic};
use crate::processes::{ProcessLiveness, Timestamp};
use crate::protocol::{Activity, Agent, Phase, ProcessIdentity, SessionIdentity, StatusEvent};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const MAX_HEALTH_PROBLEMS: usize = 50;
// Expired diagnostics must not replay as current faults on every read.
const HEALTH_PROBLEM_TTL_SECS: u64 = 600;
const UNVERIFIABLE_RECORD_TTL_SECS: u64 = 24 * 60 * 60;
const SHARED_RECORD_TTL_SECS: u64 = 24 * 60 * 60;

// Doctor recovers the last sink failure from the health log, so the failing subsystem is
// part of the record rather than a prefix on its message.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum HealthKind {
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
pub(crate) struct HealthProblem {
    pub observed_at: Timestamp,
    pub message: String,
    // Protocol 1 records predate the field and read back as `Other`.
    #[serde(default)]
    pub kind: HealthKind,
}

impl HealthProblem {
    pub(crate) fn new(kind: HealthKind, message: impl Into<String>) -> Self {
        Self {
            observed_at: Timestamp::now(),
            message: message.into(),
            kind,
        }
    }

    pub(crate) const fn is_sink_failure(&self) -> bool {
        matches!(self.kind, HealthKind::Sink { .. })
    }

    pub(crate) const fn is_ingest_failure(&self) -> bool {
        matches!(self.kind, HealthKind::Ingest)
    }

    pub(crate) const fn is_recent_problem(&self, now: Timestamp) -> bool {
        self.is_recent(now)
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
    #[serde(default)]
    own_phase: Option<Phase>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    children: Children,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    held: bool,
}

// `parallel` and `own_phase` are resolved once, at the read boundary: a legacy record
// without them infers them there, so nothing downstream carries the optional forms.
#[derive(Clone)]
struct StoredStatus {
    status: StatusEvent,
    own_phase: Phase,
    children: Children,
    held: bool,
    parallel: bool,
    retired: bool,
}

impl From<StoredStatusWire> for StoredStatus {
    fn from(wire: StoredStatusWire) -> Self {
        let parallel = wire
            .parallel
            .unwrap_or(wire.status.agent == Agent::Opencode);
        Self {
            own_phase: wire.own_phase.unwrap_or(wire.status.phase),
            children: wire.children,
            held: wire.held,
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
            own_phase: Some(self.own_phase),
            children: self.children.clone(),
            held: self.held,
        };
        let bytes = serde_json::to_vec_pretty(&wire)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_private_atomic(path, &bytes)
    }
}

#[derive(Default)]
pub(crate) struct SweepOutcome {
    pub events: Vec<StatusEvent>,
    pub problems: Vec<String>,
}

pub(crate) struct ResetOutcome {
    pub removed: usize,
    pub ended: Vec<StatusEvent>,
    pub problems: Vec<String>,
}

pub(crate) enum CurrentStatus {
    Stored(StatusEvent),
    Adopted(StatusEvent),
    Gone,
}

pub(crate) struct IngestSignals<'a> {
    pub context: SessionContext,
    pub claim: ChildClaim,
    pub child: ChildEvent<'a>,
    pub clears_children: bool,
    pub details: &'a Details,
    pub traits: EventTraits,
}

pub(crate) struct IngestSessionOutcome {
    pub forward: bool,
    pub retired: SweepOutcome,
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

pub(crate) struct StatusStore {
    bindings_dir: PathBuf,
    lock_path: PathBuf,
    health_path: PathBuf,
}

impl StatusStore {
    pub(crate) fn open(root: impl AsRef<Path>) -> io::Result<Self> {
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

    #[cfg(test)]
    pub(crate) fn record(&self, event: &StatusEvent) -> io::Result<()> {
        let _guard = self.lock()?;
        let path = self.binding_path(&event.binding_id)?;
        let bytes = serde_json::to_vec_pretty(event)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_private_atomic(&path, &bytes)
    }

    pub(crate) fn ingest_session(
        &self,
        event: &mut StatusEvent,
        signals: &IngestSignals<'_>,
        liveness: &(impl ProcessLiveness + ?Sized),
    ) -> io::Result<IngestSessionOutcome> {
        let _guard = self.lock()?;
        let snapshot = self.read_all()?;
        if let Some(earlier) = binding_before_host_rename(event, &snapshot.records) {
            event.binding_id.clone_from(&earlier.binding_id);
            event.process.clone_from(&earlier.process);
        }
        let previous = snapshot.records.iter().find_map(|(_, stored)| {
            (stored.status.binding_id == event.binding_id).then_some(stored)
        });
        match signals.traits.kind {
            DetailKind::Annotation => {
                let current = previous.filter(|stored| !stored.retired).cloned();
                return self.ingest_annotation(current, signals, snapshot, liveness);
            }
            DetailKind::Refresh => {
                let (target, update) =
                    refreshed_target(&snapshot.records, event, previous, signals);
                return self.store_update(event, target.as_ref(), update, snapshot, liveness);
            }
            DetailKind::Status => {}
        }
        if let ChildClaim::Agent { child_key, starts } = &signals.claim
            && let Some(root) = previous
        {
            let root = root.clone();
            let update = (!root.retired)
                .then(|| child_update(&root, event, child_key, *starts, signals))
                .flatten();
            return self.store_update(event, Some(&root), update, snapshot, liveness);
        }
        if let ChildClaim::Spawned {
            child_key,
            parent_session_id,
        } = &signals.claim
            && let Some(root) = find_spawn_root(&snapshot.records, event, parent_session_id)
        {
            let root = root.clone();
            let stray = previous.cloned();
            let update = attach_child(
                &root,
                event,
                child_key,
                Phase::Working,
                true,
                signals.details,
            );
            return self.ingest_spawned_child(
                event,
                &root,
                update,
                stray.as_ref(),
                snapshot,
                liveness,
            );
        }
        if let ChildClaim::MaybeLinked { child_key } = &signals.claim
            && previous.is_none_or(|stored| stored.retired)
            && let Some(root) = find_linked_root(&snapshot.records, event, child_key)
        {
            let root = root.clone();
            let update = (!root.retired)
                .then(|| child_update(&root, event, child_key, false, signals))
                .flatten();
            return self.store_update(event, Some(&root), update, snapshot, liveness);
        }
        let previous = previous.cloned();
        self.ingest_root(event, previous.as_ref(), signals, snapshot, liveness)
    }

    fn ingest_root(
        &self,
        event: &mut StatusEvent,
        previous: Option<&StoredStatus>,
        signals: &IngestSignals<'_>,
        snapshot: StatusSnapshot,
        liveness: &(impl ProcessLiveness + ?Sized),
    ) -> io::Result<IngestSessionOutcome> {
        let SessionDisposition::Store(mode) = session_disposition(event, previous, signals.context)
        else {
            // Ingest is the only sweeper, so a hook for a retired binding still owes the
            // ledger its pass or dead bindings for other agents go unannounced.
            return Ok(IngestSessionOutcome {
                forward: false,
                retired: Self::reconcile_existing_records(
                    None,
                    snapshot.records,
                    snapshot.problems,
                    false,
                    liveness,
                ),
            });
        };
        let rootless = rootless_child(event, &signals.claim, signals.child);
        let from_child = rootless.is_some();
        let (own_phase, children) = if let Some(rootless) = rootless {
            rootless
        } else {
            let own_phase = if matches!(
                signals.context,
                SessionContext::Selected | SessionContext::SelectedParallel
            ) && crate::agents::profile(event.agent).retains_phase_on_selection
                && let Some(previous) = previous
            {
                previous.own_phase
            } else {
                event.phase
            };
            (
                own_phase,
                root_children(event, previous, signals.clears_children),
            )
        };
        event.phase = aggregate_phase(own_phase, &children, false);
        let earlier = earlier_session(event, previous, &snapshot.records, mode.is_parallel());
        (event.session, event.activity) = recorded_details(
            event,
            previous,
            earlier.as_ref(),
            own_phase,
            &children,
            from_child,
            signals,
        );
        self.persist_session(event, own_phase, children, mode.is_parallel())?;
        let retired = Self::reconcile_existing_records(
            Some(event),
            snapshot.records,
            snapshot.problems,
            mode.retires_superseded(),
            liveness,
        );
        Ok(IngestSessionOutcome {
            forward: true,
            retired,
        })
    }

    fn ingest_annotation(
        &self,
        current: Option<StoredStatus>,
        signals: &IngestSignals<'_>,
        snapshot: StatusSnapshot,
        liveness: &(impl ProcessLiveness + ?Sized),
    ) -> io::Result<IngestSessionOutcome> {
        let annotated = current.map(|stored| annotated(&stored, signals.details, &signals.traits));
        if let Some(stored) = &annotated {
            stored.write(&self.binding_path(&stored.status.binding_id)?)?;
        }
        Ok(IngestSessionOutcome {
            forward: false,
            retired: Self::reconcile_existing_records(
                annotated.as_ref().map(|stored| &stored.status),
                snapshot.records,
                snapshot.problems,
                false,
                liveness,
            ),
        })
    }

    fn store_update(
        &self,
        event: &mut StatusEvent,
        root: Option<&StoredStatus>,
        update: Option<ChildUpdate>,
        snapshot: StatusSnapshot,
        liveness: &(impl ProcessLiveness + ?Sized),
    ) -> io::Result<IngestSessionOutcome> {
        let Some(ChildUpdate { stored, visible }) = update else {
            let current = root.filter(|root| !root.retired).map(|root| &root.status);
            return Ok(IngestSessionOutcome {
                forward: false,
                retired: Self::reconcile_existing_records(
                    current,
                    snapshot.records,
                    snapshot.problems,
                    false,
                    liveness,
                ),
            });
        };
        stored.write(&self.binding_path(&stored.status.binding_id)?)?;
        *event = stored.status;
        let retired = Self::reconcile_existing_records(
            Some(event),
            snapshot.records,
            snapshot.problems,
            false,
            liveness,
        );
        Ok(IngestSessionOutcome {
            forward: visible,
            retired,
        })
    }

    // A stray record already reached the sinks, so its deletion is reported too.
    fn ingest_spawned_child(
        &self,
        event: &mut StatusEvent,
        root: &StoredStatus,
        update: Option<ChildUpdate>,
        stray: Option<&StoredStatus>,
        mut snapshot: StatusSnapshot,
        liveness: &(impl ProcessLiveness + ?Sized),
    ) -> io::Result<IngestSessionOutcome> {
        if let Some(stray) = stray {
            self.remove_binding(&stray.status.binding_id)?;
            snapshot
                .records
                .retain(|(_, stored)| stored.status.binding_id != stray.status.binding_id);
        }
        let Some(ChildUpdate { stored, visible }) = update else {
            let mut retired = Self::reconcile_existing_records(
                Some(&root.status),
                snapshot.records,
                snapshot.problems,
                false,
                liveness,
            );
            if let Some(stray) = stray {
                retired.events.push(stray_removal(stray));
                *event = child_labelled(&root.status, event);
            }
            return Ok(IngestSessionOutcome {
                forward: stray.is_some(),
                retired,
            });
        };
        stored.write(&self.binding_path(&stored.status.binding_id)?)?;
        *event = stored.status;
        let mut retired = Self::reconcile_existing_records(
            Some(event),
            snapshot.records,
            snapshot.problems,
            false,
            liveness,
        );
        if let Some(stray) = stray {
            retired.events.push(stray_removal(stray));
        }
        Ok(IngestSessionOutcome {
            forward: visible || stray.is_some(),
            retired,
        })
    }

    pub(crate) fn current_status(&self, sent: &StatusEvent) -> io::Result<CurrentStatus> {
        let _guard = self.lock()?;
        let path = self.binding_path(&sent.binding_id)?;
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<StoredStatusWire>(&bytes)
                .map(|wire| CurrentStatus::Stored(wire.status))
                .map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("failed to parse status record {}: {e}", path.display()),
                    )
                }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if !matches!(
                    profile(sent.agent).child_tracking,
                    ChildTracking::LinkedSessions { .. }
                ) {
                    return Ok(CurrentStatus::Gone);
                }
                let snapshot = self.read_all()?;
                Ok(
                    find_linked_root(&snapshot.records, sent, &child_key(&sent.session.id))
                        .filter(|root| !root.retired)
                        .map_or(CurrentStatus::Gone, |root| {
                            CurrentStatus::Adopted(root.status.clone())
                        }),
                )
            }
            Err(e) => Err(e),
        }
    }

    fn persist_session(
        &self,
        event: &StatusEvent,
        own_phase: Phase,
        children: Children,
        parallel: bool,
    ) -> io::Result<()> {
        let stored = StoredStatus {
            status: event.clone(),
            own_phase,
            children,
            held: false,
            parallel,
            retired: !event.running && event.process.is_some(),
        };
        if !event.running && event.process.is_none() {
            self.remove_binding(&event.binding_id)
        } else {
            stored.write(&self.binding_path(&event.binding_id)?)
        }
    }

    fn reconcile_existing_records(
        current: Option<&StatusEvent>,
        records: Vec<(PathBuf, StoredStatus)>,
        problems: Vec<String>,
        retire_superseded: bool,
        liveness: &(impl ProcessLiveness + ?Sized),
    ) -> SweepOutcome {
        let mut outcome = SweepOutcome {
            events: Vec::new(),
            problems,
        };
        let now = Timestamp::now();
        for (path, mut stored) in records {
            if current.is_some_and(|event| event.binding_id == stored.status.binding_id) {
                continue;
            }
            if current.is_some_and(|event| is_same_session_in_process(event, &stored.status)) {
                outcome.problems.extend(removal_problem(&path));
                continue;
            }
            if retire_superseded
                && !stored.parallel
                && !stored.retired
                && current.is_some_and(|event| is_superseded_record(event, &stored.status))
            {
                stored.status.running = false;
                stored.retired = true;
                stored.children = all_idle(&stored.children);
                stored.held = false;
                stored.status.phase = aggregate_phase(stored.own_phase, &stored.children, false);
                match stored.write(&path) {
                    Ok(()) => {
                        outcome.events.push(stored.status.clone());
                    }
                    Err(e) => outcome.problems.push(format!(
                        "failed to retire superseded status record {}: {e}",
                        path.display()
                    )),
                }
            }
            if is_sweepable(&stored, liveness, now) {
                if let Some(problem) = removal_problem(&path) {
                    outcome.problems.push(problem);
                    continue;
                }
                outcome.events.extend(ended_status(stored));
            }
        }
        outcome
    }

    #[cfg(test)]
    pub(crate) fn end(&self, binding_id: &str) -> io::Result<()> {
        let _guard = self.lock()?;
        self.remove_binding(binding_id)
    }

    fn remove_binding(&self, binding_id: &str) -> io::Result<()> {
        remove_record(&self.binding_path(binding_id)?)
    }

    fn read_all(&self) -> io::Result<StatusSnapshot> {
        let scanned = read_json_records(&self.bindings_dir, "status", |path, bytes| {
            serde_json::from_slice::<StoredStatusWire>(bytes)
                .map(StoredStatus::from)
                .map_err(|e| format!("failed to parse status record {}: {e}", path.display()))
        })?;
        Ok(StatusSnapshot {
            records: scanned.records,
            problems: scanned.problems,
        })
    }

    #[cfg(test)]
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

    #[cfg(test)]
    pub(crate) fn running<L: ProcessLiveness + ?Sized>(
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

    pub(crate) fn dead_records<L: ProcessLiveness + ?Sized>(
        &self,
        liveness: &L,
    ) -> io::Result<usize> {
        let _guard = self.lock()?;
        let snapshot = self.read_all()?;
        if !snapshot.problems.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                snapshot.problems.join("; "),
            ));
        }
        let now = Timestamp::now();
        Ok(snapshot
            .records
            .iter()
            .filter(|(_, stored)| !stored.retired && is_sweepable(stored, liveness, now))
            .count())
    }

    pub(crate) fn sweep<L: ProcessLiveness + ?Sized>(
        &self,
        liveness: &L,
    ) -> io::Result<SweepOutcome> {
        let _guard = self.lock()?;
        let snapshot = self.read_all()?;
        Ok(Self::reconcile_existing_records(
            None,
            snapshot.records,
            snapshot.problems,
            false,
            liveness,
        ))
    }

    pub(crate) fn reset<L: ProcessLiveness + ?Sized>(
        &self,
        liveness: &L,
    ) -> io::Result<ResetOutcome> {
        let _guard = self
            .lock()
            .map_err(|e| io::Error::new(e.kind(), format!("failed to lock the ledger: {e}")))?;
        let listed = json_record_paths(&self.bindings_dir, "status").map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("failed to list {}: {e}", self.bindings_dir.display()),
            )
        })?;
        let now = Timestamp::now();
        let mut outcome = ResetOutcome {
            removed: 0,
            ended: Vec::new(),
            problems: listed.problems,
        };
        for path in &listed.paths {
            let stored = read_stored(path);
            if let Some(problem) = removal_problem(path) {
                outcome.problems.push(problem);
                continue;
            }
            outcome.removed += 1;
            outcome.ended.extend(
                stored
                    .filter(|stored| is_sweepable(stored, liveness, now))
                    .and_then(ended_status),
            );
        }
        Ok(outcome)
    }

    pub(crate) fn record_health(&self, problem: HealthProblem) -> io::Result<()> {
        self.record_health_many([problem])
    }

    pub(crate) fn record_health_many(
        &self,
        new_problems: impl IntoIterator<Item = HealthProblem>,
    ) -> io::Result<()> {
        let _guard = self.lock()?;
        let mut problems = self.read_health_locked()?;
        for problem in new_problems {
            problems.retain(|p| p.kind != problem.kind || p.message != problem.message);
            problems.push(problem);
        }
        if problems.len() > MAX_HEALTH_PROBLEMS {
            let excess = problems.len() - MAX_HEALTH_PROBLEMS;
            problems.drain(0..excess);
        }
        let bytes = serde_json::to_vec_pretty(&problems)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        write_private_atomic(&self.health_path, &bytes)
    }

    pub(crate) fn health_problems(&self) -> io::Result<Vec<HealthProblem>> {
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

    pub(crate) fn parse_problems(&self) -> io::Result<Vec<String>> {
        let _guard = self.lock()?;
        Ok(self.read_all()?.problems)
    }

    pub(crate) fn sessions_envelope<L: ProcessLiveness + ?Sized>(
        &self,
        liveness: &L,
    ) -> SessionsEnvelope {
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

    // Do not delete dead records here; ingest must emit their sink removal events.
    fn read_sessions_envelope<L: ProcessLiveness + ?Sized>(
        &self,
        liveness: &L,
    ) -> io::Result<SessionsEnvelope> {
        let _guard = self.lock()?;
        let snapshot = self.read_all()?;
        let mut sessions: Vec<_> = snapshot
            .records
            .into_iter()
            .filter(|(_, stored)| !stored.retired)
            .map(|(path, stored)| (path, stored.status))
            .filter(|(_, event)| is_live(event, liveness))
            .map(|(_, event)| event)
            .collect();
        // Consumers replay these in order, so the newest record for a terminal lands last.
        sessions.sort_by_cached_key(|event| Timestamp::parse(&event.observed_at));
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

struct ChildUpdate {
    stored: StoredStatus,
    visible: bool,
}

fn child_update(
    root: &StoredStatus,
    event: &StatusEvent,
    child_key: &str,
    starts: bool,
    signals: &IngestSignals<'_>,
) -> Option<ChildUpdate> {
    let child_stop = profile(event.agent).child_tracking.child_stop()?;
    attach_child(
        root,
        event,
        child_key,
        child_phase(child_stop, signals.child),
        starts,
        signals.details,
    )
}

// A refresh only changes details: a linked child's refresh keeps that child's phase.
fn refreshed_target(
    records: &[(PathBuf, StoredStatus)],
    event: &StatusEvent,
    previous: Option<&StoredStatus>,
    signals: &IngestSignals<'_>,
) -> (Option<StoredStatus>, Option<ChildUpdate>) {
    let linked = match &signals.claim {
        ChildClaim::MaybeLinked { child_key } if previous.is_none_or(|stored| stored.retired) => {
            find_linked_root(records, event, child_key).map(|root| (root, child_key))
        }
        _ => None,
    };
    let update = match linked {
        Some((root, child_key)) if !root.retired => root
            .children
            .get(child_key)
            .and_then(|&phase| attach_child(root, event, child_key, phase, false, signals.details)),
        Some(_) => None,
        None => previous
            .filter(|stored| !stored.retired)
            .and_then(|stored| refreshed(stored, event, signals)),
    };
    let target = linked.map(|(root, _)| root).or(previous).cloned();
    (target, update)
}

fn refreshed(
    stored: &StoredStatus,
    event: &StatusEvent,
    signals: &IngestSignals<'_>,
) -> Option<ChildUpdate> {
    let mut next = annotated(stored, signals.details, &signals.traits);
    if next.status == stored.status {
        return None;
    }
    next.status.event.clone_from(&event.event);
    next.status.observed_at.clone_from(&event.observed_at);
    Some(ChildUpdate {
        stored: next,
        visible: true,
    })
}

fn stray_removal(stray: &StoredStatus) -> StatusEvent {
    let mut status = stray.status.clone();
    status.running = false;
    status
}

fn child_labelled(root: &StatusEvent, child: &StatusEvent) -> StatusEvent {
    StatusEvent {
        event: format!("child:{}", child.event),
        observed_at: child.observed_at.clone(),
        ..root.clone()
    }
}

fn attach_child(
    root: &StoredStatus,
    event: &StatusEvent,
    child_key: &str,
    phase: Phase,
    create: bool,
    details: &Details,
) -> Option<ChildUpdate> {
    let children = with_child(&root.children, child_key, phase, create);
    let tracking = profile(event.agent).child_tracking;
    let held = root.held
        || tracking.completion_wakes_root()
            && tracking.child_stop() == Some(event.event.as_str())
            && completion_holds_root(
                root.own_phase,
                root.children.get(child_key).copied(),
                phase,
                &children,
            );
    let mut status = root.status.clone();
    status.phase = aggregate_phase(root.own_phase, &children, held);
    status.activity = child_activity(
        &root.status.activity,
        details,
        Transition {
            before: Some(root.status.phase),
            after: status.phase,
        },
        running_children(&children),
    );
    if same_process(event.process.as_ref(), root.status.process.as_ref()) {
        status.terminal.clone_from(&event.terminal);
        status.tmux.clone_from(&event.tmux);
        status.git.clone_from(&event.git);
        status.remote_host.clone_from(&event.remote_host);
    }
    let visible = status != root.status;
    if !visible && children == root.children && held == root.held {
        return None;
    }
    Some(ChildUpdate {
        stored: StoredStatus {
            status: child_labelled(&status, event),
            own_phase: root.own_phase,
            children,
            held,
            parallel: root.parallel,
            retired: root.retired,
        },
        visible,
    })
}

fn annotated(stored: &StoredStatus, details: &Details, traits: &EventTraits) -> StoredStatus {
    let traits = EventTraits {
        turn_start: false,
        ..*traits
    };
    let mut status = stored.status.clone();
    status.session = merged_session(
        &status.session,
        Some(&stored.status.session),
        details,
        &traits,
    );
    status.activity = merged_activity(
        Some(&stored.status.activity),
        details,
        &traits,
        Transition::steady(stored.own_phase),
        Transition::steady(stored.status.phase),
        subagent_count(
            profile(status.agent).subagent_source,
            &stored.children,
            Some(&stored.status.activity),
            &traits,
        ),
    );
    StoredStatus {
        status,
        ..stored.clone()
    }
}

fn recorded_details(
    event: &StatusEvent,
    previous: Option<&StoredStatus>,
    earlier_session: Option<&SessionIdentity>,
    own_phase: Phase,
    children: &Children,
    from_child: bool,
    signals: &IngestSignals<'_>,
) -> (SessionIdentity, Activity) {
    let earlier_activity = previous.map(|stored| &stored.status.activity);
    let subagents = subagent_count(
        profile(event.agent).subagent_source,
        children,
        earlier_activity,
        &signals.traits,
    );
    let aggregate = Transition {
        before: previous.map(|stored| stored.status.phase),
        after: event.phase,
    };
    if from_child {
        return (
            kept_session(&event.session, earlier_session),
            child_activity(
                &earlier_activity.cloned().unwrap_or_default(),
                signals.details,
                aggregate,
                subagents,
            ),
        );
    }
    (
        merged_session(
            &event.session,
            earlier_session,
            signals.details,
            &signals.traits,
        ),
        merged_activity(
            earlier_activity,
            signals.details,
            &signals.traits,
            Transition {
                before: previous.map(|stored| stored.own_phase),
                after: own_phase,
            },
            aggregate,
            subagents,
        ),
    )
}

fn earlier_session(
    event: &StatusEvent,
    previous: Option<&StoredStatus>,
    records: &[(PathBuf, StoredStatus)],
    parallel: bool,
) -> Option<SessionIdentity> {
    match previous {
        Some(stored) => Some(stored.status.session.clone()),
        None if parallel => None,
        None => superseded_session(event, records),
    }
}

// After `/clear` the new conversation runs on the same model and settings, under a new title.
fn superseded_session(
    event: &StatusEvent,
    records: &[(PathBuf, StoredStatus)],
) -> Option<SessionIdentity> {
    records
        .iter()
        .map(|(_, stored)| stored)
        .filter(|stored| !stored.parallel && is_superseded_record(event, &stored.status))
        .max_by_key(|stored| Timestamp::parse(&stored.status.observed_at))
        .map(|stored| SessionIdentity {
            title: None,
            ..stored.status.session.clone()
        })
}

// Same-process candidate wins; otherwise the most recently observed one does.
fn best_root<'a>(
    candidates: impl Iterator<Item = &'a StoredStatus>,
    process: Option<&ProcessIdentity>,
) -> Option<&'a StoredStatus> {
    candidates.max_by_key(|stored| {
        let in_process = same_process(stored.status.process.as_ref(), process);
        (in_process, Timestamp::parse(&stored.status.observed_at))
    })
}

// The parent is a running root, or already a child of one (grandchild spawn).
fn find_spawn_root<'a>(
    records: &'a [(PathBuf, StoredStatus)],
    event: &StatusEvent,
    parent_session_id: &str,
) -> Option<&'a StoredStatus> {
    let parent_key = child_key(parent_session_id);
    best_root(
        records.iter().map(|(_, stored)| stored).filter(|stored| {
            stored.status.agent == event.agent
                && !stored.retired
                && ((stored.status.session.id == parent_session_id && stored.status.running)
                    || stored.children.contains_key(&parent_key))
        }),
        event.process.as_ref(),
    )
}

// Includes retired roots so a late child event is ignored.
fn find_linked_root<'a>(
    records: &'a [(PathBuf, StoredStatus)],
    event: &StatusEvent,
    child_key: &str,
) -> Option<&'a StoredStatus> {
    best_root(
        records.iter().map(|(_, stored)| stored).filter(|stored| {
            stored.status.agent == event.agent && stored.children.contains_key(child_key)
        }),
        event.process.as_ref(),
    )
}

fn rootless_child(
    event: &StatusEvent,
    claim: &ChildClaim,
    child: ChildEvent<'_>,
) -> Option<(Phase, Children)> {
    let ChildClaim::Agent { child_key, .. } = claim else {
        return None;
    };
    let child_stop = profile(event.agent).child_tracking.child_stop()?;
    let phase = child_phase(child_stop, child);
    Some((
        Phase::Idle,
        with_child(&Children::new(), child_key, phase, true),
    ))
}

fn root_children(
    event: &StatusEvent,
    previous: Option<&StoredStatus>,
    clears_children: bool,
) -> Children {
    let Some(previous) = previous else {
        return Children::new();
    };
    if clears_children {
        Children::new()
    } else if !event.running
        || profile(event.agent).child_tracking.root_interrupt() == Some(event.event.as_str())
    {
        all_idle(&previous.children)
    } else {
        previous.children.clone()
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

pub(crate) struct SessionsEnvelope {
    pub sessions: Vec<StatusEvent>,
    pub problems: Vec<HealthProblem>,
}

fn is_live(event: &StatusEvent, liveness: &(impl ProcessLiveness + ?Sized)) -> bool {
    event
        .process
        .as_ref()
        .is_some_and(|identity| liveness.process_is_alive(identity))
}

fn read_stored(path: &Path) -> Option<StoredStatus> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice::<StoredStatusWire>(&bytes)
        .ok()
        .map(StoredStatus::from)
}

fn removal_problem(path: &Path) -> Option<String> {
    remove_record(path)
        .err()
        .map(|e| format!("failed to remove status record {}: {e}", path.display()))
}

fn ended_status(mut stored: StoredStatus) -> Option<StatusEvent> {
    (stored.status.process.is_some() && !stored.retired).then(|| {
        stored.status.running = false;
        stored.status
    })
}

fn remove_record(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn same_process(a: Option<&ProcessIdentity>, b: Option<&ProcessIdentity>) -> bool {
    a.zip(b).is_some_and(|(a, b)| a.is_same_process(b))
}

fn is_same_session_in_process(a: &StatusEvent, b: &StatusEvent) -> bool {
    a.agent == b.agent
        && a.session.id == b.session.id
        && a.remote_host == b.remote_host
        && same_process(a.process.as_ref(), b.process.as_ref())
}

fn is_superseded_record(current: &StatusEvent, previous: &StatusEvent) -> bool {
    let pane = current.tmux.as_ref().and_then(|tmux| tmux.pane.as_deref());
    // Attaching or detaching tmux changes the reported terminal within one process.
    previous.agent == current.agent
        && previous.session.id != current.session.id
        && same_process(previous.process.as_ref(), current.process.as_ref())
        && previous.tmux.as_ref().and_then(|tmux| tmux.pane.as_deref()) == pane
        && (pane.is_some() || previous.terminal == current.terminal)
        && previous.remote_host == current.remote_host
}

// The binding hashes the hostname, which macOS renames as networks change; a session keeps
// the binding it already has so consumers never see it re-keyed.
fn binding_before_host_rename<'a>(
    event: &StatusEvent,
    records: &'a [(PathBuf, StoredStatus)],
) -> Option<&'a StatusEvent> {
    if records
        .iter()
        .any(|(_, stored)| stored.status.binding_id == event.binding_id)
    {
        return None;
    }
    records
        .iter()
        .map(|(_, stored)| &stored.status)
        .filter(|status| is_same_session_in_process(event, status))
        .max_by_key(|status| Timestamp::parse(&status.observed_at))
}

fn is_dead(event: &StatusEvent, liveness: &(impl ProcessLiveness + ?Sized)) -> bool {
    event
        .process
        .as_ref()
        .is_some_and(|identity| !liveness.process_is_alive(identity))
}

fn is_sweepable(
    stored: &StoredStatus,
    liveness: &(impl ProcessLiveness + ?Sized),
    now: Timestamp,
) -> bool {
    let event = &stored.status;
    if is_dead(event, liveness) {
        return true;
    }
    // A verified-live process outranks its own timestamp: a backward clock step or a corrupt
    // observedAt must not delete the record and announce the session as gone.
    let Some(observed) = Timestamp::parse(&event.observed_at) else {
        return event.process.is_none();
    };
    let age = now.seconds_since(observed);
    event.process.is_none() && (observed > now || age > UNVERIFIABLE_RECORD_TTL_SECS)
        || stored.parallel && age > SHARED_RECORD_TTL_SECS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::details::Field;
    use serde_json::{Value, json};

    fn legacy_record() -> Value {
        json!({
            "protocol": 1,
            "bindingId": "0".repeat(64),
            "agent": "codex",
            "event": "PreToolUse",
            "phase": "working",
            "running": true,
            "observedAt": "2026-10-08T00:00:00Z",
            "session": {"id": "a", "cwd": "/tmp", "transcriptPath": null},
            "process": null,
            "terminal": null,
            "tmux": null,
            "git": null,
            "remoteHost": null,
            "parallel": false
        })
    }

    fn written(stored: &StoredStatus, name: &str) -> Value {
        let path =
            std::env::temp_dir().join(format!("hls-state-{}-{name}.json", std::process::id()));
        stored.write(&path).unwrap();
        let value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        fs::remove_file(&path).unwrap();
        value
    }

    #[test]
    fn legacy_records_resolve_own_phase_from_their_phase() {
        let stored = StoredStatus::from(
            serde_json::from_value::<StoredStatusWire>(legacy_record()).unwrap(),
        );
        assert_eq!(stored.own_phase, Phase::Working);
        assert_eq!(stored.children, Children::new());
        assert!(!stored.held);
    }

    #[test]
    fn records_write_own_phase_and_omit_empty_children() {
        let stored = StoredStatus::from(
            serde_json::from_value::<StoredStatusWire>(legacy_record()).unwrap(),
        );
        let empty = written(&stored, "empty");
        assert_eq!(empty["ownPhase"], "working");
        assert!(empty.get("children").is_none());
        assert!(empty.get("held").is_none());

        let held = written(
            &StoredStatus {
                held: true,
                ..stored.clone()
            },
            "held",
        );
        assert_eq!(held["held"], true);

        let key = crate::children::child_key("worker");
        let with_child = StoredStatus {
            children: Children::from([(key.clone(), Phase::Working)]),
            ..stored
        };
        let value = written(&with_child, "child");
        assert_eq!(value["ownPhase"], "working");
        assert_eq!(value["children"], json!({ key.as_str(): "working" }));
        assert_eq!(key.len(), 16);
    }

    fn candidate_root(pid: u32, observed_at: &str) -> StoredStatus {
        let mut record = legacy_record();
        record["session"]["id"] = json!("p");
        record["observedAt"] = json!(observed_at);
        record["process"] = json!({"pid": pid, "startedAt": "2026-10-08T00:00:00Z", "host": "h"});
        StoredStatus::from(serde_json::from_value::<StoredStatusWire>(record).unwrap())
    }

    #[test]
    fn spawned_children_prefer_the_parent_in_their_own_process() {
        let elsewhere = candidate_root(1, "2026-10-08T00:00:05Z");
        let same_process = candidate_root(2, "2026-10-08T00:00:00Z");
        let records = vec![
            (PathBuf::from("elsewhere"), elsewhere),
            (PathBuf::from("same-process"), same_process),
        ];
        let mut event_value = legacy_record();
        event_value["session"]["id"] = json!("c");
        event_value["process"] =
            json!({"pid": 2, "startedAt": "2026-10-08T00:00:00Z", "host": "h"});
        let event: StatusEvent = serde_json::from_value(event_value).unwrap();

        let found = find_spawn_root(&records, &event, "p").unwrap();
        assert_eq!(found.status.process.as_ref().unwrap().pid, 2);
    }

    #[test]
    fn spawned_children_find_their_parent_across_a_host_rename() {
        let elsewhere = candidate_root(1, "2026-10-08T00:00:05Z");
        let same_process = candidate_root(2, "2026-10-08T00:00:00Z");
        let records = vec![
            (PathBuf::from("elsewhere"), elsewhere),
            (PathBuf::from("same-process"), same_process),
        ];
        let mut event_value = legacy_record();
        event_value["session"]["id"] = json!("c");
        event_value["process"] =
            json!({"pid": 2, "startedAt": "2026-10-08T00:00:00Z", "host": "renamed"});
        let event: StatusEvent = serde_json::from_value(event_value).unwrap();

        let found = find_spawn_root(&records, &event, "p").unwrap();
        assert_eq!(found.status.process.as_ref().unwrap().pid, 2);
    }

    #[test]
    fn child_events_from_a_renamed_host_refresh_the_root_terminal() {
        let root = candidate_root(2, "2026-10-08T00:00:00Z");
        let mut event_value = legacy_record();
        event_value["process"] =
            json!({"pid": 2, "startedAt": "2026-10-08T00:00:00Z", "host": "renamed"});
        event_value["terminal"] = json!({
            "sessionId": "w1t0p0:moved",
            "terminalType": "iterm2",
            "kittyListenOn": null,
            "kittyPid": null
        });
        let event: StatusEvent = serde_json::from_value(event_value).unwrap();

        let update = attach_child(
            &root,
            &event,
            "k",
            Phase::Working,
            true,
            &Details::default(),
        )
        .unwrap();
        let terminal = update.stored.status.terminal.unwrap();
        assert_eq!(terminal.session_id.as_deref(), Some("w1t0p0:moved"));
    }

    #[test]
    fn spawned_children_fall_back_to_the_most_recently_observed_candidate() {
        let older = candidate_root(1, "2026-10-08T00:00:00Z");
        let newer = candidate_root(2, "2026-10-08T00:00:05Z");
        let records = vec![
            (PathBuf::from("older"), older),
            (PathBuf::from("newer"), newer),
        ];
        let mut event_value = legacy_record();
        event_value["session"]["id"] = json!("c");
        event_value["process"] =
            json!({"pid": 3, "startedAt": "2026-10-08T00:00:00Z", "host": "h"});
        let event: StatusEvent = serde_json::from_value(event_value).unwrap();

        let found = find_spawn_root(&records, &event, "p").unwrap();
        assert_eq!(found.status.process.as_ref().unwrap().pid, 2);
    }

    #[test]
    fn children_without_a_known_process_keep_the_root_terminal() {
        let mut record = legacy_record();
        record["terminal"] = json!({
            "sessionId": "w1t0p0:root",
            "terminalType": "iterm2",
            "kittyListenOn": null,
            "kittyPid": null
        });
        let root = StoredStatus::from(serde_json::from_value::<StoredStatusWire>(record).unwrap());
        let mut event_value = legacy_record();
        event_value["session"]["id"] = json!("c");
        event_value["terminal"] = json!({
            "sessionId": "w1t0p0:elsewhere",
            "terminalType": "iterm2",
            "kittyListenOn": null,
            "kittyPid": null
        });
        let event: StatusEvent = serde_json::from_value(event_value).unwrap();

        let update = attach_child(
            &root,
            &event,
            "k",
            Phase::Permission,
            true,
            &Details::default(),
        )
        .unwrap();
        assert_eq!(
            update.stored.status.terminal.unwrap().session_id.as_deref(),
            Some("w1t0p0:root")
        );
        assert_eq!(update.stored.status.phase, Phase::Permission);
    }

    #[test]
    fn an_annotation_keeps_the_hold_children_and_phase() {
        let mut stored = candidate_root(2, "2026-10-08T00:00:00Z");
        stored.own_phase = Phase::Idle;
        stored.held = true;
        stored.children = Children::from([("k".to_string(), Phase::Idle)]);
        stored.status.phase = Phase::Working;
        stored.status.activity.tool = Some("Bash".into());

        let annotated = annotated(
            &stored,
            &Details {
                model: Field::Set("opus".into()),
                ..Details::default()
            },
            &EventTraits {
                kind: DetailKind::Annotation,
                turn_start: true,
                ..EventTraits::default()
            },
        );
        assert!(annotated.held);
        assert_eq!(annotated.own_phase, Phase::Idle);
        assert_eq!(annotated.children, stored.children);
        assert_eq!(annotated.status.phase, Phase::Working);
        assert_eq!(annotated.status.event, stored.status.event);
        assert_eq!(annotated.status.session.model.as_deref(), Some("opus"));
        assert_eq!(annotated.status.activity.tool.as_deref(), Some("Bash"));
        assert_eq!(
            annotated.status.activity.turns,
            stored.status.activity.turns
        );
    }

    #[test]
    fn records_without_details_load_with_empty_details() {
        let stored = StoredStatus::from(
            serde_json::from_value::<StoredStatusWire>(legacy_record()).unwrap(),
        );
        assert_eq!(stored.status.activity, Activity::default());
        assert_eq!(stored.status.session.model, None);
    }

    #[test]
    fn records_from_before_turns_load_with_zero_turns() {
        let mut record = legacy_record();
        record["activity"] = json!({"tool": "Bash", "subagents": 1});
        let stored =
            StoredStatus::from(serde_json::from_value::<StoredStatusWire>(record).unwrap());
        assert_eq!(stored.status.activity.turns, 0);
        assert_eq!(stored.status.activity.subagents, 1);
    }
}
