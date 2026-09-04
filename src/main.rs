mod cli;

use clap::Parser;
use cli::{Cli, Command};
use hooklinesinker::environment::{self, SystemEnv};
use hooklinesinker::paths;
use hooklinesinker::processes::{SystemProcessLookup, now_rfc3339};
use hooklinesinker::protocol::{Agent, PROTOCOL_VERSION};
use hooklinesinker::state::{self, HealthProblem, SessionsEnvelope, StatusStore};
use std::io;

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Version { json } => print_version(json),
        Command::Ingest { agent, event } => run_ingest(agent, &event),
        Command::Sessions { json: true } => run_sessions_json(),
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

    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_default();
    let env = environment::gather_environment(&SystemEnv, cwd);
    let liveness = SystemProcessLookup::new();
    let hook_pid = std::process::id();
    state::handle_ingest(&store, &liveness, agent, event, &input, &env, hook_pid);
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

fn not_implemented() -> ! {
    eprintln!("not implemented yet");
    std::process::exit(2);
}
