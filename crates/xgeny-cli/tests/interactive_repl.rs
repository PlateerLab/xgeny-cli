use std::fs;
use std::io::{self, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::tempdir;
use xgeny_local_store::{RunStore, SqliteRunStore};

const MODEL: &str = "test-repl-model";
const TOKENIZER: &str = "test-repl-tokenizer";
const COMPLETION: &str = "interactive workspace read completed";
const TEST_TIMEOUT: Duration = Duration::from_secs(60);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(180);

struct SequentialServer {
    base_url: String,
    requests: Receiver<Vec<u8>>,
    handle: thread::JoinHandle<()>,
}

#[cfg(unix)]
struct DelayedServer {
    base_url: String,
    request: Receiver<Vec<u8>>,
    release: mpsc::Sender<()>,
    handle: thread::JoinHandle<()>,
}

#[cfg(unix)]
impl DelayedServer {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener address should resolve");
        let (request_sender, request) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut stream = accept_with_timeout(&listener).expect("model turn should connect");
            let request = read_http_request(&mut stream);
            request_sender
                .send(request)
                .expect("request observation should send");
            release_receiver
                .recv_timeout(TEST_TIMEOUT)
                .expect("test should release the response");
            let response = plan_response(
                "cancel_before_read",
                "Read only after the cancellation boundary",
                "xgeny.fs/read-text",
                &json!({"path": "README.md"}),
            );
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            let _ = stream
                .write_all(headers.as_bytes())
                .and_then(|()| stream.write_all(&response));
        });
        Self {
            base_url: format!("http://{address}/v1"),
            request,
            release,
            handle,
        }
    }
}

impl SequentialServer {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener address should resolve");
        let (sender, requests) = mpsc::channel();
        let responses = [
            plan_response(
                "read_readme",
                "Read the workspace README",
                "xgeny.fs/read-text",
                &json!({"path": "README.md"}),
            ),
            plan_response(
                "check_git",
                "Check the catalogued Git executable without a shell",
                "xgeny.process/execute",
                &json!({
                    "executable": "git",
                    "args": ["--version"],
                    "cwd": ".",
                    "env": {},
                    "timeoutMs": 30000,
                    "maxOutputBytes": 32768
                }),
            ),
            completion_response(),
        ];
        let handle = thread::spawn(move || {
            for response in responses {
                let mut stream =
                    accept_with_timeout(&listener).expect("each model turn should connect");
                let request = read_http_request(&mut stream);
                if sender.send(request).is_err() {
                    return;
                }
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                );
                stream
                    .write_all(headers.as_bytes())
                    .and_then(|()| stream.write_all(&response))
                    .expect("provider response should write");
            }
        });
        Self {
            base_url: format!("http://{address}/v1"),
            requests,
            handle,
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn bare_xgeny_streams_durable_progress_prompts_separate_approvals_and_replays_offline() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("state");
    let config_root = fixture.path().join("config");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(workspace.join("README.md"), "interactive fixture\n")
        .expect("workspace fixture should write");
    let server = SequentialServer::spawn();

    let output = bounded_scripted_output(
        Command::new(env!("CARGO_BIN_EXE_xgeny"))
            .current_dir(&workspace)
            .env("XGENY_STATE_HOME", &state_root)
            .env("XGENY_CONFIG_HOME", &config_root)
            .env("XGENY_OPENAI_BASE_URL", &server.base_url)
            .env("XGENY_OPENAI_MODEL", MODEL)
            .env("XGENY_OPENAI_TOKENIZER", TOKENIZER)
            .env_remove("XGENY_OPENAI_API_KEY"),
        b"Read README.md, check Git, and report the result.\ny\ny\ny\ny\ny\n/status\n/resume\n/exit\n",
    )
    .expect("interactive CLI should exit cleanly");
    assert!(output.status.success(), "{}", stderr(&output));
    let stderr = stderr(&output);
    let stdout = String::from_utf8(output.stdout).expect("terminal output should be UTF-8");

    assert!(stdout.contains("XGENy Developer Preview"));
    assert!(stdout.contains(
        "Allow sending the goal, session context, and tool observations to the model? [y/N]"
    ));
    assert!(stdout.contains("Allow read for this durable continuation? [y/N]"));
    assert!(stdout.contains("Allow execute for this durable continuation? [y/N]"));
    assert_eq!(
        stdout
            .matches("Allow sending the goal, session context, and tool observations to the model?")
            .count(),
        3
    );
    for progress in [
        "progress: model_call_starting",
        "progress: plan_committed",
        "progress: approval_required effect=read",
        "progress: action_authorized effect=read",
        "progress: effect_starting effect=read",
        "progress: effect_committed effect=read",
        "progress: verification_committed",
        "progress: approval_required effect=execute",
        "progress: action_authorized effect=execute",
        "progress: effect_starting effect=execute",
        "progress: effect_committed effect=execute",
        "progress: completion_committed",
    ] {
        assert!(stdout.contains(progress), "missing {progress}: {stdout}");
    }
    assert_eq!(stdout.matches(COMPLETION).count(), 2, "{stdout}");
    assert!(stdout.contains("status: completed"));
    assert!(!stdout.contains("session cleared"));
    assert!(stderr.contains("XGENY_STARTED run_id="));
    let run_id = extract_run_id(&stderr);

    let first_request = server
        .requests
        .recv_timeout(TEST_TIMEOUT)
        .expect("planning request should arrive");
    let process_request = server
        .requests
        .recv_timeout(TEST_TIMEOUT)
        .expect("process planning request should arrive");
    let completion_request = server
        .requests
        .recv_timeout(TEST_TIMEOUT)
        .expect("completion request should arrive");
    server.handle.join().expect("provider server should finish");
    assert_eq!(request_model(&first_request), MODEL);
    assert_eq!(request_model(&process_request), MODEL);
    let context = planning_context(&completion_request);
    let output = context["toolOutputs"]
        .as_array()
        .expect("tool outputs should be present")
        .iter()
        .find(|output| output["capability"]["capabilityId"] == "xgeny.fs/read-text")
        .expect("read output should be present");
    assert_eq!(output["output"]["content"], "interactive fixture\n");
    let process_output = context["toolOutputs"]
        .as_array()
        .expect("tool outputs should be present")
        .iter()
        .find(|output| output["capability"]["capabilityId"] == "xgeny.process/execute")
        .expect("process output should be present");
    assert_eq!(process_output["output"]["success"], true);
    assert!(
        process_output["output"]["stdout"]
            .as_str()
            .expect("process stdout should be text")
            .starts_with("git version")
    );

    let database = state_root.join("runs").join(&run_id).join("run.sqlite3");
    let store = SqliteRunStore::open_existing(database).expect("Run store should reopen");
    let receipts = store
        .load_execution_receipts()
        .expect("execution receipts should load");
    assert_eq!(receipts.len(), 2, "offline /resume must not replay tools");
    assert!(
        !config_root.exists(),
        "environment-only model use must not persist a profile"
    );
}

#[cfg(unix)]
#[test]
fn sigint_during_model_call_stops_at_a_safe_or_unknown_no_replay_boundary() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("state");
    let config_root = fixture.path().join("config");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(workspace.join("README.md"), "must not be read\n")
        .expect("workspace fixture should write");
    let server = DelayedServer::spawn();

    let mut child = Command::new(env!("CARGO_BIN_EXE_xgeny"))
        .current_dir(&workspace)
        .env("XGENY_STATE_HOME", &state_root)
        .env("XGENY_CONFIG_HOME", &config_root)
        .env("XGENY_OPENAI_BASE_URL", &server.base_url)
        .env("XGENY_OPENAI_MODEL", MODEL)
        .env("XGENY_OPENAI_TOKENIZER", TOKENIZER)
        .env_remove("XGENY_OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("interactive CLI should spawn");
    child
        .stdin
        .take()
        .expect("child stdin should exist")
        .write_all(b"/permissions model allow\nRead README.md.\n/exit\n")
        .expect("scripted input should write");

    let request = server
        .request
        .recv_timeout(TEST_TIMEOUT)
        .expect("in-flight model request should arrive");
    assert_eq!(request_model(&request), MODEL);
    let signal_status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("POSIX kill utility should run");
    assert!(signal_status.success());
    server
        .release
        .send(())
        .expect("provider response should release");

    let output = collect_bounded_output(child).expect("cancelled CLI should exit cleanly");
    server.handle.join().expect("provider server should finish");
    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = stderr(&output);
    assert!(stdout.contains("progress: model_call_starting"), "{stdout}");
    let safely_committed = stdout.contains("progress: plan_committed")
        && stdout.contains("paused: user_cancelled")
        && !stdout.contains("recovery_required:");
    let outcome_unknown = !stdout.contains("progress: plan_committed")
        && stdout.contains("recovery_required:")
        && stdout.contains("reason=model_call_unknown");
    assert!(
        safely_committed || outcome_unknown,
        "model cancellation must stop at one documented boundary: {stdout}"
    );
    assert!(!stdout.contains("progress: effect_starting"), "{stdout}");
    let run_id = extract_run_id(&stderr);
    let database = state_root.join("runs").join(run_id).join("run.sqlite3");
    let store = SqliteRunStore::open_existing(database).expect("Run store should reopen");
    assert!(
        store
            .load_execution_receipts()
            .expect("receipts should load")
            .is_empty(),
        "cancellation before approval must not perform an effect"
    );
}

#[cfg(unix)]
#[test]
fn sigint_while_idle_interrupts_input_and_emits_a_fresh_prompt() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("state");
    let config_root = fixture.path().join("config");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace should create");

    let mut child = Command::new(env!("CARGO_BIN_EXE_xgeny"))
        .current_dir(&workspace)
        .env("XGENY_STATE_HOME", &state_root)
        .env("XGENY_CONFIG_HOME", &config_root)
        .env_remove("XGENY_OPENAI_BASE_URL")
        .env_remove("XGENY_OPENAI_MODEL")
        .env_remove("XGENY_OPENAI_TOKENIZER")
        .env_remove("XGENY_OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("interactive CLI should spawn");
    let mut input = child.stdin.take().expect("child stdin should exist");
    let mut stdout = child.stdout.take().expect("child stdout should exist");
    let (prompt_sender, prompt_receiver) = mpsc::channel();
    let stdout_handle = thread::spawn(move || {
        let mut output = Vec::new();
        let mut byte = [0_u8; 1];
        while stdout.read(&mut byte).expect("stdout should read") != 0 {
            output.push(byte[0]);
            if output.ends_with(b"xgeny> ") {
                let _ = prompt_sender.send(());
            }
        }
        output
    });
    prompt_receiver
        .recv_timeout(TEST_TIMEOUT)
        .expect("initial prompt should appear");

    let signal_status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("POSIX kill utility should run");
    assert!(signal_status.success());
    prompt_receiver
        .recv_timeout(TEST_TIMEOUT)
        .expect("fresh prompt should appear after Ctrl+C");
    input
        .write_all(b"/exit\n")
        .expect("exit command should write");
    drop(input);

    let status = wait_bounded_status(&mut child).expect("interactive CLI should exit");
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .expect("child stderr should exist")
        .read_to_end(&mut stderr)
        .expect("stderr should read");
    let stdout = String::from_utf8(stdout_handle.join().expect("stdout reader should finish"))
        .expect("terminal output should be UTF-8");
    assert!(status.success(), "{}", String::from_utf8_lossy(&stderr));
    assert!(stdout.contains("^C\nxgeny> "), "{stdout}");
    assert!(stdout.ends_with("bye\n"), "{stdout}");
    assert!(!state_root.exists());
    assert!(!config_root.exists());
}

fn plan_response(key: &str, objective: &str, capability_id: &str, arguments: &Value) -> Vec<u8> {
    provider_response(&json!({
        "formatVersion": 1,
        "kind": "plan",
        "steps": [{
            "key": key,
            "objective": objective,
            "dependsOn": [],
            "capability": {
                "capabilityId": capability_id,
                "contractVersion": "1.0.0"
            },
            "arguments": arguments
        }],
        "summary": ""
    }))
}

fn completion_response() -> Vec<u8> {
    provider_response(&json!({
        "formatVersion": 1,
        "kind": "completion_candidate",
        "steps": [],
        "summary": COMPLETION
    }))
}

fn provider_response(content: &Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": "repl-test-response",
        "model": MODEL,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content.to_string()},
            "finish_reason": "stop"
        }]
    }))
    .expect("provider response should serialize")
}

fn bounded_scripted_output(command: &mut Command, input: &[u8]) -> io::Result<Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .expect("child stdin should exist")
        .write_all(input)?;
    collect_bounded_output(child)
}

fn collect_bounded_output(mut child: std::process::Child) -> io::Result<Output> {
    let status = wait_bounded_status(&mut child)?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .expect("child stdout should exist")
        .read_to_end(&mut stdout)?;
    child
        .stderr
        .take()
        .expect("child stderr should exist")
        .read_to_end(&mut stderr)?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn wait_bounded_status(child: &mut std::process::Child) -> io::Result<std::process::ExitStatus> {
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "interactive xgeny child did not exit before the deadline",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk).expect("request should read");
        assert_ne!(read, 0, "request ended before headers completed");
        request.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = find_header_end(&request) {
            let header = std::str::from_utf8(&request[..header_end]).unwrap();
            let content_length = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .expect("Content-Length should exist");
            let target = header_end + 4 + content_length;
            while request.len() < target {
                let read = stream.read(&mut chunk).expect("request body should read");
                assert_ne!(read, 0, "request ended before body completed");
                request.extend_from_slice(&chunk[..read]);
            }
            request.truncate(target);
            return request;
        }
    }
}

fn planning_context(request: &[u8]) -> Value {
    let body = request_body(request);
    let prompt: Value = serde_json::from_str(body["messages"][1]["content"].as_str().unwrap())
        .expect("planner prompt should be JSON");
    prompt["planningContext"].clone()
}

fn request_model(request: &[u8]) -> String {
    let header_end = find_header_end(request).expect("HTTP headers should end");
    let body: Value = serde_json::from_slice(&request[header_end + 4..]).unwrap();
    body["model"]
        .as_str()
        .expect("model should be text")
        .to_owned()
}

fn request_body(request: &[u8]) -> Value {
    let header_end = find_header_end(request).expect("HTTP headers should end");
    serde_json::from_slice(&request[header_end + 4..]).expect("request body should be JSON")
}

fn accept_with_timeout(listener: &TcpListener) -> io::Result<TcpStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + PROCESS_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                stream.set_read_timeout(Some(TEST_TIMEOUT))?;
                stream.set_write_timeout(Some(TEST_TIMEOUT))?;
                return Ok(stream);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "planner call did not connect before the deadline",
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn extract_run_id(stderr: &str) -> String {
    stderr
        .split_whitespace()
        .find_map(|field| field.strip_prefix("run_id="))
        .expect("run ID should be announced")
        .to_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
