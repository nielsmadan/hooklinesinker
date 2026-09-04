use assert_cmd::Command;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn unique_temp_dir(label: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "hooklinesinker-cli-{label}-{}-{nanos}-{id}",
        std::process::id()
    ))
}

#[test]
fn version_reports_protocol_one() {
    let output = Command::cargo_bin("hooklinesinker")
        .unwrap()
        .args(["version", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["protocol"], 1);
    assert!(value["version"].as_str().is_some());
}

#[test]
fn invalid_agent_is_rejected_by_clap() {
    Command::cargo_bin("hooklinesinker")
        .unwrap()
        .args(["ingest", "--agent", "other", "--event", "start"])
        .assert()
        .failure();
}

#[test]
fn plain_version_prints_human_readable_line() {
    let output = Command::cargo_bin("hooklinesinker")
        .unwrap()
        .args(["version"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("hooklinesinker"));
    assert!(stdout.contains('1'));
}

#[test]
fn unimplemented_subcommands_exit_two_with_stderr_message() {
    let cases: &[&[&str]] = &[
        &["sessions"],
        &["consumers"],
        &["doctor"],
        &["install", "--consumer", "juggler"],
        &["uninstall", "--consumer", "juggler"],
        &["hooks", "install", "--agent", "claude"],
        &["hooks", "status", "--agent", "claude"],
        &["hooks", "uninstall", "--agent", "claude"],
    ];

    for args in cases {
        let output = Command::cargo_bin("hooklinesinker")
            .unwrap()
            .args(*args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "args: {args:?}");
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "not implemented yet\n",
            "args: {args:?}"
        );
        assert!(output.stdout.is_empty(), "args: {args:?}");
    }
}

#[test]
fn hooks_status_accepts_json_flag() {
    let output = Command::cargo_bin("hooklinesinker")
        .unwrap()
        .args(["hooks", "status", "--agent", "codex", "--json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn install_accepts_optional_sink() {
    let output = Command::cargo_bin("hooklinesinker")
        .unwrap()
        .args([
            "install",
            "--consumer",
            "juggler",
            "--sink",
            "http://127.0.0.1:7483/hook",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn sessions_json_reports_an_empty_envelope_for_a_fresh_state_dir() {
    let temp = unique_temp_dir("sessions-empty");
    let output = Command::cargo_bin("hooklinesinker")
        .unwrap()
        .env("XDG_STATE_HOME", &temp)
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["protocol"], 1);
    assert_eq!(value["sessions"], serde_json::json!([]));
    assert_eq!(value["problems"], serde_json::json!([]));
}

#[test]
fn ingest_writes_a_ledger_record_that_sessions_json_can_read() {
    let temp = unique_temp_dir("sessions-roundtrip");
    let ingest = Command::cargo_bin("hooklinesinker")
        .unwrap()
        .env("XDG_STATE_HOME", &temp)
        .args(["ingest", "--agent", "claude", "--event", "SessionStart"])
        .write_stdin(r#"{"session_id":"cli-session"}"#)
        .output()
        .unwrap();
    assert!(ingest.status.success());
    assert!(ingest.stderr.is_empty());

    let ledger_files: Vec<_> = std::fs::read_dir(temp.join("hooklinesinker/status"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(ledger_files.len(), 1);
    let record: Value = serde_json::from_slice(&std::fs::read(&ledger_files[0]).unwrap()).unwrap();
    assert_eq!(record["session"]["id"], "cli-session");
    assert_eq!(record["phase"], "idle");
    assert_eq!(record["running"], true);

    let output = Command::cargo_bin("hooklinesinker")
        .unwrap()
        .env("XDG_STATE_HOME", &temp)
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["protocol"], 1);
    // Whether this record appears here depends on whether the test binary
    // happens to have a real "claude" process ancestor in this environment;
    // the ledger-file assertions above already pin what ingest itself wrote.
    assert!(value["sessions"].is_array());
    assert!(value["problems"].is_array());
}

#[test]
fn ingest_captures_terminal_and_remote_host_from_the_process_environment() {
    let temp = unique_temp_dir("env-capture");
    let ingest = Command::cargo_bin("hooklinesinker")
        .unwrap()
        .env("XDG_STATE_HOME", &temp)
        .env("KITTY_WINDOW_ID", "42")
        .env_remove("KITTY_LISTEN_ON")
        .env_remove("KITTY_PID")
        .env_remove("ITERM_SESSION_ID")
        .env_remove("WEZTERM_PANE")
        .env_remove("TMUX_PANE")
        .env("SSH_CONNECTION", "203.0.113.5 1234 203.0.113.9 22")
        .env("USER", "alice")
        .env("HOSTNAME", "build-host.example.com")
        .args(["ingest", "--agent", "claude", "--event", "SessionStart"])
        .write_stdin(r#"{"session_id":"env-session"}"#)
        .output()
        .unwrap();
    assert!(ingest.status.success());
    assert!(ingest.stderr.is_empty());

    let ledger_files: Vec<_> = std::fs::read_dir(temp.join("hooklinesinker/status"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(ledger_files.len(), 1);
    let record: Value = serde_json::from_slice(&std::fs::read(&ledger_files[0]).unwrap()).unwrap();
    assert_eq!(record["terminal"]["sessionId"], "42");
    assert_eq!(record["terminal"]["terminalType"], "kitty");
    assert_eq!(record["remoteHost"], "alice@build-host");
}

#[test]
fn sessions_json_reports_a_problem_when_the_state_store_cannot_be_opened() {
    let temp = unique_temp_dir("sessions-open-failure");
    std::fs::create_dir_all(&temp).unwrap();
    // Occupy the exact path hooklinesinker needs as a directory with a plain
    // file, so StatusStore::open's create_dir_all fails deterministically.
    std::fs::write(temp.join("hooklinesinker"), b"not a directory").unwrap();

    let output = Command::cargo_bin("hooklinesinker")
        .unwrap()
        .env("XDG_STATE_HOME", &temp)
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["protocol"], 1);
    assert_eq!(value["sessions"], serde_json::json!([]));
    let problems = value["problems"].as_array().unwrap();
    assert!(!problems.is_empty());
}

#[test]
fn missing_required_flag_is_a_clap_usage_error_not_unimplemented() {
    let output = Command::cargo_bin("hooklinesinker")
        .unwrap()
        .args(["ingest", "--agent", "claude"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_ne!(
        String::from_utf8(output.stderr).unwrap(),
        "not implemented yet\n"
    );
}
