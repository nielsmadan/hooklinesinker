mod cli;

use clap::Parser;
use cli::{Cli, Command};
use hooklinesinker::consumers::{Consumer, ConsumerStore};
use hooklinesinker::environment::{self, SystemEnv};
use hooklinesinker::paths;
use hooklinesinker::processes::{SystemProcessLookup, now_rfc3339};
use hooklinesinker::protocol::{Agent, PROTOCOL_VERSION};
use hooklinesinker::sinks::UreqHttpClient;
use hooklinesinker::state::{self, HealthProblem, SessionsEnvelope, StatusStore};
use std::io;

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Version { json } => print_version(json),
        Command::Ingest { agent, event } => run_ingest(agent, &event),
        Command::Sessions { json: true } => run_sessions_json(),
        Command::Consumers { json: true } => run_consumers_json(),
        Command::Doctor { json } => run_doctor(json),
        Command::Install { consumer, sink } => run_install(&consumer, sink.as_deref()),
        Command::Uninstall { consumer } => run_uninstall(&consumer),
        _ => not_implemented(),
    }
}

fn print_version(json: bool) {
    if json {
        let envelope = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "version": env!("CARGO_PKG_VERSION"),
        });
        println!("{envelope}");
    } else {
        println!(
            "hooklinesinker {} (protocol {})",
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION
        );
    }
}

fn run_ingest(agent: Agent, event: &str) {
    let Ok(store) = StatusStore::open(paths::state_root()) else {
        std::process::exit(0);
    };

    let input = match state::read_capped(&mut io::stdin(), 1_048_576) {
        Ok(input) => input,
        Err(e) => {
            let _ = store.record_health(&format!("stdin read failed: {e}"));
            std::process::exit(0);
        }
    };

    let consumers = match ConsumerStore::open(paths::state_root()) {
        Ok(consumers) => Some(consumers),
        Err(e) => {
            let _ = store.record_health(&format!("failed to open consumer store: {e}"));
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
    let ctx = state::IngestContext {
        store: &store,
        liveness: &liveness,
        consumers: consumers.as_ref(),
        http_client: &http_client,
    };
    state::handle_ingest(&ctx, agent, event, &input, &env, hook_pid);
    std::process::exit(0);
}

fn run_sessions_json() {
    let envelope = match StatusStore::open(paths::state_root()) {
        Ok(store) => {
            let liveness = SystemProcessLookup::new();
            store.sessions_envelope(&liveness)
        }
        Err(e) => SessionsEnvelope {
            sessions: Vec::new(),
            problems: vec![HealthProblem {
                observed_at: now_rfc3339(),
                message: format!("failed to open state store: {e}"),
            }],
        },
    };
    let json = serde_json::json!({
        "protocol": PROTOCOL_VERSION,
        "sessions": envelope.sessions,
        "problems": envelope.problems,
    });
    println!("{json}");
}

fn run_consumers_json() {
    let (consumers, problems) = match ConsumerStore::open(paths::state_root()) {
        Ok(store) => match store.list() {
            Ok(consumers) => (consumers, Vec::new()),
            Err(e) => (Vec::new(), vec![format!("failed to list consumers: {e}")]),
        },
        Err(e) => (
            Vec::new(),
            vec![format!("failed to open consumer store: {e}")],
        ),
    };
    let json = serde_json::json!({
        "protocol": PROTOCOL_VERSION,
        "consumers": consumers,
        "problems": problems,
    });
    println!("{json}");
}

fn run_install(consumer_name: &str, sink: Option<&str>) {
    let consumer = Consumer {
        name: consumer_name.to_string(),
        protocol: PROTOCOL_VERSION,
        capabilities: vec!["status".to_string()],
        sink: sink.map(str::to_string),
    };
    let result =
        ConsumerStore::open(paths::state_root()).and_then(|store| store.register(consumer));
    match result {
        Ok(()) => {
            println!("registered consumer {consumer_name}");
        }
        Err(e) => {
            eprintln!("failed to register consumer {consumer_name}: {e}");
            std::process::exit(1);
        }
    }
}

fn run_uninstall(consumer_name: &str) {
    let result =
        ConsumerStore::open(paths::state_root()).and_then(|store| store.remove(consumer_name));
    match result {
        Ok(()) => {
            println!("removed consumer {consumer_name}");
        }
        Err(e) => {
            eprintln!("failed to remove consumer {consumer_name}: {e}");
            std::process::exit(1);
        }
    }
}

struct DoctorCheck {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn push_parse_problems_check<T>(
    checks: &mut Vec<DoctorCheck>,
    fatal: &mut bool,
    name: &'static str,
    store_label: &str,
    store: Result<&T, String>,
    parse_problems: impl FnOnce(&T) -> io::Result<Vec<String>>,
) {
    let problems = match store {
        Ok(store) => {
            parse_problems(store).map_err(|e| format!("failed to read {store_label}: {e}"))
        }
        Err(e) => Err(format!("failed to open {store_label}: {e}")),
    };
    match problems {
        Ok(problems) if problems.is_empty() => checks.push(DoctorCheck {
            name,
            ok: true,
            detail: "none".to_string(),
        }),
        Ok(problems) => {
            *fatal = true;
            checks.push(DoctorCheck {
                name,
                ok: false,
                detail: problems.join("; "),
            });
        }
        Err(detail) => {
            *fatal = true;
            checks.push(DoctorCheck {
                name,
                ok: false,
                detail,
            });
        }
    }
}

fn run_doctor(json: bool) {
    let mut checks = Vec::new();
    let mut fatal = false;

    checks.push(DoctorCheck {
        name: "version",
        ok: true,
        detail: format!(
            "hooklinesinker {} (protocol {})",
            env!("CARGO_PKG_VERSION"),
            PROTOCOL_VERSION
        ),
    });

    let status_store = StatusStore::open(paths::state_root());
    let consumer_store = ConsumerStore::open(paths::state_root());

    let permissions = permissions_check(&paths::state_root());
    if !permissions.ok {
        fatal = true;
    }
    checks.push(permissions);

    checks.push(DoctorCheck {
        name: "active_version_target",
        ok: true,
        detail: "not implemented yet: self-install lands in a later task".to_string(),
    });

    push_parse_problems_check(
        &mut checks,
        &mut fatal,
        "consumer_parse_problems",
        "consumer store",
        consumer_store.as_ref().map_err(|e| e.to_string()),
        |store| store.parse_problems(),
    );

    checks.push(DoctorCheck {
        name: "hook_status",
        ok: true,
        detail: "not implemented yet: hook reconciliation lands in a later task".to_string(),
    });

    push_parse_problems_check(
        &mut checks,
        &mut fatal,
        "status_parse_problems",
        "status store",
        status_store.as_ref().map_err(|e| e.to_string()),
        |store| store.parse_problems(),
    );

    match &status_store {
        Ok(store) => {
            let liveness = SystemProcessLookup::new();
            match store.sweep(&liveness) {
                Ok(swept) => checks.push(DoctorCheck {
                    name: "dead_records_removed",
                    ok: true,
                    detail: swept.len().to_string(),
                }),
                Err(e) => checks.push(DoctorCheck {
                    name: "dead_records_removed",
                    ok: true,
                    detail: format!("sweep failed: {e}"),
                }),
            }
        }
        Err(_) => checks.push(DoctorCheck {
            name: "dead_records_removed",
            ok: true,
            detail: "unavailable".to_string(),
        }),
    }

    match &status_store {
        Ok(store) => match store.health_problems() {
            Ok(problems) => {
                let last_sink_error = problems
                    .iter()
                    .rev()
                    .find(|p| p.message.starts_with("sink "));
                checks.push(DoctorCheck {
                    name: "last_sink_error",
                    ok: true,
                    detail: last_sink_error
                        .map(|p| p.message.clone())
                        .unwrap_or_else(|| "none".to_string()),
                });
            }
            Err(e) => checks.push(DoctorCheck {
                name: "last_sink_error",
                ok: true,
                detail: format!("failed to read health problems: {e}"),
            }),
        },
        Err(_) => checks.push(DoctorCheck {
            name: "last_sink_error",
            ok: true,
            detail: "unavailable".to_string(),
        }),
    }

    let exit_code = if fatal { 1 } else { 0 };

    if json {
        let checks_json: Vec<_> = checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "ok": c.ok,
                    "detail": c.detail,
                })
            })
            .collect();
        let envelope = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "version": env!("CARGO_PKG_VERSION"),
            "checks": checks_json,
            "exitCode": exit_code,
        });
        println!("{envelope}");
    } else {
        for check in &checks {
            let status = if check.ok { "ok" } else { "fail" };
            println!("[{status}] {}: {}", check.name, check.detail);
        }
    }

    std::process::exit(exit_code);
}

#[cfg(unix)]
fn permissions_check(root: &std::path::Path) -> DoctorCheck {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(root) {
        Ok(metadata) if metadata.permissions().mode() & 0o777 == 0o700 => DoctorCheck {
            name: "permissions",
            ok: true,
            detail: format!("{} is 0700", root.display()),
        },
        Ok(metadata) => DoctorCheck {
            name: "permissions",
            ok: false,
            detail: format!(
                "{} is {:o}, expected 0700",
                root.display(),
                metadata.permissions().mode() & 0o777
            ),
        },
        Err(e) => DoctorCheck {
            name: "permissions",
            ok: true,
            detail: format!("{} does not exist yet: {e}", root.display()),
        },
    }
}

#[cfg(not(unix))]
fn permissions_check(_root: &std::path::Path) -> DoctorCheck {
    DoctorCheck {
        name: "permissions",
        ok: true,
        detail: "permission checks apply only on unix".to_string(),
    }
}

fn not_implemented() -> ! {
    eprintln!("not implemented yet");
    std::process::exit(2);
}
