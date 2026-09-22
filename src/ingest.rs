use crate::consumers::ConsumerStore;
use crate::lifecycle::SessionContext;
use crate::normalize::{HookEnvironment, Normalized, normalize};
use crate::processes::ProcessLookup;
use crate::protocol::{Agent, StatusEvent};
use crate::sinks::{HttpClient, SinkFanout};
use crate::state::{HealthKind, HealthProblem, StatusStore, SweepOutcome};
use std::io::{self, Read};
use std::time::{Duration, Instant};

// A sweep backlog is unbounded, each sink send costs up to ~600ms of ureq timeouts, and
// Codex caps a hook at 3s. Deliver what fits and record the rest as a health problem.
const SINK_FANOUT_BUDGET: Duration = Duration::from_millis(1_000);

pub(crate) fn read_capped(reader: &mut impl Read, limit: u64) -> io::Result<String> {
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

pub(crate) struct IngestOutcome {
    pub problem: Option<String>,
}

pub(crate) struct IngestContext<'a> {
    pub store: &'a StatusStore,
    pub liveness: &'a dyn ProcessLookup,
    pub consumers: Option<&'a ConsumerStore>,
    pub http_client: &'a dyn HttpClient,
}

pub(crate) fn handle_ingest(
    ctx: &IngestContext,
    agent: Agent,
    event: &str,
    input: &str,
    env: &HookEnvironment,
    hook_pid: u32,
) -> IngestOutcome {
    let store = ctx.store;
    let process = ctx.liveness.owner_of(hook_pid, agent);
    let mut forwarded: Option<StatusEvent> = None;
    let mut problems: Vec<HealthProblem> = Vec::new();
    let mut retired = SweepOutcome::default();
    let mut swept_with_ingest = false;

    let outcome =
        normalize(agent, event, input, env, process).and_then(|normalized| match normalized {
            Normalized::Ignored => Ok(()),
            Normalized::Unrecognized => {
                problems.push(HealthProblem::new(
                    HealthKind::Ingest,
                    format!(
                        "unrecognized {} event \"{event}\"; no status was recorded",
                        agent.as_str()
                    ),
                ));
                Ok(())
            }
            Normalized::ForwardOnly(status_event) => {
                forwarded = Some(status_event);
                Ok(())
            }
            Normalized::Recordable(mut status_event) => {
                let mut context = SessionContext::parse(agent, event, input)?;
                if status_event
                    .process
                    .as_ref()
                    .is_some_and(|process| !ctx.liveness.has_exclusive_session(process, agent))
                {
                    context = context.in_shared_process();
                }
                let session_outcome =
                    store.ingest_session(&mut status_event, context, ctx.liveness)?;
                if session_outcome.forward {
                    forwarded = Some(status_event);
                }
                retired = session_outcome.retired;
                swept_with_ingest = true;
                Ok(())
            }
        });

    let ingest_failed = outcome.is_err();
    if let Err(e) = outcome {
        problems.push(HealthProblem::new(HealthKind::Ingest, e.to_string()));
    }
    problems.extend(
        retired
            .problems
            .into_iter()
            .map(|message| HealthProblem::new(HealthKind::Sweep, message)),
    );
    let mut swept = retired.events;
    // A failed ingest has usually just timed out on the store lock; retrying the sweep would
    // wait on the same lock a second time and blow the host's hook budget.
    if !swept_with_ingest && !ingest_failed {
        match store.sweep(ctx.liveness) {
            Ok(outcome) => {
                problems.extend(
                    outcome
                        .problems
                        .into_iter()
                        .map(|message| HealthProblem::new(HealthKind::Sweep, message)),
                );
                swept.extend(outcome.events);
            }
            Err(e) => {
                problems.push(HealthProblem::new(
                    HealthKind::Sweep,
                    format!("sweep failed: {e}"),
                ));
            }
        }
    }

    problems.extend(fan_out_to_sinks(ctx, forwarded.as_ref(), swept));
    let messages: Vec<String> = problems.iter().map(|p| p.message.clone()).collect();
    let mut problem = (!messages.is_empty()).then(|| messages.join("; "));
    if !problems.is_empty()
        && let Err(e) = store.record_health_many(problems)
    {
        let health_error = format!("failed to record health problems: {e}");
        problem = Some(match problem {
            Some(existing) => format!("{existing}; {health_error}"),
            None => health_error,
        });
    }

    IngestOutcome { problem }
}

fn fan_out_to_sinks(
    ctx: &IngestContext,
    forwarded: Option<&StatusEvent>,
    swept: Vec<StatusEvent>,
) -> Vec<HealthProblem> {
    let mut problems = Vec::new();
    if forwarded.is_none() && swept.is_empty() {
        return problems;
    }
    let Some(consumers) = ctx.consumers else {
        return problems;
    };

    let snapshot = match consumers.snapshot() {
        Ok(snapshot) => snapshot,
        Err(e) => {
            problems.push(HealthProblem::new(
                HealthKind::Other,
                format!("sink consumer lookup failed: {e}"),
            ));
            return problems;
        }
    };
    for problem in snapshot.problems {
        problems.push(HealthProblem::new(
            HealthKind::Other,
            format!("sink consumer lookup failed: {problem}"),
        ));
    }
    let status_consumers = snapshot.consumers;
    if status_consumers.is_empty() {
        return problems;
    }

    let fanout = SinkFanout::new(ctx.http_client);
    let mut collect_problems = |sink_problems: Vec<crate::sinks::SinkProblem>| {
        for problem in sink_problems {
            // The message keeps its existing wording for consumers reading `problems[].message`;
            // the kind is what doctor classifies on.
            let message = format!("sink {} failed: {}", problem.consumer, problem.message);
            problems.push(HealthProblem::new(
                HealthKind::Sink {
                    consumer: problem.consumer,
                },
                message,
            ));
        }
    };

    let deadline = Instant::now() + SINK_FANOUT_BUDGET;
    let mut undelivered = 0usize;
    if let Some(event) = forwarded {
        let outcome = fanout.send_until(event, &status_consumers, Some(deadline));
        undelivered += outcome.undelivered;
        collect_problems(outcome.problems);
    }
    for swept_event in swept {
        let outcome = fanout.send_until(
            &synthetic_swept_event(swept_event),
            &status_consumers,
            Some(deadline),
        );
        undelivered += outcome.undelivered;
        collect_problems(outcome.problems);
    }
    if undelivered > 0 {
        problems.push(HealthProblem::new(
            HealthKind::Sweep,
            format!(
                "sink fan-out exceeded its {}ms budget; {undelivered} sink delivery attempt(s) were skipped",
                SINK_FANOUT_BUDGET.as_millis()
            ),
        ));
    }
    problems
}

fn synthetic_swept_event(mut swept: StatusEvent) -> StatusEvent {
    // The `swept` event name is part of the protocol 1 consumer contract.
    swept.event = "swept".to_string();
    swept.running = false;
    swept.observed_at = crate::processes::now_rfc3339();
    swept
}
