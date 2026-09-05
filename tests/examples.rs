//! End-to-end coverage for the runnable examples in `examples/`, which the README's
//! `## Integrating` section points readers at.
//!
//! Proving a session is genuinely "live" needs a real OS process an agent name can be
//! matched against (`SystemProcessLookup` in src/processes.rs walks real process ancestry).
//! Copying a signed system shell to a fake name gets killed by macOS on exec, so these tests
//! instead lean on the same script-runtime argv fallback the adapters rely on: a node process
//! whose script file is literally named after the agent (e.g. `claude`) satisfies
//! `is_owning_process` without needing a renamed system binary. That means the live-session
//! tests need node, same as tests/ts_adapters.rs, and skip themselves when it is missing.

#![cfg(unix)]

use serde_json::Value;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_hooklinesinker"))
}

fn poll_script() -> PathBuf {
    repo_root().join("examples/cli-poll/poll.sh")
}

fn sink_server_script() -> PathBuf {
    repo_root().join("examples/sink-server/sink-server.mjs")
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "hooklinesinker-examples-{label}-{}-{nanos}-{id}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `Some(())`-shaped guard: prints a SKIP line and returns false when node is missing, same
/// convention as tests/ts_adapters.rs.
fn usable_node() -> bool {
    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => true,
        _ => {
            eprintln!(
                "SKIP: node is not on PATH; examples.rs live-session tests need node to fake an agent process"
            );
            false
        }
    }
}

struct IsolatedHome {
    root: PathBuf,
}

impl IsolatedHome {
    fn new(label: &str) -> Self {
        let root = unique_temp_dir(label);
        std::fs::create_dir_all(root.join("home")).unwrap();
        Self { root }
    }

    fn apply(&self, cmd: &mut Command) {
        cmd.env("XDG_STATE_HOME", &self.root)
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("HOME", self.root.join("home"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("OPENCODE_CONFIG_DIR")
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("QWEN_HOME")
            .env_remove("KIMI_CODE_HOME");
    }
}

fn run_bin(home: &IsolatedHome, args: &[&str]) -> (i32, String, String) {
    let mut cmd = Command::new(bin_path());
    home.apply(&mut cmd);
    let output = cmd.args(args).output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn run_bin_with_stdin(home: &IsolatedHome, args: &[&str], stdin: &str) -> (i32, String, String) {
    let mut cmd = Command::new(bin_path());
    home.apply(&mut cmd);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn wait_for(mut condition: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if condition() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    wait_for(|| path.exists(), timeout)
}

fn read_log(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn wait_for_log_contains(path: &Path, needle: &str, timeout: Duration) -> bool {
    wait_for(|| read_log(path).contains(needle), timeout)
}

/// A node script file literally named after the agent so `is_owning_process`'s script-runtime
/// fallback (src/processes.rs) matches it as the owning "claude" process. It ingests the given
/// fixture once, signals readiness, waits to be told to continue, ingests a second event under
/// the same still-alive process (so `ingest`'s ancestry walk yields the same pid/started_at and
/// therefore the same bindingId), then idles until killed.
fn fake_claude_agent_script(fixture: &Path, ready1: &Path, cont: &Path, ready2: &Path) -> String {
    format!(
        r#"const {{ execFileSync }} = require("node:child_process");
const fs = require("node:fs");

function sleepMs(ms) {{
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}}

function ingest(event) {{
  execFileSync({bin}, ["ingest", "--agent", "claude", "--event", event], {{
    input: fs.readFileSync({fixture}),
  }});
}}

ingest("SessionStart");
fs.writeFileSync({ready1}, "ready");
while (!fs.existsSync({cont})) sleepMs(30);
ingest("PreToolUse");
fs.writeFileSync({ready2}, "ready");
while (true) sleepMs(200);
"#,
        bin = serde_json::to_string(bin_path().to_str().unwrap()).unwrap(),
        fixture = serde_json::to_string(fixture.to_str().unwrap()).unwrap(),
        ready1 = serde_json::to_string(ready1.to_str().unwrap()).unwrap(),
        cont = serde_json::to_string(cont.to_str().unwrap()).unwrap(),
        ready2 = serde_json::to_string(ready2.to_str().unwrap()).unwrap(),
    )
}

fn write_stub_bin(dir: &Path, name: &str, sessions_json: &str) -> PathBuf {
    let path = dir.join(name);
    let script = format!(
        "#!/bin/sh\ncase \"$1\" in\n  install) exit 0 ;;\n  sessions) printf '%s' '{sessions_json}' ;;\n  *) exit 0 ;;\nesac\n"
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// A PATH containing only the plain-text tools poll.sh's no-jq fallback needs, so
/// `command -v jq` reliably fails regardless of what else is installed on the host.
fn narrow_path_without_jq(temp: &Path) -> PathBuf {
    let dir = temp.join("narrow-path");
    std::fs::create_dir_all(&dir).unwrap();
    for tool in ["sed", "awk", "grep", "cut", "head"] {
        if let Some(real) = find_in_path(tool) {
            std::os::unix::fs::symlink(&real, dir.join(tool)).unwrap();
        }
    }
    dir
}

#[test]
fn example_files_exist_and_are_executable() {
    let readme = repo_root().join("examples/README.md");
    for path in [&poll_script(), &sink_server_script(), &readme] {
        assert!(path.exists(), "missing {}", path.display());
    }
    for path in [&poll_script(), &sink_server_script()] {
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "{} is not executable", path.display());
    }
}

#[test]
fn poll_sh_refuses_an_envelope_with_an_unsupported_protocol() {
    let temp = unique_temp_dir("poll-protocol-refuse");
    let stub = write_stub_bin(
        &temp,
        "stub-hooklinesinker",
        r#"{"protocol":2,"sessions":[],"problems":[]}"#,
    );

    let output = Command::new(poll_script())
        .env("HOOKLINESINKER_BIN", &stub)
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing envelope with protocol 2"),
        "{stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "a refused envelope must not print a session table"
    );
}

#[test]
fn poll_sh_surfaces_reported_problems() {
    let temp = unique_temp_dir("poll-problems");
    let stub = write_stub_bin(
        &temp,
        "stub-hooklinesinker",
        r#"{"protocol":1,"sessions":[],"problems":[{"observedAt":"2026-09-04T00:00:00Z","message":"disk full"}]}"#,
    );

    let output = Command::new(poll_script())
        .env("HOOKLINESINKER_BIN", &stub)
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("disk full"), "{stdout}");
}

#[test]
fn poll_sh_falls_back_to_the_raw_envelope_without_jq() {
    let temp = unique_temp_dir("poll-no-jq");
    let narrow_path = narrow_path_without_jq(&temp);

    let stub = write_stub_bin(
        &temp,
        "stub-hooklinesinker",
        r#"{"protocol":1,"sessions":[{"protocol":1,"bindingId":"abc123","agent":"qwen","session":{"id":"fallback-session","cwd":"/tmp/fallback"}}],"problems":[]}"#,
    );
    let output = Command::new(poll_script())
        .env("HOOKLINESINKER_BIN", &stub)
        .env("PATH", &narrow_path)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("jq not found"), "{stderr}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fallback-session"), "{stdout}");

    // The protocol refusal must not depend on jq being present.
    let stub2 = write_stub_bin(
        &temp,
        "stub-hooklinesinker",
        r#"{"protocol":2,"sessions":[],"problems":[]}"#,
    );
    let output2 = Command::new(poll_script())
        .env("HOOKLINESINKER_BIN", &stub2)
        .env("PATH", &narrow_path)
        .output()
        .unwrap();
    assert!(!output2.status.success());
    assert!(
        String::from_utf8_lossy(&output2.stderr).contains("refusing"),
        "{}",
        String::from_utf8_lossy(&output2.stderr)
    );
}

#[test]
fn poll_and_sink_server_track_a_live_session_end_to_end() {
    if !usable_node() {
        return;
    }

    let home = IsolatedHome::new("live");
    let agents_dir = home.root.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();

    let fixture = repo_root().join("tests/fixtures/claude-start.json");
    let claude_script = agents_dir.join("claude");
    let ready1 = home.root.join("ready1");
    let cont = home.root.join("continue");
    let ready2 = home.root.join("ready2");
    std::fs::write(
        &claude_script,
        fake_claude_agent_script(&fixture, &ready1, &cont, &ready2),
    )
    .unwrap();

    let mut wrapper_cmd = Command::new("node");
    wrapper_cmd.arg(&claude_script);
    home.apply(&mut wrapper_cmd);
    wrapper_cmd.stdout(Stdio::null()).stderr(Stdio::null());
    let mut wrapper = wrapper_cmd
        .spawn()
        .expect("failed to spawn fake claude agent");

    assert!(
        wait_for_file(&ready1, Duration::from_secs(10)),
        "fake claude agent never completed its first ingest"
    );

    // Sanity check the fixture, independent of either example: the real binary must already
    // see one live claude session before we ask the examples to find it.
    let (code, stdout, stderr) = run_bin(&home, &["sessions", "--json"]);
    assert_eq!(code, 0, "{stderr}");
    let envelope: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(envelope["protocol"], 1);
    let sessions = envelope["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "{stdout}");
    assert_eq!(sessions[0]["session"]["id"], "claude-session");

    // 1. Pull model: poll.sh must show that session.
    let mut poll_cmd = Command::new(poll_script());
    home.apply(&mut poll_cmd);
    poll_cmd.env("HOOKLINESINKER_BIN", bin_path());
    let poll_output = poll_cmd.output().unwrap();
    assert!(
        poll_output.status.success(),
        "{}",
        String::from_utf8_lossy(&poll_output.stderr)
    );
    let poll_stdout = String::from_utf8_lossy(&poll_output.stdout);
    assert!(poll_stdout.contains("claude"), "{poll_stdout}");
    assert!(poll_stdout.contains("idle"), "{poll_stdout}");
    assert!(poll_stdout.contains("claude-session"), "{poll_stdout}");
    assert!(
        poll_stdout.contains("/Users/example/project"),
        "{poll_stdout}"
    );

    // 2. Push model: sink-server.mjs must hydrate the same session on startup.
    let sink_log = home.root.join("sink.log");
    let mut sink_cmd = Command::new("node");
    sink_cmd.arg(sink_server_script());
    home.apply(&mut sink_cmd);
    sink_cmd
        .env("HOOKLINESINKER_BIN", bin_path())
        .env("HOOKLINESINKER_CONSUMER", "examples-test-sink")
        .env("PORT", "0")
        .stdout(std::fs::File::create(&sink_log).unwrap())
        .stderr(std::fs::File::create(home.root.join("sink.err")).unwrap());
    let mut sink = sink_cmd.spawn().expect("failed to spawn sink-server.mjs");

    assert!(
        wait_for_log_contains(&sink_log, "hydrate complete", Duration::from_secs(10)),
        "sink server never finished hydration:\n{}",
        read_log(&sink_log)
    );
    let log_after_hydrate = read_log(&sink_log);
    assert!(
        log_after_hydrate.contains("hydrate add"),
        "{log_after_hydrate}"
    );
    assert!(
        log_after_hydrate.contains("session=claude-session"),
        "{log_after_hydrate}"
    );
    assert!(
        log_after_hydrate.contains("hydrate complete: 1 session"),
        "{log_after_hydrate}"
    );

    // 3. A live event for the same bindingId (same still-alive fake agent process) must update
    // the hydrated row, never add a second one — the dedupe the README calls for.
    std::fs::write(&cont, "go").unwrap();
    assert!(
        wait_for_file(&ready2, Duration::from_secs(10)),
        "fake claude agent never completed its second ingest"
    );
    assert!(
        wait_for_log_contains(&sink_log, "post update", Duration::from_secs(10)),
        "sink server never logged a dedupe update:\n{}",
        read_log(&sink_log)
    );
    let log_after_update = read_log(&sink_log);
    assert!(
        !log_after_update.contains("post add "),
        "a live event for an already-hydrated session must update it, not add a second row:\n{log_after_update}"
    );
    assert!(
        log_after_update.contains("phase=working"),
        "{log_after_update}"
    );

    // 4. Killing the agent process, then any unrelated ingest, must sweep the dead binding and
    // fan out a running:false event that removes the sink's row.
    wrapper.kill().expect("failed to kill fake claude agent");
    wrapper.wait().expect("failed to reap fake claude agent");

    let codex_fixture = repo_root().join("tests/fixtures/codex-stop.json");
    let codex_stdin = std::fs::read_to_string(&codex_fixture).unwrap();
    let (code, _stdout, stderr) = run_bin_with_stdin(
        &home,
        &["ingest", "--agent", "codex", "--event", "Stop"],
        &codex_stdin,
    );
    assert_eq!(code, 0, "unrelated ingest failed: {stderr}");

    assert!(
        wait_for_log_contains(&sink_log, "reason=running:false", Duration::from_secs(10)),
        "sink server never removed the dead binding:\n{}",
        read_log(&sink_log)
    );
    let final_log = read_log(&sink_log);
    assert!(final_log.contains("post remove"), "{final_log}");

    sink.kill().ok();
    let _ = sink.wait();
}
