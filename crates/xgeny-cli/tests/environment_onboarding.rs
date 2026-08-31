use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::tempdir;

const MODEL: &str = "environment-model";
const TIMEOUT: Duration = Duration::from_secs(60);

struct CompletionServer {
    base_url: String,
    handle: thread::JoinHandle<Vec<u8>>,
}

struct CatalogServer {
    base_url: String,
    handle: thread::JoinHandle<Vec<u8>>,
}

impl CatalogServer {
    fn spawn(response: &Value) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener should bind");
        listener
            .set_nonblocking(true)
            .expect("test listener should become nonblocking");
        let address = listener
            .local_addr()
            .expect("listener address should resolve");
        let response = serde_json::to_vec(&response).expect("response should encode");
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + TIMEOUT;
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "catalog request did not arrive");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("catalog accept failed: {error}"),
                }
            };
            stream
                .set_nonblocking(false)
                .expect("catalog stream should become blocking");
            stream
                .set_read_timeout(Some(TIMEOUT))
                .expect("request timeout should configure");
            let request = read_http_headers(&mut stream);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            )
            .and_then(|()| stream.write_all(&response))
            .expect("response should write");
            request
        });
        Self {
            base_url: format!("http://{address}/v1"),
            handle,
        }
    }
}

impl CompletionServer {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener should bind");
        listener
            .set_nonblocking(true)
            .expect("test listener should become nonblocking");
        let address = listener
            .local_addr()
            .expect("listener address should resolve");
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + TIMEOUT;
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "planner request did not arrive");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("planner accept failed: {error}"),
                }
            };
            stream
                .set_nonblocking(false)
                .expect("planner stream should become blocking");
            stream
                .set_read_timeout(Some(TIMEOUT))
                .expect("request timeout should configure");
            let request = read_http_request(&mut stream);
            let proposal = json!({
                "formatVersion": 1,
                "kind": "plan",
                "steps": [{
                    "key": "read_allowed_file",
                    "objective": "Read the only allow-listed text resource",
                    "dependsOn": [],
                    "capability": {
                        "capabilityId": "xgeny.fs/read-text",
                        "contractVersion": "1.0.0"
                    },
                    "arguments": {"path": "README.md"}
                }],
                "summary": ""
            })
            .to_string();
            let response = serde_json::to_vec(&json!({
                "model": MODEL,
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": proposal},
                    "finish_reason": "stop"
                }]
            }))
            .expect("response should encode");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            )
            .and_then(|()| stream.write_all(&response))
            .expect("response should write");
            request
        });
        Self {
            base_url: format!("http://{address}/v1"),
            handle,
        }
    }
}

#[test]
fn environment_model_current_workspace_and_tokenizer_fallback_reach_real_request() {
    let fixture = tempdir().expect("fixture should exist");
    let workspace = fixture.path().join("workspace");
    let state = fixture.path().join("state");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(workspace.join("README.md"), "fixture").expect("file should write");
    let server = CompletionServer::spawn();

    let output = xgeny(&state)
        .current_dir(&workspace)
        .env("XGENY_OPENAI_BASE_URL", &server.base_url)
        .env("XGENY_OPENAI_MODEL", MODEL)
        .env("XGENY_OPENAI_API_KEY", "AMBIENT-KEY-MUST-NOT-BE-SENT")
        .env_remove("XGENY_OPENAI_TOKENIZER")
        .args([
            "run",
            "--allow-file",
            "README.md",
            "--allow-remote-model-egress",
            "test environment onboarding",
        ])
        .output()
        .expect("xgeny should run");

    assert_read_approval_pause(&output);
    let request = server.handle.join().expect("server should finish");
    assert!(
        !String::from_utf8_lossy(&request)
            .to_ascii_lowercase()
            .contains("authorization:")
    );
    assert_eq!(request_body(&request)["model"], MODEL);

    let run_id = extract_run_id(&String::from_utf8(output.stderr).unwrap());
    let manifest = read_manifest(&state, &run_id);
    assert_eq!(manifest["record"]["model"], MODEL);
    assert_eq!(manifest["record"]["tokenizer"], MODEL);
}

#[test]
fn command_line_model_settings_override_environment_values() {
    let fixture = tempdir().expect("fixture should exist");
    let workspace = fixture.path().join("workspace");
    let state = fixture.path().join("state");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(workspace.join("README.md"), "fixture").expect("file should write");
    let server = CompletionServer::spawn();

    let output = xgeny(&state)
        .current_dir(&workspace)
        .env("XGENY_OPENAI_BASE_URL", "http://127.0.0.1:1/v1")
        .env("XGENY_OPENAI_MODEL", "wrong-environment-model")
        .env("XGENY_OPENAI_TOKENIZER", "wrong-environment-tokenizer")
        .args([
            "run",
            "--base-url",
            &server.base_url,
            "--model",
            MODEL,
            "--tokenizer",
            "explicit-tokenizer",
            "--allow-file",
            "README.md",
            "--allow-remote-model-egress",
            "test command precedence",
        ])
        .output()
        .expect("xgeny should run");

    assert_read_approval_pause(&output);
    let request = server.handle.join().expect("server should finish");
    assert_eq!(request_body(&request)["model"], MODEL);
    let run_id = extract_run_id(&String::from_utf8(output.stderr).unwrap());
    let manifest = read_manifest(&state, &run_id);
    assert_eq!(manifest["record"]["model"], MODEL);
    assert_eq!(manifest["record"]["tokenizer"], "explicit-tokenizer");
}

#[test]
fn model_check_uses_environment_without_inference_or_local_state() {
    let fixture = tempdir().expect("fixture should exist");
    let state = fixture.path().join("state");
    let server = CatalogServer::spawn(&json!({
        "object": "list",
        "data": [{"id": MODEL, "object": "model"}]
    }));

    let output = xgeny(&state)
        .env("XGENY_OPENAI_BASE_URL", &server.base_url)
        .env("XGENY_OPENAI_MODEL", MODEL)
        .env("XGENY_OPENAI_API_KEY", "AMBIENT-KEY-MUST-NOT-BE-SENT")
        .env_remove("XGENY_OPENAI_TOKENIZER")
        .args(["model", "check"])
        .output()
        .expect("xgeny model check should run");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    assert!(stdout.contains("XGENy model check: PASS"));
    assert!(stdout.contains("model catalog: exact model advertised"));
    assert!(stdout.contains("inference requests: 0"));
    assert!(!state.exists(), "model check must not create Run state");

    let request = server.handle.join().expect("server should finish");
    let request_text = String::from_utf8(request).expect("request should be UTF-8");
    assert!(request_text.starts_with("GET /v1/models HTTP/1.1\r\n"));
    assert!(!request_text.to_ascii_lowercase().contains("authorization:"));
    assert!(
        !request_text
            .to_ascii_lowercase()
            .contains("content-length:")
    );
    assert!(!request_text.contains("chat/completions"));
    assert!(!request_text.contains("AMBIENT-KEY-MUST-NOT-BE-SENT"));
}

#[test]
fn model_check_command_line_settings_override_environment_values() {
    let fixture = tempdir().expect("fixture should exist");
    let state = fixture.path().join("state");
    let server = CatalogServer::spawn(&json!({
        "data": [{"id": MODEL}]
    }));

    let output = xgeny(&state)
        .env("XGENY_OPENAI_BASE_URL", "http://127.0.0.1:1/v1")
        .env("XGENY_OPENAI_MODEL", "wrong-environment-model")
        .env("XGENY_OPENAI_TOKENIZER", "wrong-environment-tokenizer")
        .args([
            "model",
            "check",
            "--base-url",
            &server.base_url,
            "--model",
            MODEL,
            "--tokenizer",
            "explicit-tokenizer",
        ])
        .output()
        .expect("xgeny model check should run");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(!state.exists(), "model check must not create Run state");
    let request = server.handle.join().expect("server should finish");
    assert!(
        String::from_utf8(request)
            .expect("request should be UTF-8")
            .starts_with("GET /v1/models HTTP/1.1\r\n")
    );
}

#[test]
fn model_check_failure_is_redacted_and_does_not_create_local_state() {
    let fixture = tempdir().expect("fixture should exist");
    let state = fixture.path().join("state");
    let server = CatalogServer::spawn(&json!({
        "data": [{"id": "RAW-PROVIDER-SENTINEL"}]
    }));
    let base_url = server.base_url.clone();

    let output = xgeny(&state)
        .env("XGENY_OPENAI_BASE_URL", &base_url)
        .env("XGENY_OPENAI_MODEL", MODEL)
        .args(["model", "check"])
        .output()
        .expect("xgeny model check should run");

    assert_eq!(output.status.code(), Some(65));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("XGENy model check: FAIL"));
    assert!(stderr.contains("reason=model_not_advertised"));
    assert!(!stderr.contains("RAW-PROVIDER-SENTINEL"));
    assert!(!stderr.contains(&base_url));
    assert!(
        !state.exists(),
        "failed model check must not create Run state"
    );
    let _ = server.handle.join().expect("server should finish");
}

#[test]
fn model_check_reports_actionable_transport_configuration_without_state() {
    let fixture = tempdir().expect("fixture should exist");
    let state = fixture.path().join("state");
    let output = xgeny(&state)
        .env("XGENY_OPENAI_BASE_URL", "http://localhost:8000/v1")
        .env("XGENY_OPENAI_MODEL", MODEL)
        .args(["model", "check"])
        .output()
        .expect("xgeny model check should reject locally");

    assert_eq!(output.status.code(), Some(64));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("stderr should be UTF-8");
    assert!(stderr.contains("reason=plaintext_endpoint_must_be_loopback"));
    assert!(!stderr.contains("localhost:8000"));
    assert!(
        !state.exists(),
        "configuration check must not create Run state"
    );
}

#[test]
fn invalid_transport_is_rejected_before_any_run_state_is_created() {
    let fixture = tempdir().expect("fixture should exist");
    let workspace = fixture.path().join("workspace");
    let state = fixture.path().join("state");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(workspace.join("README.md"), "fixture").expect("file should write");

    let output = xgeny(&state)
        .current_dir(&workspace)
        .env("XGENY_OPENAI_BASE_URL", "http://192.0.2.1:8000/v1")
        .env("XGENY_OPENAI_MODEL", MODEL)
        .args([
            "run",
            "--allow-file",
            "README.md",
            "--allow-remote-model-egress",
            "test transport preflight",
        ])
        .output()
        .expect("xgeny should reject locally");

    assert_configuration_before_state(&output, &state);
}

#[test]
fn invalid_https_credential_is_rejected_before_any_run_state_is_created() {
    let fixture = tempdir().expect("fixture should exist");
    let workspace = fixture.path().join("workspace");
    let state = fixture.path().join("state");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(workspace.join("README.md"), "fixture").expect("file should write");

    let output = xgeny(&state)
        .current_dir(&workspace)
        .env("XGENY_OPENAI_BASE_URL", "https://provider.example/v1")
        .env("XGENY_OPENAI_MODEL", MODEL)
        .env("XGENY_OPENAI_API_KEY", "invalid\ncredential")
        .args([
            "run",
            "--allow-file",
            "README.md",
            "--allow-remote-model-egress",
            "test credential preflight",
        ])
        .output()
        .expect("xgeny should reject locally");

    assert_configuration_before_state(&output, &state);
}

#[cfg(unix)]
#[test]
fn non_utf8_https_credential_is_rejected_before_any_run_state_is_created() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    let fixture = tempdir().expect("fixture should exist");
    let workspace = fixture.path().join("workspace");
    let state = fixture.path().join("state");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(workspace.join("README.md"), "fixture").expect("file should write");

    let output = xgeny(&state)
        .current_dir(&workspace)
        .env("XGENY_OPENAI_BASE_URL", "https://provider.example/v1")
        .env("XGENY_OPENAI_MODEL", MODEL)
        .env("XGENY_OPENAI_API_KEY", OsString::from_vec(vec![b't', 0xff]))
        .args([
            "run",
            "--allow-file",
            "README.md",
            "--allow-remote-model-egress",
            "test non-UTF-8 credential preflight",
        ])
        .output()
        .expect("xgeny should reject locally");

    assert_configuration_before_state(&output, &state);
}

fn xgeny(state: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xgeny"));
    command
        .env("XGENY_STATE_HOME", state)
        .env_remove("XGENY_OPENAI_API_KEY");
    command
}

fn read_manifest(state: &Path, run_id: &str) -> Value {
    let path: PathBuf = state.join("runs").join(run_id).join("manifest.json");
    serde_json::from_slice(&fs::read(path).expect("manifest should read"))
        .expect("manifest should be JSON")
}

fn assert_read_approval_pause(output: &Output) {
    assert_eq!(output.status.code(), Some(10));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("reason=read_approval_required"));
}

fn assert_configuration_before_state(output: &Output, state: &Path) {
    assert_eq!(output.status.code(), Some(64));
    assert!(output.stdout.is_empty());
    assert!(
        !state.exists(),
        "configuration failure must precede state creation"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("XGENY_STARTED"),
        "configuration failure must not announce a durable Run"
    );
}

fn extract_run_id(stderr: &str) -> String {
    stderr
        .split_whitespace()
        .find_map(|field| field.strip_prefix("run_id="))
        .expect("status should contain a Run ID")
        .to_owned()
}

fn request_body(request: &[u8]) -> Value {
    let header_end = find_header_end(request).expect("request should contain headers");
    serde_json::from_slice(&request[header_end + 4..]).expect("request should contain JSON")
}

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut expected_length = None;
    loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).expect("request should read");
        assert_ne!(read, 0, "request ended before its body completed");
        request.extend_from_slice(&chunk[..read]);
        if expected_length.is_none()
            && let Some(header_end) = find_header_end(&request)
        {
            let headers = std::str::from_utf8(&request[..header_end])
                .expect("request headers should be UTF-8");
            let body_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("length should parse"))
                })
                .expect("request should have content length");
            expected_length = Some(header_end + 4 + body_length);
        }
        if expected_length.is_some_and(|length| request.len() >= length) {
            return request;
        }
    }
}

fn read_http_headers(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).expect("request should read");
        assert_ne!(read, 0, "request ended before its headers completed");
        request.extend_from_slice(&chunk[..read]);
        if find_header_end(&request).is_some() {
            return request;
        }
    }
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}
