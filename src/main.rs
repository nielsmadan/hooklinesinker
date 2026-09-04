mod cli;

use clap::Parser;
use cli::{Cli, Command};
use hooklinesinker::normalize::HookEnvironment;
use hooklinesinker::paths;
use hooklinesinker::processes::{SystemProcessLookup, local_hostname};
use hooklinesinker::protocol::{
    Agent, GitIdentity, PROTOCOL_VERSION, TerminalIdentity, TmuxIdentity,
};
use hooklinesinker::state::{self, StatusStore};
use std::io;
use std::path::Path;
use std::process::Command as OsCommand;

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

    let env = gather_environment();
    let liveness = SystemProcessLookup::new();
    let hook_pid = std::process::id();
    state::handle_ingest(&store, &liveness, agent, event, &input, &env, hook_pid);
    std::process::exit(0);
}

fn run_sessions_json() {
    let store = StatusStore::open(paths::state_root());
    let (sessions, problems) = match &store {
        Ok(store) => {
            let liveness = SystemProcessLookup::new();
            (
                store.running(&liveness).unwrap_or_default(),
                store.health_problems().unwrap_or_default(),
            )
        }
        Err(_) => (Vec::new(), Vec::new()),
    };
    let envelope = serde_json::json!({
        "protocol": PROTOCOL_VERSION,
        "sessions": sessions,
        "problems": problems,
    });
    println!("{envelope}");
}

fn not_implemented() -> ! {
    eprintln!("not implemented yet");
    std::process::exit(2);
}

fn gather_environment() -> HookEnvironment {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_default();
    HookEnvironment {
        terminal: detect_terminal(),
        tmux: detect_tmux(),
        git: detect_git(&cwd),
        remote_host: detect_remote_host(),
        host: local_hostname(),
        cwd,
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn detect_terminal() -> Option<TerminalIdentity> {
    if let Some(session_id) = env_nonempty("KITTY_WINDOW_ID") {
        return Some(TerminalIdentity {
            session_id: Some(session_id),
            terminal_type: Some("kitty".into()),
            kitty_listen_on: env_nonempty("KITTY_LISTEN_ON"),
            kitty_pid: env_nonempty("KITTY_PID"),
        });
    }
    if let Some(session_id) = env_nonempty("ITERM_SESSION_ID") {
        return Some(TerminalIdentity {
            session_id: Some(session_id),
            terminal_type: Some("iterm2".into()),
            kitty_listen_on: None,
            kitty_pid: None,
        });
    }
    if let Some(session_id) = env_nonempty("WEZTERM_PANE") {
        return Some(TerminalIdentity {
            session_id: Some(session_id),
            terminal_type: Some("wezterm".into()),
            kitty_listen_on: None,
            kitty_pid: None,
        });
    }
    None
}

fn detect_tmux() -> Option<TmuxIdentity> {
    let pane = env_nonempty("TMUX_PANE")?;
    let session_name = OsCommand::new("tmux")
        .args(["display-message", "-p", "-t", &pane, "#{session_name}"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Some(TmuxIdentity {
        pane: Some(pane),
        session_name,
    })
}

fn run_git(cwd: &str, args: &[&str]) -> Option<String> {
    OsCommand::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn detect_git(cwd: &str) -> Option<GitIdentity> {
    let toplevel = run_git(cwd, &["rev-parse", "--show-toplevel"])?;
    let repo = Path::new(&toplevel)
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string);
    let branch = run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]);
    Some(GitIdentity { branch, repo })
}

fn detect_remote_host() -> Option<String> {
    env_nonempty("SSH_CONNECTION")?;
    let user = env_nonempty("USER").or_else(|| env_nonempty("LOGNAME"))?;
    let hostname = env_nonempty("HOSTNAME")
        .or_else(|| {
            OsCommand::new("hostname")
                .arg("-s")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(local_hostname);
    let short = hostname.split('.').next().unwrap_or(&hostname);
    Some(format!("{user}@{short}"))
}
