use std::process::Command;

fn xgeny() -> Command {
    Command::new(env!("CARGO_BIN_EXE_xgeny"))
}

#[test]
fn protocol_check_succeeds_and_reports_conformance_scope() {
    let output = xgeny()
        .args(["protocol", "check"])
        .output()
        .expect("xgeny should execute");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("XGENy protocol v0.1: PASS"));
    assert!(stdout.contains("schemas: 9"));
    assert!(stdout.contains("fixtures: 20 (11 valid, 9 invalid)"));
    assert!(stdout.contains("reference resolution: bundled/offline"));
}

#[test]
fn version_is_available_without_running_protocol_checks() {
    let output = xgeny()
        .arg("--version")
        .output()
        .expect("xgeny should execute");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)
            .expect("stdout should be UTF-8")
            .trim(),
        format!("xgeny {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_command_returns_nonzero_exit() {
    let output = xgeny()
        .arg("unknown-command")
        .output()
        .expect("xgeny should execute");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("unrecognized subcommand"));
}
