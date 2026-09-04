use crate::protocol::{Agent, ProcessIdentity};
use std::path::Path;
use sysinfo::{Pid, ProcessesToUpdate, System};

pub trait ProcessLookup {
    fn owner_of(&self, hook_pid: u32, agent: Agent) -> Option<ProcessIdentity>;
    fn is_alive(&self, identity: &ProcessIdentity) -> bool;
}

pub fn local_hostname() -> String {
    System::host_name().unwrap_or_else(|| "localhost".to_string())
}

pub fn now_rfc3339() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_epoch_seconds(seconds)
}

pub fn format_epoch_seconds(seconds: u64) -> String {
    let days = (seconds / 86400) as i64;
    let secs_of_day = seconds % 86400;
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

// Howard Hinnant's civil_from_days algorithm; keeps observed_at/started_at
// formatting free of a chrono dependency.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

pub struct SystemProcessLookup {
    system: System,
}

impl SystemProcessLookup {
    pub fn new() -> Self {
        let mut system = System::new_all();
        system.refresh_processes(ProcessesToUpdate::All, true);
        Self { system }
    }

    // Kimi's own CLI renames its process via `process.title = "kimi-code"` at
    // startup (confirmed from the shipped @moonshot-ai/kimi-code bundle), so the
    // OS-visible name is "kimi-code" rather than the "kimi" command users type.
    fn expected_executable(agent: Agent) -> &'static str {
        match agent {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
            Agent::Opencode => "opencode",
            Agent::Pi => "pi",
            Agent::Droid => "droid",
            Agent::Qwen => "qwen",
            Agent::Kimi => "kimi-code",
        }
    }
}

// Some agent CLIs are `#!/usr/bin/env node` shims (confirmed for Qwen's shipped
// @qwen-code/qwen-code bundle, which never renames its process on non-Windows
// platforms): the OS-visible process name is the interpreter's ("node", "bun",
// "deno"), not the agent's own command name. npm's bin-shimming still invokes
// the interpreter with the original command-named script path in argv, so fall
// back to matching that path's file name when the direct name check misses and
// the process is a known script runtime.
fn is_script_runtime(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "node" | "bun" | "deno")
}

fn is_owning_process(name: &str, cmd: &[std::ffi::OsString], expected: &str) -> bool {
    if name.eq_ignore_ascii_case(expected) {
        return true;
    }
    if !is_script_runtime(name) {
        return false;
    }
    cmd.iter().any(|arg| {
        Path::new(arg)
            .file_name()
            .map(|f| f.to_string_lossy())
            .is_some_and(|f| f.eq_ignore_ascii_case(expected))
    })
}

impl Default for SystemProcessLookup {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessLookup for SystemProcessLookup {
    fn owner_of(&self, hook_pid: u32, agent: Agent) -> Option<ProcessIdentity> {
        let expected = Self::expected_executable(agent);
        let mut current = Some(Pid::from(hook_pid as usize));
        let mut depth = 0;
        while let Some(pid) = current {
            if depth > 64 {
                return None;
            }
            depth += 1;
            let process = self.system.process(pid)?;
            if is_owning_process(&process.name().to_string_lossy(), process.cmd(), expected) {
                return Some(ProcessIdentity {
                    pid: usize::from(pid) as u32,
                    started_at: format_epoch_seconds(process.start_time()),
                    host: local_hostname(),
                });
            }
            current = process.parent();
        }
        None
    }

    fn is_alive(&self, identity: &ProcessIdentity) -> bool {
        match self.system.process(Pid::from(identity.pid as usize)) {
            Some(process) => format_epoch_seconds(process.start_time()) == identity.started_at,
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_zero_is_the_unix_epoch() {
        assert_eq!(format_epoch_seconds(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn a_known_timestamp_formats_correctly() {
        assert_eq!(format_epoch_seconds(1_798_761_296), "2026-12-31T23:54:56Z");
    }

    fn os_string_argv(args: &[&str]) -> Vec<std::ffi::OsString> {
        args.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn exact_process_name_matches_without_inspecting_argv() {
        assert!(is_owning_process("droid", &[], "droid"));
        assert!(is_owning_process("kimi-code", &[], "kimi-code"));
    }

    #[test]
    fn node_shim_matches_by_the_invoked_scripts_file_name() {
        let argv = os_string_argv(&["node", "/usr/local/bin/qwen", "--flag"]);
        assert!(is_owning_process("node", &argv, "qwen"));
    }

    #[test]
    fn node_process_without_a_matching_argv_entry_does_not_match() {
        let argv = os_string_argv(&["node", "/usr/local/bin/some-other-cli"]);
        assert!(!is_owning_process("node", &argv, "qwen"));
    }

    #[test]
    fn a_non_runtime_process_never_matches_via_the_argv_fallback() {
        let argv = os_string_argv(&["bash", "-c", "qwen"]);
        assert!(!is_owning_process("bash", &argv, "qwen"));
    }

    #[test]
    fn an_implausible_pid_is_never_alive() {
        let lookup = SystemProcessLookup::new();
        let identity = ProcessIdentity {
            pid: u32::MAX,
            started_at: "1970-01-01T00:00:00Z".into(),
            host: "test".into(),
        };
        assert!(!lookup.is_alive(&identity));
    }
}
