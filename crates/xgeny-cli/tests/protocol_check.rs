use std::process::{Command, Stdio};

use tempfile::tempdir;

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
fn licenses_are_embedded_and_available_offline() {
    let output = xgeny()
        .arg("licenses")
        .output()
        .expect("xgeny should execute");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("===== XGENy project license ====="));
    assert!(stdout.contains("===== Cargo dependency notices ====="));
    assert!(stdout.contains("XGENy CLI Third-Party License Notices"));
    assert!(stdout.contains("===== Rust standard library notices ====="));
    assert!(stdout.contains("Copyright notices for The Rust Standard Library"));
    assert!(stdout.contains("===== musl C runtime notices ====="));
    assert!(stdout.contains("Copyright © 2005-2020 Rich Felker, et al."));
    assert!(
        stdout.contains("musl as a whole is licensed under the following standard MIT license")
    );
    assert!(stdout.contains("===== LLVM libunwind notices ====="));
    assert!(stdout.contains("University of Illinois/NCSA"));
    assert!(stdout.contains("Open Source License"));
    assert!(stdout.contains("Copyright (c) 2009-2014 by the contributors listed in CREDITS.TXT"));
}

#[cfg(unix)]
#[test]
fn licenses_treat_an_early_reader_close_as_success() {
    let mut child = xgeny()
        .arg("licenses")
        .stdout(Stdio::piped())
        .spawn()
        .expect("xgeny should execute");
    drop(child.stdout.take().expect("stdout should be piped"));

    let status = child.wait().expect("xgeny should exit");
    assert!(status.success());
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

#[test]
fn environment_backed_run_arguments_reach_explicit_egress_consent_gate() {
    let workspace = tempdir().expect("workspace should exist");
    let output = xgeny()
        .current_dir(workspace.path())
        .env("XGENY_OPENAI_BASE_URL", "http://127.0.0.1:18000/v1")
        .env("XGENY_OPENAI_MODEL", "test-model")
        .env_remove("XGENY_OPENAI_TOKENIZER")
        .env_remove("XGENY_OPENAI_API_KEY")
        .args(["run", "--allow-file", "README.md", "test goal"])
        .output()
        .expect("xgeny should parse environment-backed model settings");

    assert_eq!(output.status.code(), Some(10));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("reason=remote_model_egress_consent_required"));
}

#[test]
fn run_help_names_environment_contract_without_exposing_values() {
    let sentinel = "SENSITIVE-MODEL-SETTING-MUST-NOT-APPEAR";
    let output = xgeny()
        .env("XGENY_OPENAI_BASE_URL", sentinel)
        .env("XGENY_OPENAI_MODEL", sentinel)
        .env("XGENY_OPENAI_TOKENIZER", sentinel)
        .args(["run", "--help"])
        .output()
        .expect("xgeny help should execute");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("XGENY_OPENAI_BASE_URL"));
    assert!(stdout.contains("XGENY_OPENAI_MODEL"));
    assert!(stdout.contains("XGENY_OPENAI_TOKENIZER"));
    assert!(stdout.contains("XGENY_OPENAI_API_KEY"));
    assert!(!stdout.contains(sentinel));
}

#[test]
fn model_check_help_names_secret_environment_contract_without_exposing_values() {
    let sentinel = "SENSITIVE-MODEL-CHECK-SETTING-MUST-NOT-APPEAR";
    let output = xgeny()
        .env("XGENY_OPENAI_BASE_URL", sentinel)
        .env("XGENY_OPENAI_MODEL", sentinel)
        .env("XGENY_OPENAI_TOKENIZER", sentinel)
        .env("XGENY_OPENAI_API_KEY", sentinel)
        .args(["model", "check", "--help"])
        .output()
        .expect("xgeny model check help should execute");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("XGENY_OPENAI_BASE_URL"));
    assert!(stdout.contains("XGENY_OPENAI_MODEL"));
    assert!(stdout.contains("XGENY_OPENAI_TOKENIZER"));
    assert!(stdout.contains("XGENY_OPENAI_API_KEY"));
    assert!(!stdout.contains(sentinel));
}
