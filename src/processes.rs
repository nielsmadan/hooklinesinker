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
    format_epoch_seconds(epoch_now())
}

pub fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// Inverse of `format_epoch_seconds` for the exact `YYYY-MM-DDTHH:MM:SSZ` shape it emits.
// Returns None for anything not in that shape rather than guessing.
pub fn parse_epoch_seconds(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    if d.next().is_some() {
        return None;
    }
    let mut t = time.split(':');
    let hour: u64 = t.next()?.parse().ok()?;
    let minute: u64 = t.next()?.parse().ok()?;
    let second: u64 = t.next()?.parse().ok()?;
    if d.next().is_some() || t.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86400 + hour * 3600 + minute * 60 + second)
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

// Howard Hinnant's days_from_civil; the exact inverse of civil_from_days.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month as i64;
    let d = day as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
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
    // startup (confirmed from the shipped @moonshot-ai/kimi-code bundle), but
    // that rename reaching sysinfo's name()/cmd() has not been verified
    // against a live process on either documented install path (npm shim,
    // curl-installed native binary — both named "kimi"), so accept both.
    fn expected_executables(agent: Agent) -> &'static [&'static str] {
        match agent {
            Agent::Claude => &["claude"],
            Agent::Codex => &["codex"],
            Agent::Opencode => &["opencode"],
            Agent::Pi => &["pi"],
            Agent::Droid => &["droid"],
            Agent::Qwen => &["qwen"],
            Agent::Kimi => &["kimi", "kimi-code"],
        }
    }
}

// Some agent CLIs are `#!/usr/bin/env node` shims (confirmed for Qwen's shipped
// @qwen-code/qwen-code bundle, which never renames its process on non-Windows
// platforms): the OS-visible process name is the interpreter's, not the agent's
// own command name. npm's bin-shimming still invokes
// the interpreter with the original command-named script path in argv, so fall
// back to matching that path's file name when the direct name check misses and
// the process is a known script runtime.
fn is_script_runtime(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "node" | "bun" | "deno")
}

// On Linux the reported process name is the main thread's (/proc/<pid>/comm), and
// node >= 24 names that thread "MainThread"; argv[0] still carries the interpreter's path.
fn runs_a_script_runtime(name: &str, cmd: &[std::ffi::OsString]) -> bool {
    if is_script_runtime(name) {
        return true;
    }
    cmd.first().is_some_and(|argv0| {
        Path::new(argv0)
            .file_name()
            .is_some_and(|f| is_script_runtime(&f.to_string_lossy()))
    })
}

fn is_owning_process(name: &str, cmd: &[std::ffi::OsString], expected: &[&str]) -> bool {
    if expected.iter().any(|e| name.eq_ignore_ascii_case(e)) {
        return true;
    }
    if !runs_a_script_runtime(name, cmd) {
        return false;
    }
    cmd.iter().any(|arg| {
        Path::new(arg)
            .file_name()
            .map(|f| f.to_string_lossy())
            .is_some_and(|f| expected.iter().any(|e| f.eq_ignore_ascii_case(e)))
    })
}

impl Default for SystemProcessLookup {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessLookup for SystemProcessLookup {
    fn owner_of(&self, hook_pid: u32, agent: Agent) -> Option<ProcessIdentity> {
        let expected = Self::expected_executables(agent);
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

    #[test]
    fn parse_epoch_seconds_inverts_format() {
        for secs in [
            0u64,
            1,
            86_399,
            86_400,
            1_000_000_000,
            1_798_761_296,
            4_102_444_800,
        ] {
            assert_eq!(parse_epoch_seconds(&format_epoch_seconds(secs)), Some(secs));
        }
    }

    #[test]
    fn parse_epoch_seconds_rejects_malformed() {
        for bad in [
            "",
            "not-a-date",
            "2026-12-31 23:54:56Z",
            "2026-13-01T00:00:00Z",
            "2026-12-31T23:54:56",
        ] {
            assert_eq!(parse_epoch_seconds(bad), None);
        }
    }

    fn os_string_argv(args: &[&str]) -> Vec<std::ffi::OsString> {
        args.iter().map(std::ffi::OsString::from).collect()
    }

    #[test]
    fn exact_process_name_matches_without_inspecting_argv() {
        assert!(is_owning_process("droid", &[], &["droid"]));
        assert!(is_owning_process("kimi-code", &[], &["kimi-code"]));
    }

    #[test]
    fn kimi_matches_both_the_documented_command_name_and_its_renamed_title() {
        // The curl-installed native binary and the npm shim are both
        // documented as "kimi"; process.title = "kimi-code" is only
        // confirmed for the shipped JS bundle's main() path, never verified
        // against a live process. Matching "kimi-code" alone would make
        // every Kimi session permanently unverifiable if that rename does
        // not reach sysinfo's name()/cmd() on some install path.
        let candidates = SystemProcessLookup::expected_executables(Agent::Kimi);
        assert!(is_owning_process("kimi", &[], candidates));
        assert!(is_owning_process("kimi-code", &[], candidates));
        assert!(!is_owning_process("kimi-cli", &[], candidates));
    }

    #[test]
    fn kimi_node_shim_matches_by_the_documented_kimi_argv_basename() {
        let candidates = SystemProcessLookup::expected_executables(Agent::Kimi);
        let argv = os_string_argv(&["node", "/usr/local/bin/kimi"]);
        assert!(is_owning_process("node", &argv, candidates));
    }

    #[test]
    fn node_shim_matches_by_the_invoked_scripts_file_name() {
        let argv = os_string_argv(&["node", "/usr/local/bin/qwen", "--flag"]);
        assert!(is_owning_process("node", &argv, &["qwen"]));
    }

    #[test]
    fn a_runtime_reporting_its_thread_name_matches_by_the_invoked_scripts_file_name() {
        let argv = os_string_argv(&["/usr/local/bin/node", "/usr/local/bin/qwen"]);
        assert!(is_owning_process("MainThread", &argv, &["qwen"]));
    }

    #[test]
    fn a_thread_named_process_that_is_not_a_runtime_never_matches() {
        // Fails if "MainThread" is ever added to is_script_runtime instead.
        let argv = os_string_argv(&["bash", "-c", "qwen"]);
        assert!(!is_owning_process("MainThread", &argv, &["qwen"]));
    }

    #[test]
    fn a_renamed_runtime_without_a_matching_argv_entry_does_not_match() {
        let argv = os_string_argv(&["node", "/usr/local/bin/some-other-cli"]);
        assert!(!is_owning_process("MainThread", &argv, &["qwen"]));
    }

    // Only argv[0] names the running interpreter: `sudo` forks rather than execs, so an
    // ancestor that merely passes a runtime path along must not be read as that runtime.
    #[test]
    fn a_launcher_that_merely_mentions_a_runtime_in_its_arguments_never_matches() {
        let argv = os_string_argv(&["sudo", "/usr/local/bin/node", "/usr/local/bin/qwen"]);
        assert!(!is_owning_process("sudo", &argv, &["qwen"]));
    }

    #[test]
    fn node_process_without_a_matching_argv_entry_does_not_match() {
        let argv = os_string_argv(&["node", "/usr/local/bin/some-other-cli"]);
        assert!(!is_owning_process("node", &argv, &["qwen"]));
    }

    #[test]
    fn a_non_runtime_process_never_matches_via_the_argv_fallback() {
        let argv = os_string_argv(&["bash", "-c", "qwen"]);
        assert!(!is_owning_process("bash", &argv, &["qwen"]));
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
