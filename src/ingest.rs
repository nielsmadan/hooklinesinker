use crate::agents::{ChildTracking, profile};
use crate::children::ChildEvent;
use crate::consumers::ConsumerStore;
use crate::details::{Details, EventTraits};
use crate::lifecycle::{ChildClaim, SessionContext, awaits_user, clears_children};
use crate::normalize::{HookEnvironment, Normalized, normalize};
use crate::processes::ProcessLookup;
use crate::protocol::{Agent, StatusEvent};
use crate::sinks::{HttpClient, SinkFanout};
use crate::state::{
    CurrentStatus, HealthKind, HealthProblem, IngestSessionOutcome, IngestSignals, ResetOutcome,
    StatusStore, SweepOutcome,
};
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
    let process = ctx.liveness.owner_of(hook_pid, agent);
    handle_normalized(
        ctx,
        agent,
        event,
        input,
        normalize(agent, event, input, env, process),
    )
}

pub(crate) fn handle_normalized(
    ctx: &IngestContext,
    agent: Agent,
    event: &str,
    input: &str,
    normalized: io::Result<Normalized>,
) -> IngestOutcome {
    let store = ctx.store;
    let mut forwarded: Option<StatusEvent> = None;
    let mut stored = false;
    let mut problems: Vec<HealthProblem> = Vec::new();
    let mut retired = SweepOutcome::default();
    let mut swept_with_ingest = false;

    let outcome = normalized.and_then(|normalized| match normalized {
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
        Normalized::Recordable(mut status_event, details, traits) => {
            let session_outcome = record_session(
                ctx,
                agent,
                event,
                input,
                &mut status_event,
                &details,
                traits,
            )?;
            if session_outcome.forward {
                forwarded = Some(status_event);
                stored = true;
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

    let deadline = Instant::now() + SINK_FANOUT_BUDGET;
    let fan_out = fan_out_to_sinks(ctx, forwarded.as_ref(), swept, deadline);
    problems.extend(fan_out.problems);
    if needs_convergence(profile(agent).child_tracking, stored, fan_out.had_consumers)
        && let Some(sent) = forwarded.as_ref()
    {
        problems.extend(converge_on_stored_status(ctx, sent, deadline));
    }
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

// A consumer whose sink is down must not turn a completed reset into a failed one.
pub(crate) fn reset_sessions(ctx: &IngestContext) -> io::Result<ResetOutcome> {
    let mut outcome = ctx.store.reset(ctx.liveness)?;
    let deadline = Instant::now() + SINK_FANOUT_BUDGET;
    let delivery = fan_out_to_sinks(ctx, None, outcome.ended.clone(), deadline).problems;
    if !delivery.is_empty()
        && let Err(e) = ctx.store.record_health_many(delivery)
    {
        outcome
            .problems
            .push(format!("failed to record health problems: {e}"));
    }
    Ok(outcome)
}

fn record_session(
    ctx: &IngestContext,
    agent: Agent,
    event: &str,
    input: &str,
    status_event: &mut StatusEvent,
    details: &Details,
    traits: EventTraits,
) -> io::Result<IngestSessionOutcome> {
    let mut context = SessionContext::parse(agent, event, input)?;
    if status_event
        .process
        .as_ref()
        .is_some_and(|process| !ctx.liveness.has_exclusive_session(process, agent))
    {
        context = context.in_shared_process();
    }
    let signals = IngestSignals {
        context,
        claim: ChildClaim::parse(agent, event, input, &status_event.session.id)?,
        child: ChildEvent {
            event,
            mapped: status_event.phase,
            running: status_event.running,
            awaiting_user: awaits_user(agent, event, input),
        },
        clears_children: clears_children(event, input),
        details,
        traits,
    };
    ctx.store
        .ingest_session(status_event, &signals, ctx.liveness)
}

const fn needs_convergence(
    child_tracking: ChildTracking,
    stored: bool,
    had_consumers: bool,
) -> bool {
    stored && had_consumers && !matches!(child_tracking, ChildTracking::Untracked)
}

fn resend_labelled(status: &StatusEvent) -> StatusEvent {
    StatusEvent {
        event: format!("resend:{}", status.event),
        ..status.clone()
    }
}

fn diverged(current: &StatusEvent, sent: &StatusEvent) -> bool {
    current.phase != sent.phase
        || current.running != sent.running
        || current.session != sent.session
        || current.activity != sent.activity
}

// Fan-out runs unlocked, so a concurrent ingest can deliver an older aggregate after ours.
fn converge_on_stored_status(
    ctx: &IngestContext,
    sent: &StatusEvent,
    deadline: Instant,
) -> Vec<HealthProblem> {
    match ctx.store.current_status(sent) {
        Ok(CurrentStatus::Stored(current)) if diverged(&current, sent) => {
            fan_out_to_sinks(ctx, Some(&resend_labelled(&current)), Vec::new(), deadline).problems
        }
        Ok(CurrentStatus::Adopted(root)) if sent.running => {
            fan_out_to_sinks(
                ctx,
                Some(&resend_labelled(&root)),
                vec![sent.clone()],
                deadline,
            )
            .problems
        }
        Ok(CurrentStatus::Gone) if sent.running => {
            let mut removed = sent.clone();
            removed.running = false;
            fan_out_to_sinks(ctx, Some(&removed), Vec::new(), deadline).problems
        }
        Ok(_) => Vec::new(),
        Err(e) => vec![HealthProblem::new(
            HealthKind::Ingest,
            format!("failed to re-read the session status after delivery: {e}"),
        )],
    }
}

struct FanOutOutcome {
    problems: Vec<HealthProblem>,
    had_consumers: bool,
}

fn fan_out_to_sinks(
    ctx: &IngestContext,
    forwarded: Option<&StatusEvent>,
    swept: Vec<StatusEvent>,
    deadline: Instant,
) -> FanOutOutcome {
    let mut problems = Vec::new();
    if forwarded.is_none() && swept.is_empty() {
        return FanOutOutcome {
            problems,
            had_consumers: false,
        };
    }
    let Some(consumers) = ctx.consumers else {
        return FanOutOutcome {
            problems,
            had_consumers: false,
        };
    };

    let snapshot = match consumers.snapshot() {
        Ok(snapshot) => snapshot,
        Err(e) => {
            problems.push(HealthProblem::new(
                HealthKind::Other,
                format!("sink consumer lookup failed: {e}"),
            ));
            return FanOutOutcome {
                problems,
                had_consumers: false,
            };
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
        return FanOutOutcome {
            problems,
            had_consumers: false,
        };
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
    FanOutOutcome {
        problems,
        had_consumers: true,
    }
}

fn synthetic_swept_event(mut swept: StatusEvent) -> StatusEvent {
    // The `swept` event name is part of the protocol 1 consumer contract.
    swept.event = "swept".to_string();
    swept.running = false;
    swept.observed_at = crate::processes::now_rfc3339();
    swept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convergence_needs_a_stored_tracked_status_that_reached_a_consumer() {
        let tracked = ChildTracking::LinkedSessions {
            child_stop: "Stop",
            completion_wakes_root: true,
        };
        let cases = [
            (tracked, true, true, true),
            (tracked, true, false, false),
            (tracked, false, true, false),
            (ChildTracking::Untracked, true, true, false),
        ];
        for (child_tracking, stored, had_consumers, expected) in cases {
            assert_eq!(
                needs_convergence(child_tracking, stored, had_consumers),
                expected,
                "{child_tracking:?} stored={stored} had_consumers={had_consumers}"
            );
        }
    }

    #[test]
    fn convergence_resends_when_only_the_details_moved_on() {
        let sent: StatusEvent = serde_json::from_value(serde_json::json!({
            "protocol": 1,
            "bindingId": "b",
            "agent": "codex",
            "event": "PreToolUse",
            "phase": "working",
            "running": true,
            "observedAt": "2026-10-30T00:00:00Z",
            "session": {"id": "a", "cwd": "/tmp", "transcriptPath": null},
            "process": null,
            "terminal": null,
            "tmux": null,
            "git": null,
            "remoteHost": null
        }))
        .unwrap();
        assert!(!diverged(&sent, &sent));

        let mut counted = sent.clone();
        counted.activity.subagents = 2;
        assert!(diverged(&counted, &sent));

        let mut switched = sent.clone();
        switched.session.model = Some("gpt-5.5-codex".into());
        assert!(diverged(&switched, &sent));
    }
}
