use crate::normalize::HookEnvironment;
use crate::processes::local_hostname;
use crate::protocol::{GitIdentity, TerminalIdentity, TmuxIdentity};
use std::path::Path;
use std::process::Command;

pub trait EnvSource {
    fn var(&self, name: &str) -> Option<String>;
    fn command_output(&self, program: &str, args: &[&str]) -> Option<String>;
    fn local_hostname(&self) -> String;
}

pub struct SystemEnv;

impl EnvSource for SystemEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn command_output(&self, program: &str, args: &[&str]) -> Option<String> {
        Command::new(program)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    fn local_hostname(&self) -> String {
        local_hostname()
    }
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

pub fn gather_environment(env: &dyn EnvSource, cwd: String) -> HookEnvironment {
    HookEnvironment {
        terminal: detect_terminal(env),
        tmux: detect_tmux(env),
        git: detect_git(env, &cwd),
        remote_host: detect_remote_host(env),
        host: env.local_hostname(),
        cwd,
    }
}

fn detect_terminal(env: &dyn EnvSource) -> Option<TerminalIdentity> {
    if let Some(session_id) = nonempty(env.var("KITTY_WINDOW_ID")) {
        return Some(TerminalIdentity {
            session_id: Some(session_id),
            terminal_type: Some("kitty".into()),
            kitty_listen_on: nonempty(env.var("KITTY_LISTEN_ON")),
            kitty_pid: nonempty(env.var("KITTY_PID")),
        });
    }
    if let Some(session_id) = nonempty(env.var("ITERM_SESSION_ID")) {
        return Some(TerminalIdentity {
            session_id: Some(session_id),
            terminal_type: Some("iterm2".into()),
            kitty_listen_on: None,
            kitty_pid: None,
        });
    }
    if let Some(session_id) = nonempty(env.var("WEZTERM_PANE")) {
        return Some(TerminalIdentity {
            session_id: Some(session_id),
            terminal_type: Some("wezterm".into()),
            kitty_listen_on: None,
            kitty_pid: None,
        });
    }
    None
}

fn detect_tmux(env: &dyn EnvSource) -> Option<TmuxIdentity> {
    let pane = nonempty(env.var("TMUX_PANE"))?;
    let session_name = env.command_output(
        "tmux",
        &["display-message", "-p", "-t", &pane, "#{session_name}"],
    );
    Some(TmuxIdentity {
        pane: Some(pane),
        session_name,
    })
}

fn detect_git(env: &dyn EnvSource, cwd: &str) -> Option<GitIdentity> {
    let toplevel = env.command_output("git", &["-C", cwd, "rev-parse", "--show-toplevel"])?;
    let repo = Path::new(&toplevel)
        .file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string);
    let branch = env.command_output("git", &["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"]);
    Some(GitIdentity { branch, repo })
}

fn detect_remote_host(env: &dyn EnvSource) -> Option<String> {
    nonempty(env.var("SSH_CONNECTION"))?;
    let user = nonempty(env.var("USER")).or_else(|| nonempty(env.var("LOGNAME")))?;
    let hostname = nonempty(env.var("HOSTNAME"))
        .or_else(|| env.command_output("hostname", &["-s"]))
        .unwrap_or_else(|| env.local_hostname());
    let short = hostname.split('.').next().unwrap_or(&hostname);
    Some(format!("{user}@{short}"))
}
