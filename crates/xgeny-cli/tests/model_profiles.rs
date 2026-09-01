use std::fs;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::tempdir;

const MODEL: &str = "profile-model";
const TIMEOUT: Duration = Duration::from_secs(60);

struct ModelServer {
    base_url: String,
    handle: thread::JoinHandle<Vec<Vec<u8>>>,
}

impl ModelServer {
    fn spawn(expected_requests: usize, planner_after_probe: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener should bind");
        listener
            .set_nonblocking(true)
            .expect("test listener should become nonblocking");
        let address = listener.local_addr().expect("address should resolve");
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + TIMEOUT;
            let mut requests = Vec::new();
            let mut chat_requests = 0_usize;
            while requests.len() < expected_requests {
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(pair) => break pair,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "model request did not arrive");
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("model server accept failed: {error}"),
                    }
                };
                stream
                    .set_nonblocking(false)
                    .expect("accepted model stream should become blocking");
                stream
                    .set_read_timeout(Some(TIMEOUT))
                    .expect("read timeout should configure");
                let request = read_http_request(&mut stream);
                let request_line = std::str::from_utf8(
                    &request[..request
                        .windows(2)
                        .position(|window| window == b"\r\n")
                        .expect("request line should end")],
                )
                .expect("request line should be UTF-8");
                let response = if request_line == "GET /v1/models HTTP/1.1" {
                    json!({"object":"list","data":[{"id":MODEL,"object":"model"}]})
                } else {
                    assert_eq!(request_line, "POST /v1/chat/completions HTTP/1.1");
                    chat_requests += 1;
                    let content = if planner_after_probe && chat_requests > 1 {
                        json!({
                            "formatVersion": 1,
                            "kind": "plan",
                            "steps": [{
                                "key": "read_profile_file",
                                "objective": "Read the configured file",
                                "dependsOn": [],
                                "capability": {
                                    "capabilityId": "xgeny.fs/read-text",
                                    "contractVersion": "1.0.0"
                                },
                                "arguments": {"path":"README.md"}
                            }],
                            "summary": ""
                        })
                        .to_string()
                    } else {
                        json!({"status":"ok"}).to_string()
                    };
                    json!({
                        "model": MODEL,
                        "choices": [{
                            "index": 0,
                            "message": {"role":"assistant","content":content},
                            "finish_reason": "stop"
                        }]
                    })
                };
                let body = serde_json::to_vec(&response).expect("response should encode");
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .and_then(|()| stream.write_all(&body))
                .expect("response should write");
                requests.push(request);
            }
            requests
        });
        Self {
            base_url: format!("http://{address}/v1"),
            handle,
        }
    }
}

#[test]
fn setup_persists_non_secret_profile_and_run_resolves_it_without_environment() {
    let fixture = tempdir().expect("fixture should exist");
    let config = fixture.path().join("config");
    let state = fixture.path().join("state");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(workspace.join("README.md"), "profile fixture").expect("file should write");
    let server = ModelServer::spawn(3, true);

    let setup = xgeny(&config, &state)
        .args([
            "model",
            "setup",
            "--base-url",
            &server.base_url,
            "--model",
            MODEL,
        ])
        .output()
        .expect("setup should execute");
    assert_success(&setup);
    let setup_stdout = String::from_utf8(setup.stdout).expect("stdout should be UTF-8");
    assert!(setup_stdout.contains("XGENy model setup: PASS"));
    assert!(setup_stdout.contains("chat completions: strict JSON compatible"));
    assert!(!state.exists(), "setup must not create Run state");

    let profile_bytes = fs::read(config.join("model-profiles.json")).expect("profile should read");
    let profile_text = String::from_utf8(profile_bytes).expect("profile should be UTF-8");
    assert!(profile_text.contains(MODEL));
    assert!(!profile_text.to_ascii_lowercase().contains("api_key"));
    assert!(!profile_text.contains("RAW-SECRET-SENTINEL"));

    let run = xgeny(&config, &state)
        .current_dir(&workspace)
        .args([
            "run",
            "--allow-file",
            "README.md",
            "--allow-remote-model-egress",
            "use active profile",
        ])
        .output()
        .expect("run should execute");
    assert_eq!(run.status.code(), Some(10));
    assert!(String::from_utf8_lossy(&run.stderr).contains("reason=read_approval_required"));

    let requests = server.handle.join().expect("server should finish");
    assert_eq!(requests.len(), 3);
    let setup_probe = request_body(&requests[1]);
    assert_eq!(setup_probe["model"], MODEL);
    assert_eq!(setup_probe["response_format"]["type"], "json_schema");
    assert_eq!(
        setup_probe["response_format"]["json_schema"]["strict"],
        true
    );
    let planner = request_body(&requests[2]);
    assert_eq!(planner["model"], MODEL);
}

#[test]
fn list_use_logout_remove_manage_profiles_without_network_or_plaintext_secret_files() {
    let fixture = tempdir().expect("fixture should exist");
    let config = fixture.path().join("config");
    let state = fixture.path().join("state");
    let server = ModelServer::spawn(4, false);

    for name in ["alpha", "beta"] {
        let output = xgeny(&config, &state)
            .args([
                "model",
                "setup",
                "--name",
                name,
                "--base-url",
                &server.base_url,
                "--model",
                MODEL,
            ])
            .output()
            .expect("setup should execute");
        assert_success(&output);
    }
    server.handle.join().expect("server should finish");

    let listed = xgeny(&config, &state)
        .args(["model", "list"])
        .output()
        .expect("list should execute");
    assert_success(&listed);
    let stdout = String::from_utf8(listed.stdout).unwrap();
    assert!(stdout.contains("  alpha"));
    assert!(stdout.contains("* beta"));
    assert!(!stdout.contains(&server.base_url));

    assert_success(
        &xgeny(&config, &state)
            .args(["model", "use", "alpha"])
            .output()
            .expect("use should execute"),
    );
    assert_success(
        &xgeny(&config, &state)
            .args(["model", "logout", "alpha"])
            .output()
            .expect("logout should execute"),
    );
    assert_success(
        &xgeny(&config, &state)
            .args(["model", "remove", "beta"])
            .output()
            .expect("remove should execute"),
    );

    let final_list = xgeny(&config, &state)
        .args(["model", "list"])
        .output()
        .expect("final list should execute");
    assert_success(&final_list);
    let stdout = String::from_utf8(final_list.stdout).unwrap();
    assert!(stdout.contains("* alpha"));
    assert!(!stdout.contains("beta"));
    assert!(stdout.contains("authentication=external_or_none"));
}

#[test]
fn token_stdin_is_rejected_for_plaintext_before_network_or_profile_mutation() {
    let fixture = tempdir().expect("fixture should exist");
    let config = fixture.path().join("config");
    let state = fixture.path().join("state");
    let output = xgeny(&config, &state)
        .args([
            "model",
            "setup",
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--model",
            MODEL,
            "--token-stdin",
        ])
        .output()
        .expect("setup should reject");
    assert_eq!(output.status.code(), Some(64));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("reason=api_key_requires_https"));
    assert!(!config.exists());
    assert!(!state.exists());
}

#[test]
fn token_stdin_is_ephemeral_and_redacted_for_https_headless_checks() {
    let fixture = tempdir().expect("fixture should exist");
    let config = fixture.path().join("config");
    let state = fixture.path().join("state");
    let mut child = xgeny(&config, &state)
        .args([
            "model",
            "check",
            "--base-url",
            "https://127.0.0.1:1/v1",
            "--model",
            MODEL,
            "--token-stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("check should spawn");
    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(b"RAW-SECRET-SENTINEL\n")
        .expect("token should write");
    let output = child.wait_with_output().expect("check should finish");
    assert_eq!(output.status.code(), Some(69));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("RAW-SECRET-SENTINEL"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("RAW-SECRET-SENTINEL"));
    assert!(
        !config.exists(),
        "ephemeral token must not create profile state"
    );
    assert!(!state.exists(), "model check must not create Run state");
}

fn xgeny(config: &Path, state: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xgeny"));
    command
        .env("XGENY_CONFIG_HOME", config)
        .env("XGENY_STATE_HOME", state)
        .env_remove("XGENY_MODEL_PROFILE")
        .env_remove("XGENY_OPENAI_BASE_URL")
        .env_remove("XGENY_OPENAI_MODEL")
        .env_remove("XGENY_OPENAI_TOKENIZER")
        .env_remove("XGENY_OPENAI_API_KEY");
    command
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut expected_length = None;
    loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).expect("request should read");
        assert_ne!(read, 0, "request ended before completion");
        request.extend_from_slice(&chunk[..read]);
        if expected_length.is_none()
            && let Some(header_end) = find_header_end(&request)
        {
            let headers = std::str::from_utf8(&request[..header_end])
                .expect("request headers should be UTF-8");
            let content_length = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("length should parse"))
            });
            expected_length = Some(header_end + 4 + content_length.unwrap_or(0));
        }
        if expected_length.is_some_and(|length| request.len() >= length) {
            return request;
        }
    }
}

fn request_body(request: &[u8]) -> Value {
    let header_end = find_header_end(request).expect("request should contain headers");
    serde_json::from_slice(&request[header_end + 4..]).expect("request body should be JSON")
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}
