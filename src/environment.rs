use crate::normalize::HookEnvironment;
use crate::processes::local_hostname;
use crate::protocol::{GitIdentity, TerminalIdentity, TmuxIdentity};
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_millis(250);

pub(crate) trait EnvSource {
    fn var(&self, name: &str) -> Option<String>;
    fn command_output(&self, program: &str, args: &[&str]) -> Option<String>;
    fn local_hostname(&self) -> String;
}

pub(crate) struct SystemEnv;

impl EnvSource for SystemEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn command_output(&self, program: &str, args: &[&str]) -> Option<String> {
        let mut child = Command::new(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        };
        if !status.success() {
            return None;
        }
        child
            .wait_with_output()
            .ok()
            .and_then(|output| String::from_utf8(output.stdout).ok())
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

pub(crate) fn gather_environment(env: &dyn EnvSource, cwd: String) -> HookEnvironment {
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

#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "tmux expands its own format expressions"
)]
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
