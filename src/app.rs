#[path = "cli.rs"]
mod cli;

use crate::consumers::ConsumerStore;
use crate::environment::{self, SystemEnv};
use crate::hooks::{HookManager, HookRoots, HookState, HookStatus};
use crate::ingest::{self, IngestContext};
use crate::install::{InstalledVersion, Installer, UninstallOutcome};
use crate::paths;
use crate::processes::{SystemProcessLiveness, SystemProcessLookup};
use crate::protocol::{
    Agent, Capability, Consumer, ConsumersResponse, DoctorCheck, DoctorResponse, PROTOCOL_VERSION,
    ProtocolResponse, SessionsResponse, VersionResponse,
};
use crate::sinks::UreqHttpClient;
use crate::state::{HealthKind, HealthProblem, SessionsEnvelope, StatusStore};
use clap::Parser;
use cli::{Cli, Command, HooksCommand};
use std::io::{self, Write};

macro_rules! stdoutln {
    ($($arg:tt)*) => {
        write_stdout(format_args!($($arg)*))
    };
}

pub fn run() {
    let cli = Cli::parse();

    match cli.command {
        Command::Version { json } => print_version(json),
        Command::Ingest { agent, event } => run_ingest(agent, &event),
        Command::Sessions { json } => run_sessions(json),
        Command::Consumers { json } => run_consumers(json),
        Command::Doctor { json } => run_doctor(json),
        Command::Install {
            consumer,
            sink,
            no_sink,
        } => run_install(&consumer, sink.as_deref(), no_sink),
        Command::Uninstall { consumer } => run_uninstall(&consumer),
        Command::Hooks { command } => run_hooks(command),
    }
}

fn open_installer() -> io::Result<Installer> {
    Installer::open(paths::data_root()?)
}

fn hook_manager(installer: &Installer) -> io::Result<HookManager> {
    HookRoots::from_env(installer.binary_path()).map(HookManager::new)
}

fn print_version(json: bool) {
    if json {
        print_json(&VersionResponse {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
        });
    } else {
        stdoutln!(
            "hooklinesinker {} (protocol {})",
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION
        );
    }
}

fn run_ingest(agent: Agent, event: &str) {
    let Ok(state_root) = paths::state_root() else {
        std::process::exit(0);
    };
    let Ok(store) = StatusStore::open(&state_root) else {
        std::process::exit(0);
    };

    let input = match ingest::read_capped(&mut io::stdin(), 1_048_576) {
        Ok(input) => input,
        Err(e) => {
            let _ = store.record_health(HealthProblem::new(
                HealthKind::Ingest,
                format!("stdin read failed: {e}"),
            ));
            std::process::exit(0);
        }
    };

    let consumers = match ConsumerStore::open(&state_root) {
        Ok(consumers) => Some(consumers),
        Err(e) => {
            let _ = store.record_health(HealthProblem::new(
                HealthKind::Other,
                format!("failed to open consumer store: {e}"),
            ));
            None
        }
    };

    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_default();
    let env = environment::gather_environment(&SystemEnv, cwd);
    let liveness = SystemProcessLookup::new();
    let http_client = UreqHttpClient::new();
    let hook_pid = std::process::id();
    let ctx = IngestContext {
        store: &store,
        liveness: &liveness,
        consumers: consumers.as_ref(),
        http_client: &http_client,
    };
    let outcome = ingest::handle_ingest(&ctx, agent, event, &input, &env, hook_pid);
    // Agent hosts ignore hook stderr, so this surfaces the failure to a human running the
    // command by hand without touching ingest's exit-0 contract.
    if let Some(problem) = outcome.problem {
        eprintln!("ingest problem: {problem}");
    }
    std::process::exit(0);
}

fn run_sessions(json: bool) {
    // A read that answered exits 0 even when the health log carries problems; only an
    // unreadable store means the query itself failed.
    let mut answered = true;
    let envelope = match paths::state_root().and_then(StatusStore::open) {
        Ok(store) => store.sessions_envelope(&SystemProcessLiveness),
        Err(e) => {
            answered = false;
            SessionsEnvelope {
                sessions: Vec::new(),
                problems: vec![HealthProblem::new(
                    HealthKind::Other,
                    format!("failed to open state store: {e}"),
                )],
            }
        }
    };
    if json {
        print_json(&SessionsResponse {
            protocol: PROTOCOL_VERSION,
            sessions: envelope.sessions,
            problems: envelope.problems,
        });
        return;
    }
    if envelope.sessions.is_empty() {
        stdoutln!("no running sessions");
    } else {
        for session in envelope.sessions {
            stdoutln!(
                "{}\t{}\t{}\t{}",
                session.agent.as_str(),
                session.session.id,
                session.phase.as_str(),
                session.session.cwd
            );
        }
    }
    for problem in &envelope.problems {
        eprintln!("problem: {}", problem.message);
    }
    if !answered {
        std::process::exit(1);
    }
}

fn run_consumers(json: bool) {
    let mut answered = true;
    let (consumers, problems) = match paths::state_root().and_then(ConsumerStore::open) {
        Ok(store) => match store.snapshot() {
            Ok(snapshot) => (snapshot.consumers, snapshot.problems),
            Err(e) => {
                answered = false;
                (Vec::new(), vec![format!("failed to list consumers: {e}")])
            }
        },
        Err(e) => {
            answered = false;
            (
                Vec::new(),
                vec![format!("failed to open consumer store: {e}")],
            )
        }
    };
    if json {
        print_json(&ConsumersResponse {
            protocol: PROTOCOL_VERSION,
            consumers,
            problems,
        });
        return;
    }
    if consumers.is_empty() {
        stdoutln!("no registered consumers");
    } else {
        for consumer in consumers {
            let sink = consumer.sink.as_deref().unwrap_or("poll");
            stdoutln!(
                "{}\tprotocol {}\t{}\t{sink}",
                consumer.name,
                consumer.protocol,
                consumer
                    .capabilities
                    .iter()
                    .map(|capability| capability.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            );
        }
    }
    for problem in &problems {
        eprintln!("problem: {problem}");
    }
    if !answered {
        std::process::exit(1);
    }
}

fn run_install(consumer_name: &str, sink: Option<&str>, no_sink: bool) {
    let mut registered_sink = None;
    let result = (|| -> io::Result<InstalledVersion> {
        let installer = open_installer()?;
        let consumers = ConsumerStore::open(paths::state_root()?)?;
        // Consumers re-run install on every startup, so an omitted --sink keeps the
        // registered one rather than silently downgrading a push consumer to polling.
        let resolved = match (sink, no_sink) {
            (Some(sink), _) => Some(sink.to_string()),
            (None, true) => None,
            (None, false) => consumers.get(consumer_name)?.and_then(|c| c.sink),
        };
        registered_sink.clone_from(&resolved);
        let consumer = Consumer::new(consumer_name, vec![Capability::Status], resolved)?;
        installer.install_current(&consumers, &consumer)
    })();
    match result {
        Ok(installed) => {
            let sink = registered_sink.as_deref().unwrap_or("poll");
            stdoutln!(
                "registered consumer {consumer_name} (active version {}, protocol {}, sink {sink})",
                installed.active_version,
                installed.protocol_major
            );
        }
        Err(e) => {
            eprintln!("failed to register consumer {consumer_name}: {e}");
            std::process::exit(1);
        }
    }
}

fn run_uninstall(consumer_name: &str) {
    let result = (|| -> io::Result<UninstallOutcome> {
        let installer = open_installer()?;
        let consumers = ConsumerStore::open(paths::state_root()?)?;
        let hooks = hook_manager(&installer)?;
        installer.uninstall_consumer(&consumers, &hooks, consumer_name)
    })();
    match result {
        Ok(outcome) if outcome.was_last_consumer => {
            stdoutln!(
                "removed consumer {consumer_name} (last consumer; hooks and active binary removed)"
            );
            for leftover in &outcome.leftover_hooks {
                eprintln!("problem: {leftover}");
            }
        }
        Ok(_) => {
            stdoutln!("removed consumer {consumer_name}");
        }
        Err(e) => {
            eprintln!("failed to remove consumer {consumer_name}: {e}");
            std::process::exit(1);
        }
    }
}

fn run_hooks(command: HooksCommand) {
    match command {
        HooksCommand::Install { agent } => run_hooks_install(agent),
        HooksCommand::Status { agent, json } => run_hooks_status(agent, json),
        HooksCommand::Uninstall { agent } => run_hooks_uninstall(agent),
    }
}

fn run_hooks_install(agent: Agent) {
    let result = (|| -> io::Result<HookStatus> {
        let installer = open_installer()?;
        if installer.active_version_summary()?.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no active hooklinesinker installation; run install --consumer NAME first",
            ));
        }
        let hooks = hook_manager(&installer)?;
        hooks.install(agent)
    })();
    match result {
        Ok(status) if status.state == HookState::Installed => print_hook_status_human(&status),
        Ok(status) => {
            eprintln!(
                "failed to install {} hooks: resulting state is {} ({})",
                agent.as_str(),
                status.state.as_str(),
                status.path.display()
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("failed to install {} hooks: {e}", agent.as_str());
            std::process::exit(1);
        }
    }
}

fn run_hooks_uninstall(agent: Agent) {
    match with_hook_manager(|hooks| hooks.uninstall(agent)) {
        Ok(status) if status.state == HookState::Missing => print_hook_status_human(&status),
        Ok(status) => {
            eprintln!(
                "failed to uninstall {} hooks: resulting state is {} ({})",
                agent.as_str(),
                status.state.as_str(),
                status.path.display()
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("failed to uninstall {} hooks: {e}", agent.as_str());
            std::process::exit(1);
        }
    }
}

fn run_hooks_status(agent: Agent, json: bool) {
    match with_hook_manager(|hooks| hooks.status(agent)) {
        Ok(status) if json => print_hook_status_json(&status),
        Ok(status) => print_hook_status_human(&status),
        Err(e) => {
            eprintln!("failed to read {} hook status: {e}", agent.as_str());
            std::process::exit(1);
        }
    }
}

fn with_hook_manager(
    f: impl FnOnce(&HookManager) -> io::Result<HookStatus>,
) -> io::Result<HookStatus> {
    let installer = open_installer()?;
    let hooks = hook_manager(&installer)?;
    f(&hooks)
}

fn print_hook_status_json(status: &HookStatus) {
    print_json(&ProtocolResponse {
        protocol: PROTOCOL_VERSION,
        payload: status,
    });
}

fn print_hook_status_human(status: &HookStatus) {
    let reason = status
        .reason
        .map(|reason| format!(" ({reason})"))
        .unwrap_or_default();
    stdoutln!(
        "[{}] {}{reason}: {}",
        status.agent.as_str(),
        status.state.as_str(),
        status.path.display()
    );
}

fn parse_problems_check<T>(
    name: &str,
    store_label: &str,
    store: Result<&T, String>,
    parse_problems: impl FnOnce(&T) -> io::Result<Vec<String>>,
) -> DoctorCheck {
    let problems = match store {
        Ok(store) => {
            parse_problems(store).map_err(|e| format!("failed to read {store_label}: {e}"))
        }
        Err(e) => Err(format!("failed to open {store_label}: {e}")),
    };
    match problems {
        Ok(problems) if problems.is_empty() => DoctorCheck {
            name: name.to_string(),
            ok: true,
            detail: "none".to_string(),
        },
        Ok(problems) => DoctorCheck {
            name: name.to_string(),
            ok: false,
            detail: problems.join("; "),
        },
        Err(detail) => DoctorCheck {
            name: name.to_string(),
            ok: false,
            detail,
        },
    }
}

fn run_doctor(json: bool) {
    let mut checks = Vec::new();
    let mut fatal = false;

    checks.push(DoctorCheck {
        name: "version".to_string(),
        ok: true,
        detail: format!(
            "hooklinesinker {} (protocol {})",
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION
        ),
    });

    let (permissions, status_store, consumer_store) = open_doctor_stores();
    if !permissions.ok {
        fatal = true;
    }
    checks.push(permissions);

    let active_version_check = doctor_active_version();
    if !active_version_check.ok {
        fatal = true;
    }
    checks.push(active_version_check);

    let consumer_parse_check = parse_problems_check(
        "consumer_parse_problems",
        "consumer store",
        consumer_store.as_ref().map_err(ToString::to_string),
        ConsumerStore::parse_problems,
    );
    if !consumer_parse_check.ok {
        fatal = true;
    }
    checks.push(consumer_parse_check);

    let hook_status_check = doctor_hook_status();
    if !hook_status_check.ok {
        fatal = true;
    }
    checks.push(hook_status_check);

    let status_parse_check = parse_problems_check(
        "status_parse_problems",
        "status store",
        status_store.as_ref().map_err(ToString::to_string),
        StatusStore::parse_problems,
    );
    if !status_parse_check.ok {
        fatal = true;
    }
    checks.push(status_parse_check);

    match &status_store {
        Ok(store) => match store.dead_records(&SystemProcessLiveness) {
            Ok(count) => checks.push(DoctorCheck {
                name: "dead_records".to_string(),
                ok: true,
                detail: count.to_string(),
            }),
            Err(e) => {
                fatal = true;
                checks.push(DoctorCheck {
                    name: "dead_records".to_string(),
                    ok: false,
                    detail: format!("failed to count dead records: {e}"),
                });
            }
        },
        Err(_) => checks.push(DoctorCheck {
            name: "dead_records".to_string(),
            ok: true,
            detail: "unavailable".to_string(),
        }),
    }

    let last_sink_error = doctor_last_sink_error(&status_store);
    if !last_sink_error.ok {
        fatal = true;
    }
    checks.push(last_sink_error);

    let ingest_problems = doctor_recent_ingest_problems(&status_store);
    if !ingest_problems.ok {
        fatal = true;
    }
    checks.push(ingest_problems);

    let exit_code = i32::from(fatal);

    if json {
        print_json(&DoctorResponse {
            protocol: PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
            checks,
            exit_code,
        });
    } else {
        for check in &checks {
            let status = if check.ok { "ok" } else { "fail" };
            stdoutln!("[{status}] {}: {}", check.name, check.detail);
        }
    }

    std::process::exit(exit_code);
}

fn print_json(value: &impl serde::Serialize) {
    stdoutln!(
        "{}",
        serde_json::to_string(value).expect("wire response always serializes")
    );
}

fn write_stdout(args: std::fmt::Arguments<'_>) {
    let mut stdout = io::stdout().lock();
    if let Err(e) = writeln!(stdout, "{args}") {
        if e.kind() == io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("failed to write stdout: {e}");
        std::process::exit(1);
    }
}

fn open_doctor_stores() -> (
    DoctorCheck,
    io::Result<StatusStore>,
    io::Result<ConsumerStore>,
) {
    match paths::state_root() {
        Ok(state_root) => (
            permissions_check(&state_root),
            StatusStore::open(&state_root),
            ConsumerStore::open(&state_root),
        ),
        Err(e) => {
            let detail = format!("invalid state root: {e}");
            (
                DoctorCheck {
                    name: "permissions".to_string(),
                    ok: false,
                    detail: detail.clone(),
                },
                Err(io::Error::new(io::ErrorKind::InvalidInput, detail.clone())),
                Err(io::Error::new(io::ErrorKind::InvalidInput, detail)),
            )
        }
    }
}

#[cfg(unix)]
fn permissions_check(root: &std::path::Path) -> DoctorCheck {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(root) {
        Ok(metadata) if metadata.permissions().mode() & 0o777 == 0o700 => DoctorCheck {
            name: "permissions".to_string(),
            ok: true,
            detail: format!("{} is 0700", root.display()),
        },
        Ok(metadata) => DoctorCheck {
            name: "permissions".to_string(),
            ok: false,
            detail: format!(
                "{} is {:o}, expected 0700",
                root.display(),
                metadata.permissions().mode() & 0o777
            ),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => DoctorCheck {
            name: "permissions".to_string(),
            ok: true,
            detail: format!("{} does not exist yet", root.display()),
        },
        Err(e) => DoctorCheck {
            name: "permissions".to_string(),
            ok: false,
            detail: format!("failed to inspect {}: {e}", root.display()),
        },
    }
}

#[cfg(not(unix))]
fn permissions_check(_root: &std::path::Path) -> DoctorCheck {
    DoctorCheck {
        name: "permissions".to_string(),
        ok: true,
        detail: "permission checks apply only on unix".to_string(),
    }
}

fn doctor_hook_status() -> DoctorCheck {
    match open_installer() {
        Ok(installer) => {
            let hooks = match hook_manager(&installer) {
                Ok(hooks) => hooks,
                Err(e) => {
                    return DoctorCheck {
                        name: "hook_status".to_string(),
                        ok: false,
                        detail: format!("failed to resolve hook roots: {e}"),
                    };
                }
            };
            let mut summaries = Vec::new();
            let mut problems = Vec::new();
            for agent in Agent::ALL {
                match hooks.status(agent) {
                    Ok(status) => {
                        summaries.push(format!("{}={}", agent.as_str(), status.state.as_str()));
                        if matches!(status.state, HookState::Drifted | HookState::Unsupported) {
                            let reason = status
                                .reason
                                .map(|reason| format!(" ({reason})"))
                                .unwrap_or_default();
                            problems.push(format!(
                                "{} hooks are {}{reason} at {}",
                                agent.as_str(),
                                status.state.as_str(),
                                status.path.display()
                            ));
                        }
                    }
                    Err(e) => {
                        problems.push(format!("{} hook status failed: {e}", agent.as_str()));
                    }
                }
            }
            DoctorCheck {
                name: "hook_status".to_string(),
                ok: problems.is_empty(),
                detail: if problems.is_empty() {
                    summaries.join(", ")
                } else {
                    problems.join("; ")
                },
            }
        }
        Err(e) => DoctorCheck {
            name: "hook_status".to_string(),
            ok: false,
            detail: format!("failed to prepare hook installer: {e}"),
        },
    }
}

fn doctor_active_version() -> DoctorCheck {
    match open_installer().and_then(|i| i.active_version_summary()) {
        Ok(Some(installed)) => DoctorCheck {
            name: "active_version_target".to_string(),
            ok: true,
            detail: format!(
                "v{} (protocol {})",
                installed.active_version, installed.protocol_major
            ),
        },
        Ok(None) => DoctorCheck {
            name: "active_version_target".to_string(),
            ok: true,
            detail: "not installed".to_string(),
        },
        Err(e) => DoctorCheck {
            name: "active_version_target".to_string(),
            ok: false,
            detail: format!("failed to read active version: {e}"),
        },
    }
}

// Dropped hook events are only visible in the health log, so doctor has to read it or a
// push consumer never learns its status stopped updating.
fn doctor_recent_ingest_problems(status_store: &io::Result<StatusStore>) -> DoctorCheck {
    let name = "ingest_problems".to_string();
    match status_store {
        Ok(store) => match store.health_problems() {
            Ok(problems) => {
                let now = crate::processes::Timestamp::now();
                let recent: Vec<&str> = problems
                    .iter()
                    .filter(|p| p.is_ingest_failure() && p.is_recent_problem(now))
                    .map(|p| p.message.as_str())
                    .collect();
                if recent.is_empty() {
                    DoctorCheck {
                        name,
                        ok: true,
                        detail: "none".to_string(),
                    }
                } else {
                    DoctorCheck {
                        name,
                        ok: false,
                        detail: format!("{} recent: {}", recent.len(), recent.join("; ")),
                    }
                }
            }
            Err(e) => DoctorCheck {
                name,
                ok: false,
                detail: format!("failed to read health problems: {e}"),
            },
        },
        Err(e) => DoctorCheck {
            name,
            ok: false,
            detail: format!("state store unavailable: {e}"),
        },
    }
}

fn doctor_last_sink_error(status_store: &io::Result<StatusStore>) -> DoctorCheck {
    match status_store {
        Ok(store) => match store.health_problems() {
            Ok(problems) => DoctorCheck {
                name: "last_sink_error".to_string(),
                ok: true,
                detail: problems
                    .iter()
                    .rev()
                    .find(|p| p.is_sink_failure())
                    .map_or_else(|| "none".to_string(), |p| p.message.clone()),
            },
            Err(e) => DoctorCheck {
                name: "last_sink_error".to_string(),
                ok: false,
                detail: format!("failed to read health problems: {e}"),
            },
        },
        Err(e) => DoctorCheck {
            name: "last_sink_error".to_string(),
            ok: false,
            detail: format!("state store unavailable: {e}"),
        },
    }
}
