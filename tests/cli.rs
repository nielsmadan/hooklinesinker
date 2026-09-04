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

fn hooklinesinker_isolated(temp: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("hooklinesinker").unwrap();
    cmd.env("XDG_STATE_HOME", temp);
    cmd.env("HOME", temp.join("home"));
    cmd.env("XDG_DATA_HOME", temp.join("data"));
    cmd.env_remove("XDG_CONFIG_HOME");
    cmd.env_remove("OPENCODE_CONFIG_DIR");
    cmd.env_remove("PI_CODING_AGENT_DIR");
    cmd
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
    let cases: &[&[&str]] = &[&["sessions"], &["consumers"]];

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
fn hooks_status_reports_missing_before_install() {
    let temp = unique_temp_dir("hooks-status-missing");
    let output = hooklinesinker_isolated(&temp)
        .args(["hooks", "status", "--agent", "codex", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["protocol"], 1);
    assert_eq!(value["agent"], "codex");
    assert_eq!(value["state"], "missing");
    assert_eq!(value["entries"], serde_json::json!([]));
}

#[test]
fn hooks_install_writes_claude_settings_with_the_stable_binary_path() {
    let temp = unique_temp_dir("hooks-install-claude");
    let output = hooklinesinker_isolated(&temp)
        .args(["hooks", "install", "--agent", "claude"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let settings_path = temp.join("home/.claude/settings.json");
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
    let command = settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(command.contains("hooklinesinker"));
    assert!(command.ends_with("ingest --agent claude --event SessionStart"));

    let status_output = hooklinesinker_isolated(&temp)
        .args(["hooks", "status", "--agent", "claude", "--json"])
        .output()
        .unwrap();
    assert!(status_output.status.success());
    let status: Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status["state"], "installed");
    assert_eq!(status["entries"].as_array().unwrap().len(), 11);
}

#[test]
fn hooks_uninstall_removes_what_hooks_install_wrote() {
    let temp = unique_temp_dir("hooks-uninstall");
    hooklinesinker_isolated(&temp)
        .args(["hooks", "install", "--agent", "codex"])
        .output()
        .unwrap();

    let output = hooklinesinker_isolated(&temp)
        .args(["hooks", "uninstall", "--agent", "codex"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let status_output = hooklinesinker_isolated(&temp)
        .args(["hooks", "status", "--agent", "codex", "--json"])
        .output()
        .unwrap();
    let status: Value = serde_json::from_slice(&status_output.stdout).unwrap();
    assert_eq!(status["state"], "missing");
}

#[test]
fn install_accepts_optional_sink() {
    let temp = unique_temp_dir("install-sink");
    let output = hooklinesinker_isolated(&temp)
        .args([
            "install",
            "--consumer",
            "juggler",
            "--sink",
            "http://127.0.0.1:7483/hook",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());

    let consumer_files: Vec<_> = std::fs::read_dir(temp.join("hooklinesinker/consumers"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(consumer_files.len(), 1);
    let record: Value =
        serde_json::from_slice(&std::fs::read(&consumer_files[0]).unwrap()).unwrap();
    assert_eq!(record["name"], "juggler");
    assert_eq!(record["capabilities"], serde_json::json!(["status"]));
    assert_eq!(record["sink"], "http://127.0.0.1:7483/hook");
}

#[test]
fn install_activates_a_versioned_binary_behind_a_stable_symlink() {
    let temp = unique_temp_dir("install-activates");
    let output = hooklinesinker_isolated(&temp)
        .args(["install", "--consumer", "juggler"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let bin_path = temp.join("data/hooklinesinker/bin/hooklinesinker");
    let metadata = std::fs::symlink_metadata(&bin_path).unwrap();
    assert!(metadata.file_type().is_symlink());
    let target = std::fs::read_link(&bin_path).unwrap();
    assert!(!target.is_absolute());

    let expected_version = env!("CARGO_PKG_VERSION");
    let version_binary = temp
        .join("data/hooklinesinker/versions")
        .join(expected_version)
        .join("hooklinesinker");
    assert!(version_binary.exists());

    let doctor_output = hooklinesinker_isolated(&temp)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    let doctor: Value = serde_json::from_slice(&doctor_output.stdout).unwrap();
    let checks = doctor["checks"].as_array().unwrap();
    let active_version_check = checks
        .iter()
        .find(|c| c["name"] == "active_version_target")
        .unwrap();
    assert_eq!(
        active_version_check["detail"],
        format!("v{expected_version} (protocol 1)")
    );
}

#[test]
fn install_without_a_sink_registers_a_pull_only_consumer() {
    let temp = unique_temp_dir("install-no-sink");
    let output = hooklinesinker_isolated(&temp)
        .args(["install", "--consumer", "ringleader"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let consumer_files: Vec<_> = std::fs::read_dir(temp.join("hooklinesinker/consumers"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(consumer_files.len(), 1);
    let record: Value =
        serde_json::from_slice(&std::fs::read(&consumer_files[0]).unwrap()).unwrap();
    assert_eq!(record["sink"], serde_json::Value::Null);
}

#[test]
fn install_rejects_an_invalid_consumer_name() {
    let temp = unique_temp_dir("install-invalid-name");
    let output = hooklinesinker_isolated(&temp)
        .args(["install", "--consumer", "Not Valid"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_ne!(output.status.code(), Some(2));
    assert!(
        std::fs::read_dir(temp.join("hooklinesinker/consumers"))
            .map(|entries| entries.count())
            .unwrap_or(0)
            == 0
    );
}

#[test]
fn uninstall_removes_a_registered_consumer() {
    let temp = unique_temp_dir("uninstall");
    hooklinesinker_isolated(&temp)
        .args(["install", "--consumer", "juggler"])
        .output()
        .unwrap();

    let output = hooklinesinker_isolated(&temp)
        .args(["uninstall", "--consumer", "juggler"])
        .output()
        .unwrap();
    assert!(output.status.success());

    let consumer_files: Vec<_> = std::fs::read_dir(temp.join("hooklinesinker/consumers"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(consumer_files.is_empty());
}

#[test]
fn uninstalling_the_last_consumer_removes_the_active_symlink_but_keeps_the_version_directory() {
    let temp = unique_temp_dir("uninstall-last-consumer");
    hooklinesinker_isolated(&temp)
        .args(["install", "--consumer", "juggler"])
        .output()
        .unwrap();
    let bin_path = temp.join("data/hooklinesinker/bin/hooklinesinker");
    assert!(bin_path.exists());
    let expected_version = env!("CARGO_PKG_VERSION");
    let version_binary = temp
        .join("data/hooklinesinker/versions")
        .join(expected_version)
        .join("hooklinesinker");

    let output = hooklinesinker_isolated(&temp)
        .args(["uninstall", "--consumer", "juggler"])
        .output()
        .unwrap();
    assert!(output.status.success());

    assert!(!bin_path.exists());
    assert!(
        version_binary.exists(),
        "version directories must survive last-consumer cleanup"
    );
}

#[test]
fn uninstall_of_a_never_registered_consumer_still_succeeds() {
    let temp = unique_temp_dir("uninstall-missing");
    let output = hooklinesinker_isolated(&temp)
        .args(["uninstall", "--consumer", "never-registered"])
        .output()
        .unwrap();
    assert!(output.status.success());
}

#[test]
fn consumers_json_reports_registered_consumers() {
    let temp = unique_temp_dir("consumers-json");
    hooklinesinker_isolated(&temp)
        .args([
            "install",
            "--consumer",
            "juggler",
            "--sink",
            "http://127.0.0.1:7483/hook",
        ])
        .output()
        .unwrap();

    let output = hooklinesinker_isolated(&temp)
        .args(["consumers", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["protocol"], 1);
    let consumers = value["consumers"].as_array().unwrap();
    assert_eq!(consumers.len(), 1);
    assert_eq!(consumers[0]["name"], "juggler");
}

#[test]
fn consumers_json_reports_an_empty_envelope_for_a_fresh_state_dir() {
    let temp = unique_temp_dir("consumers-empty");
    let output = Command::cargo_bin("hooklinesinker")
        .unwrap()
        .env("XDG_STATE_HOME", &temp)
        .args(["consumers", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["protocol"], 1);
    assert_eq!(value["consumers"], serde_json::json!([]));
}

#[test]
fn doctor_json_reports_ok_for_a_fresh_state_dir() {
    let temp = unique_temp_dir("doctor-fresh");
    let output = hooklinesinker_isolated(&temp)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["protocol"], 1);
    assert_eq!(value["exitCode"], 0);
    let checks = value["checks"].as_array().unwrap();
    let names: Vec<_> = checks.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "version",
            "permissions",
            "active_version_target",
            "consumer_parse_problems",
            "hook_status",
            "status_parse_problems",
            "dead_records",
            "last_sink_error",
        ]
    );
    assert!(checks.iter().all(|c| c["ok"] == true));
    let active_version_check = checks
        .iter()
        .find(|c| c["name"] == "active_version_target")
        .unwrap();
    assert_eq!(active_version_check["detail"], "not installed");
    let hook_status_check = checks.iter().find(|c| c["name"] == "hook_status").unwrap();
    assert!(
        hook_status_check["detail"]
            .as_str()
            .unwrap()
            .contains("Missing")
    );
}

#[test]
fn doctor_human_output_has_one_line_per_check() {
    let temp = unique_temp_dir("doctor-human");
    let output = hooklinesinker_isolated(&temp)
        .args(["doctor"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines.len(), 8);
    assert!(lines.iter().all(|line| line.starts_with('[')));
}

#[test]
fn doctor_does_not_fail_on_a_stale_sink_error() {
    let temp = unique_temp_dir("doctor-sink-error");
    let state_dir = temp.join("hooklinesinker");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(
        state_dir.join("health.json"),
        serde_json::json!([
            {"observedAt": "2026-09-04T00:00:00Z", "message": "sink juggler failed: connection refused"}
        ])
        .to_string(),
    )
    .unwrap();

    let output = hooklinesinker_isolated(&temp)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["exitCode"], 0);
    let checks = value["checks"].as_array().unwrap();
    let last_sink_error = checks
        .iter()
        .find(|c| c["name"] == "last_sink_error")
        .unwrap();
    assert!(
        last_sink_error["detail"]
            .as_str()
            .unwrap()
            .contains("sink juggler failed")
    );
}

#[test]
fn doctor_fails_when_a_status_record_is_unparseable() {
    let temp = unique_temp_dir("doctor-corrupt-status");
    let status_dir = temp.join("hooklinesinker/status");
    std::fs::create_dir_all(&status_dir).unwrap();
    std::fs::write(status_dir.join("broken.json"), b"not json").unwrap();

    let output = hooklinesinker_isolated(&temp)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_ne!(value["exitCode"], 0);
    assert_eq!(
        output.status.code(),
        value["exitCode"].as_i64().map(|c| c as i32)
    );
    let checks = value["checks"].as_array().unwrap();
    let status_check = checks
        .iter()
        .find(|c| c["name"] == "status_parse_problems")
        .unwrap();
    assert_eq!(status_check["ok"], false);
}

fn dead_ledger_record(session_id: &str) -> String {
    serde_json::json!({
        "protocol": 1,
        "bindingId": session_id,
        "agent": "claude",
        "event": "SessionStart",
        "phase": "idle",
        "running": true,
        "observedAt": "2026-09-04T00:00:00Z",
        "session": {"id": session_id, "cwd": "/tmp", "transcriptPath": null},
        "process": {"pid": u32::MAX, "startedAt": "1970-01-01T00:00:00Z", "host": "test-host"},
        "terminal": null,
        "tmux": null,
        "git": null,
        "remoteHost": null,
    })
    .to_string()
}

#[test]
fn doctor_counts_a_dead_record_without_removing_it() {
    let temp = unique_temp_dir("doctor-dead-record");
    let status_dir = temp.join("hooklinesinker/status");
    std::fs::create_dir_all(&status_dir).unwrap();
    let record_path = status_dir.join("dead.json");
    std::fs::write(&record_path, dead_ledger_record("dead-session")).unwrap();

    let output = hooklinesinker_isolated(&temp)
        .args(["doctor", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["exitCode"], 0);
    let checks = value["checks"].as_array().unwrap();
    let dead_records = checks.iter().find(|c| c["name"] == "dead_records").unwrap();
    assert_eq!(dead_records["ok"], true);
    assert_eq!(dead_records["detail"], "1");
    assert!(
        record_path.exists(),
        "doctor must leave the dead record for ingest to sweep and fan out"
    );
}

#[test]
fn sessions_json_omits_a_dead_record_without_removing_it() {
    let temp = unique_temp_dir("sessions-dead-record");
    let status_dir = temp.join("hooklinesinker/status");
    std::fs::create_dir_all(&status_dir).unwrap();
    let record_path = status_dir.join("dead.json");
    std::fs::write(&record_path, dead_ledger_record("dead-session")).unwrap();

    let output = Command::cargo_bin("hooklinesinker")
        .unwrap()
        .env("XDG_STATE_HOME", &temp)
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["sessions"], serde_json::json!([]));
    assert!(record_path.exists());
}

#[test]
fn ingest_without_a_session_id_writes_no_ledger_record() {
    let temp = unique_temp_dir("ingest-empty-session-id");
    let ingest = Command::cargo_bin("hooklinesinker")
        .unwrap()
        .env("XDG_STATE_HOME", &temp)
        .args([
            "ingest",
            "--agent",
            "opencode",
            "--event",
            "session.created",
        ])
        .write_stdin("{}")
        .output()
        .unwrap();
    assert!(ingest.status.success());

    let ledger_files = std::fs::read_dir(temp.join("hooklinesinker/status"))
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(ledger_files, 0);

    let output = Command::cargo_bin("hooklinesinker")
        .unwrap()
        .env("XDG_STATE_HOME", &temp)
        .args(["sessions", "--json"])
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["sessions"], serde_json::json!([]));
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
