use crate::normalize::HookEnvironment;
use crate::processes::local_hostname;
use crate::protocol::{GitIdentity, StatusEvent, TerminalIdentity, TmuxIdentity};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT: Duration = Duration::from_millis(250);

// Apps launched from Finder get launchd's PATH, which leaves out Homebrew.
const FALLBACK_BIN_DIRS: [&str; 3] = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];

#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "tmux expands its own format expressions"
)]
pub(crate) const TMUX_PANE_FORMAT: &str =
    "#{session_attached},#{session_group_attached},#{session_name}";

#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "tmux expands its own format expressions"
)]
pub(crate) const TMUX_PANES_FORMAT: &str =
    "#{pane_id},#{session_attached},#{session_group_attached},#{session_name}";

type PaneReports = HashMap<String, (TmuxAttachment, Option<String>)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TmuxAttachment {
    Attached,
    Detached,
    Unknown,
}

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
        let mut child = spawn_with_fallback(program, args).ok()?;
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

fn spawn_with_fallback(program: &str, args: &[&str]) -> io::Result<Child> {
    let spawn = |path: &str| {
        Command::new(path)
            .args(args)
            // Without -S, tmux follows the caller's $TMUX instead of the default server.
            .env_remove("TMUX")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    };
    match spawn(program) {
        Err(e) if e.kind() == io::ErrorKind::NotFound && !program.contains('/') => {
            FALLBACK_BIN_DIRS
                .iter()
                .find_map(|dir| spawn(&format!("{dir}/{program}")).ok())
                .ok_or(e)
        }
        result => result,
    }
}

pub(crate) fn gather_environment(env: &dyn EnvSource, cwd: String) -> HookEnvironment {
    let (tmux, attachment) = detect_tmux(env);
    HookEnvironment {
        terminal: displayed_terminal(detect_terminal(env), attachment),
        tmux,
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

// A detached tmux server still carries the terminal env of the tab that started it.
fn displayed_terminal(
    terminal: Option<TerminalIdentity>,
    attachment: TmuxAttachment,
) -> Option<TerminalIdentity> {
    match attachment {
        TmuxAttachment::Detached => None,
        TmuxAttachment::Attached | TmuxAttachment::Unknown => terminal,
    }
}

fn detect_tmux(env: &dyn EnvSource) -> (Option<TmuxIdentity>, TmuxAttachment) {
    let Some(pane) = nonempty(env.var("TMUX_PANE")) else {
        return (None, TmuxAttachment::Unknown);
    };
    let tmux_var = env.var("TMUX");
    let socket = tmux_var.as_deref().and_then(tmux_socket);
    let (attachment, session_name) = query_pane(env, socket, &pane);
    // Pane ids are per server, so without $TMUX the default server may not own this pane.
    let attachment = if socket.is_some() {
        attachment
    } else {
        TmuxAttachment::Unknown
    };
    (
        Some(TmuxIdentity {
            pane: Some(pane),
            session_name,
            socket: socket.map(str::to_string),
        }),
        attachment,
    )
}

fn query_pane(
    env: &dyn EnvSource,
    socket: Option<&str>,
    pane: &str,
) -> (TmuxAttachment, Option<String>) {
    let mut args = Vec::new();
    if let Some(socket) = socket {
        args.extend(["-S", socket]);
    }
    args.extend(["display-message", "-p", "-t", pane, TMUX_PANE_FORMAT]);
    env.command_output("tmux", &args)
        .map_or((TmuxAttachment::Unknown, None), |output| {
            parse_pane_report(&output)
        })
}

// A record keeps the terminal it saw at its last hook, so a session detached since then
// would still claim the tab it left.
pub(crate) fn withhold_detached_terminals(
    env: &dyn EnvSource,
    sessions: Vec<StatusEvent>,
) -> Vec<StatusEvent> {
    let sockets: BTreeSet<Option<&str>> = sessions
        .iter()
        .filter_map(rechecked_tmux)
        .map(|tmux| tmux.socket.as_deref())
        .collect();
    let reports: BTreeMap<Option<String>, PaneReports> = sockets
        .into_iter()
        .map(|socket| (socket.map(str::to_string), list_panes(env, socket)))
        .collect();
    sessions
        .into_iter()
        .map(|status| recheck_terminal(status, &reports))
        .collect()
}

fn rechecked_tmux(status: &StatusEvent) -> Option<&TmuxIdentity> {
    status.terminal.as_ref()?;
    status.tmux.as_ref().filter(|tmux| tmux.pane.is_some())
}

fn list_panes(env: &dyn EnvSource, socket: Option<&str>) -> PaneReports {
    let mut args = Vec::new();
    if let Some(socket) = socket {
        args.extend(["-S", socket]);
    }
    args.extend(["list-panes", "-a", "-F", TMUX_PANES_FORMAT]);
    env.command_output("tmux", &args)
        .map(|output| {
            output
                .lines()
                .filter_map(|line| {
                    let (pane, report) = line.split_once(',')?;
                    Some((pane.to_string(), parse_pane_report(report)))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn recheck_terminal(
    status: StatusEvent,
    reports: &BTreeMap<Option<String>, PaneReports>,
) -> StatusEvent {
    let Some(tmux) = rechecked_tmux(&status) else {
        return status;
    };
    let Some((attachment, session_name)) = tmux
        .pane
        .as_deref()
        .and_then(|pane| reports.get(&tmux.socket)?.get(pane))
    else {
        return status;
    };
    // Without a socket, the default server's answer counts only if it names the same session.
    let identified =
        tmux.socket.is_some() || (session_name.is_some() && *session_name == tmux.session_name);
    if !identified {
        return status;
    }
    let attachment = *attachment;
    StatusEvent {
        terminal: displayed_terminal(status.terminal, attachment),
        ..status
    }
}

fn tmux_socket(tmux_var: &str) -> Option<&str> {
    tmux_var
        .split(',')
        .next()
        .filter(|socket| !socket.is_empty())
}

fn parse_pane_report(output: &str) -> (TmuxAttachment, Option<String>) {
    let mut fields = output.splitn(3, ',');
    let attached = fields.next().and_then(|field| field.parse::<u32>().ok());
    let group_attached = fields.next().and_then(|field| {
        if field.is_empty() {
            Some(0)
        } else {
            field.parse::<u32>().ok()
        }
    });
    let session_name = fields
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    let attachment = match (attached, group_attached) {
        (Some(attached), Some(group)) if attached > 0 || group > 0 => TmuxAttachment::Attached,
        (Some(_), Some(_)) => TmuxAttachment::Detached,
        _ => TmuxAttachment::Unknown,
    };
    (attachment, session_name)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn iterm() -> TerminalIdentity {
        TerminalIdentity {
            session_id: Some("w2t9p0:10935D3A".into()),
            terminal_type: Some("iterm2".into()),
            kitty_listen_on: None,
            kitty_pid: None,
        }
    }

    #[test]
    fn pane_report_with_no_clients_is_detached() {
        assert_eq!(
            parse_pane_report("0,,juggler-case"),
            (TmuxAttachment::Detached, Some("juggler-case".into()))
        );
        assert_eq!(parse_pane_report("0,0,w").0, TmuxAttachment::Detached);
    }

    #[test]
    fn pane_report_with_a_client_is_attached() {
        assert_eq!(
            parse_pane_report("1,,work"),
            (TmuxAttachment::Attached, Some("work".into()))
        );
    }

    #[test]
    fn grouped_session_viewed_through_a_sibling_is_attached() {
        assert_eq!(parse_pane_report("0,1,work").0, TmuxAttachment::Attached);
    }

    #[test]
    fn unparseable_counts_are_unknown_but_keep_the_session_name() {
        assert_eq!(
            parse_pane_report("x,,work"),
            (TmuxAttachment::Unknown, Some("work".into()))
        );
        assert_eq!(
            parse_pane_report("garbage"),
            (TmuxAttachment::Unknown, None)
        );
    }

    #[test]
    fn session_names_may_contain_commas() {
        assert_eq!(parse_pane_report("0,,a,b").1.as_deref(), Some("a,b"));
    }

    #[test]
    fn tmux_socket_is_the_first_field_of_the_tmux_variable() {
        assert_eq!(
            tmux_socket("/private/tmp/tmux-501/default,123,4"),
            Some("/private/tmp/tmux-501/default")
        );
        assert_eq!(tmux_socket(""), None);
        assert_eq!(tmux_socket(",1,2"), None);
    }

    #[test]
    fn only_a_detached_session_withholds_the_terminal() {
        assert_eq!(
            displayed_terminal(Some(iterm()), TmuxAttachment::Detached),
            None
        );
        assert_eq!(
            displayed_terminal(Some(iterm()), TmuxAttachment::Attached),
            Some(iterm())
        );
        assert_eq!(
            displayed_terminal(Some(iterm()), TmuxAttachment::Unknown),
            Some(iterm())
        );
    }
}
