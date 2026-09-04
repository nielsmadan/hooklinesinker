use assert_cmd::Command;
use serde_json::Value;

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
        &["ingest", "--agent", "claude", "--event", "start"],
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
