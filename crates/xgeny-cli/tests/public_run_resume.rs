use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::tempdir;
use xgeny_local_store::{RunStore, SqliteRunStore};
use xgeny_workgraph::RunEventBody;

const MODEL: &str = "test-qwen-model";
const TOKENIZER: &str = "test-qwen-tokenizer";
const FILE_MARKER: &str = "XGENY-HERMETIC-FILE-MARKER-7c6a51";
const COMPLETION: &str = "verified local marker: XGENY-HERMETIC-FILE-MARKER-7c6a51";
const RAW_RESPONSE_SENTINEL: &str = "RAW-PROVIDER-RESPONSE-MUST-NOT-PERSIST";
const TEST_TIMEOUT: Duration = Duration::from_secs(60);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(120);

struct TwoTurnServer {
    base_url: String,
    requests: Receiver<Vec<u8>>,
    arm_second_request: mpsc::Sender<()>,
    handle: thread::JoinHandle<()>,
}

struct BlockingServer {
    base_url: String,
    request: Receiver<Vec<u8>>,
    release: mpsc::Sender<()>,
    handle: thread::JoinHandle<()>,
}

struct OneTurnServer {
    base_url: String,
    request: Receiver<Vec<u8>>,
    handle: thread::JoinHandle<()>,
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child }
    }

    fn stderr_text(&mut self) -> String {
        let mut stderr = self
            .child
            .stderr
            .take()
            .expect("child stderr should be piped");
        let mut text = String::new();
        stderr
            .read_to_string(&mut text)
            .expect("child stderr should be readable after exit");
        text
    }

    fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

trait BoundedCommandOutput {
    fn bounded_output(&mut self) -> io::Result<Output>;
}

impl BoundedCommandOutput for Command {
    fn bounded_output(&mut self) -> io::Result<Output> {
        let mut child = self.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "xgeny child did not exit before the test deadline",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        child
            .stdout
            .take()
            .expect("child stdout should be piped")
            .read_to_end(&mut stdout)?;
        child
            .stderr
            .take()
            .expect("child stderr should be piped")
            .read_to_end(&mut stderr)?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

impl BlockingServer {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener address should resolve");
        let (request_sender, request) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut stream = accept_with_timeout(&listener).expect("planner call should connect");
            let observed = read_http_request(&mut stream);
            let _ = request_sender.send(observed);
            release_receiver
                .recv_timeout(TEST_TIMEOUT)
                .expect("test should release blocked response");
        });
        Self {
            base_url: format!("http://{address}/v1"),
            request,
            release,
            handle,
        }
    }
}

impl OneTurnServer {
    fn spawn(response: Vec<u8>) -> Self {
        Self::spawn_status(200, response)
    }

    fn spawn_status(status: u16, response: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener address should resolve");
        let (request_sender, request) = mpsc::channel();
        let handle = thread::spawn(move || {
            let mut stream = accept_with_timeout(&listener).expect("planner call should connect");
            let observed = read_http_request(&mut stream);
            let _ = request_sender.send(observed);
            let headers = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            );
            stream
                .write_all(headers.as_bytes())
                .and_then(|()| stream.write_all(&response))
                .expect("provider response should write");
        });
        Self {
            base_url: format!("http://{address}/v1"),
            request,
            handle,
        }
    }
}

impl TwoTurnServer {
    fn spawn() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener address should resolve");
        let (sender, requests) = mpsc::channel();
        let (arm_second_request, second_request_arm) = mpsc::channel();
        let handle = thread::spawn(move || {
            for (index, response) in [plan_response(), completion_response()]
                .into_iter()
                .enumerate()
            {
                if index == 1 {
                    second_request_arm
                        .recv_timeout(PROCESS_TIMEOUT)
                        .expect("test should arm the second provider request");
                }
                let mut stream =
                    accept_with_timeout(&listener).expect("expected planner call should connect");
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
            arm_second_request,
            handle,
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn separate_processes_read_once_continue_with_exact_output_and_replay_offline() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("state");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(workspace.join("README.md"), FILE_MARKER).expect("fixture should write");
    let server = TwoTurnServer::spawn();

    let first = xgeny(&state_root)
        .args([
            "run",
            "--workspace",
            path_text(&workspace),
            "--base-url",
            &server.base_url,
            "--model",
            MODEL,
            "--tokenizer",
            TOKENIZER,
            "--allow-file",
            "README.md",
            "--allow-remote-model-egress",
            "--max-ticks",
            "6",
            "Read the only explicitly allowed local text file, then report its exact marker.",
        ])
        .bounded_output()
        .expect("first xgeny process should run");
    assert_exit(&first, 10);
    assert!(first.stdout.is_empty());
    let first_stderr = stderr(&first);
    assert!(first_stderr.contains("reason=read_approval_required"));
    let run_id = extract_run_id(&first_stderr);
    let first_request = server
        .requests
        .recv_timeout(TEST_TIMEOUT)
        .expect("first planner request should arrive");
    let first_context = planning_context(&first_request);
    assert_eq!(first_context["toolOutputs"], json!([]));
    assert!(first_context.get("planningConstraints").is_none());
    assert!(!String::from_utf8_lossy(&first_request).contains(FILE_MARKER));

    let local_only = xgeny(&state_root)
        .args([
            "resume",
            &run_id,
            "--workspace",
            path_text(&workspace),
            "--allow-file",
            "README.md",
            "--allow-read",
        ])
        .bounded_output()
        .expect("local-only continuation should run");
    assert_exit(&local_only, 10);
    assert!(stderr(&local_only).contains("remote_model_egress_consent_required"));
    assert!(server.requests.try_recv().is_err());

    let database = run_database(&state_root, &run_id);
    let first_store = SqliteRunStore::open_existing(&database).expect("Run store should reopen");
    let after_read = first_store
        .load_current()
        .expect("state should load")
        .expect("state should exist");
    let step = after_read
        .steps
        .values()
        .next()
        .expect("read Step should exist");
    assert_eq!(step.status, xgeny_workgraph::StepStatus::Completed);
    let effect_id = step
        .intent
        .as_ref()
        .expect("read intent should exist")
        .effect_id
        .clone();
    let tool_output = first_store
        .load_tool_output(&effect_id)
        .expect("tool output lookup should work")
        .expect("tool output should be durable before restart");
    assert_eq!(tool_output.output()["content"], FILE_MARKER);
    assert_eq!(
        first_store
            .load_execution_receipts()
            .expect("Receipt should load")
            .len(),
        1
    );
    drop(first_store);
    let before_read_only_preflights =
        fs::read(&database).expect("database bytes should be readable before preflight");

    fs::remove_file(workspace.join("README.md"))
        .expect("source should be removable after durable read");

    let no_egress = xgeny(&state_root)
        .args(["resume", &run_id])
        .bounded_output()
        .expect("consent check process should run");
    assert_exit(&no_egress, 10);
    assert!(stderr(&no_egress).contains("remote_model_egress_consent_required"));
    assert!(server.requests.try_recv().is_err());

    let wrong_workspace = fixture.path().join("different-workspace");
    fs::create_dir(&wrong_workspace).expect("different workspace should create");
    let wrong_root = resume_process(
        &state_root,
        &run_id,
        &wrong_workspace,
        &server.base_url,
        "README.md",
    );
    assert_exit(&wrong_root, 64);
    assert!(server.requests.try_recv().is_err());

    let wrong_catalog = resume_process(
        &state_root,
        &run_id,
        &workspace,
        &server.base_url,
        "different.md",
    );
    assert_exit(&wrong_catalog, 64);
    assert!(server.requests.try_recv().is_err());
    assert_eq!(
        fs::read(&database).expect("database bytes should remain readable"),
        before_read_only_preflights,
        "consent and configuration preflights must not rewrite the database"
    );
    assert!(!sqlite_sidecar(&database, "-wal").exists());
    assert!(!sqlite_sidecar(&database, "-shm").exists());

    server
        .arm_second_request
        .send(())
        .expect("second provider request should arm");
    let second = resume_process(
        &state_root,
        &run_id,
        &workspace,
        &server.base_url,
        "README.md",
    );
    assert_exit(&second, 0);
    assert!(stderr(&second).contains("XGENY_COMPLETED"));
    assert_eq!(String::from_utf8(second.stdout).unwrap(), COMPLETION);
    let second_request = server
        .requests
        .recv_timeout(TEST_TIMEOUT)
        .expect("second planner request should arrive");
    let second_context = planning_context(&second_request);
    assert_eq!(
        second_context["toolOutputs"][0]["output"]["content"],
        FILE_MARKER
    );
    assert_eq!(
        count_bytes(&second_request, FILE_MARKER.as_bytes()),
        1,
        "the exact file observation should enter the request once"
    );
    server.handle.join().expect("provider server should finish");

    let completed_store =
        SqliteRunStore::open_existing(&database).expect("completed store should reopen");
    let before_replay = completed_store
        .load()
        .expect("completed snapshot should load")
        .expect("completed snapshot should exist");
    assert_eq!(
        before_replay
            .records
            .iter()
            .filter(|record| matches!(record.event.body, RunEventBody::ModelCallReserved { .. }))
            .count(),
        2
    );
    drop(completed_store);
    fs::remove_dir_all(&workspace).expect("workspace may disappear after completion");

    let replay = xgeny(&state_root)
        .args(["resume", &run_id, "--allow-remote-model-egress"])
        .env("XGENY_OPENAI_BASE_URL", "not-a-provider-url")
        .env("XGENY_OPENAI_API_KEY", "invalid\ncredential")
        .bounded_output()
        .expect("offline replay process should run");
    assert_exit(&replay, 0);
    assert_eq!(String::from_utf8(replay.stdout).unwrap(), COMPLETION);
    let after_replay = SqliteRunStore::open_existing(&database)
        .expect("store should reopen after replay")
        .load()
        .expect("snapshot should load after replay")
        .expect("snapshot should exist after replay");
    assert_eq!(after_replay.state, before_replay.state);
    assert_eq!(after_replay.records, before_replay.records);

    let manifest = fs::read(state_root.join("runs").join(&run_id).join("manifest.json"))
        .expect("manifest should be readable");
    for forbidden in [
        "README.md",
        FILE_MARKER,
        RAW_RESPONSE_SENTINEL,
        server.base_url.as_str(),
        path_text(&workspace),
    ] {
        assert!(!String::from_utf8_lossy(&manifest).contains(forbidden));
    }
}

#[test]
fn missing_egress_consent_creates_no_run_or_network_request() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("state-must-not-exist");
    let output = xgeny(&state_root)
        .args([
            "run",
            "--workspace",
            path_text(&fixture.path().join("missing-workspace")),
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--model",
            MODEL,
            "--tokenizer",
            TOKENIZER,
            "--allow-file",
            "README.md",
            "goal",
        ])
        .bounded_output()
        .expect("consent-denied invocation should return");
    assert_exit(&output, 10);
    assert!(stderr(&output).contains("remote_model_egress_consent_required"));
    assert!(!state_root.exists());
}

#[test]
fn invalid_preflight_does_not_announce_a_phantom_run() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("state-must-not-exist");
    let output = xgeny(&state_root)
        .args([
            "run",
            "--workspace",
            path_text(&fixture.path().join("missing-workspace")),
            "--base-url",
            "http://127.0.0.1:1/v1",
            "--model",
            MODEL,
            "--tokenizer",
            TOKENIZER,
            "--allow-file",
            "README.md",
            "--allow-remote-model-egress",
            "goal",
        ])
        .bounded_output()
        .expect("invalid invocation should return");
    assert_exit(&output, 64);
    assert!(!stderr(&output).contains("XGENY_STARTED"));
    assert!(!state_root.exists());
}

#[test]
fn unavailable_provider_is_reported_as_the_already_durable_unknown_call() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("state");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(workspace.join("README.md"), FILE_MARKER).expect("fixture should write");
    let unused = TcpListener::bind("127.0.0.1:0").expect("ephemeral address should bind");
    let address = unused
        .local_addr()
        .expect("ephemeral address should resolve");
    drop(unused);

    let output = xgeny(&state_root)
        .args([
            "run",
            "--workspace",
            path_text(&workspace),
            "--base-url",
            &format!("http://{address}/v1"),
            "--model",
            MODEL,
            "--tokenizer",
            TOKENIZER,
            "--allow-file",
            "README.md",
            "--allow-remote-model-egress",
            "goal",
        ])
        .bounded_output()
        .expect("unavailable provider invocation should return");
    assert_exit(&output, 30);
    let output_stderr = stderr(&output);
    assert!(output_stderr.contains("reason=model_call_unknown"));
    let run_id = extract_run_id(&output_stderr);
    let state = SqliteRunStore::open_existing_read_only(run_database(&state_root, &run_id))
        .expect("Run store should reopen read-only")
        .load_current()
        .expect("state should load")
        .expect("state should exist");
    assert!(
        state
            .agent_loop
            .as_ref()
            .and_then(|agent| agent.model_calls.as_ref())
            .and_then(|calls| calls.active_call.as_ref())
            .is_some_and(|call| matches!(
                call.status,
                xgeny_workgraph::ModelCallStatus::Unknown { .. }
            ))
    );
}

#[test]
fn interrupted_reserved_model_call_becomes_unknown_without_egress_or_retry() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("state");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(workspace.join("README.md"), FILE_MARKER).expect("fixture should write");
    let server = BlockingServer::spawn();

    let mut child = ChildGuard::new(
        xgeny(&state_root)
            .args([
                "run",
                "--workspace",
                path_text(&workspace),
                "--base-url",
                &server.base_url,
                "--model",
                MODEL,
                "--tokenizer",
                TOKENIZER,
                "--allow-file",
                "README.md",
                "--allow-remote-model-egress",
                "goal",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("blocked xgeny process should start"),
    );
    server
        .request
        .recv_timeout(TEST_TIMEOUT)
        .expect("reserved provider request should arrive");
    child.terminate();
    let started = child.stderr_text();
    assert!(started.contains("XGENY_STARTED"));
    let run_id = extract_run_id(&started);
    server.release.send(()).expect("server should release");
    server.handle.join().expect("server should finish");

    let inspected = recovery_report(&state_root, &run_id);
    assert_eq!(inspected["active_call"]["status"]["status"], "reserved");
    assert_eq!(inspected["reserved_calls"], 1);

    let first_recovery = xgeny(&state_root)
        .args(["resume", &run_id])
        .bounded_output()
        .expect("recovery process should run");
    assert_exit(&first_recovery, 30);
    assert!(stderr(&first_recovery).contains("reason=model_call_unknown"));
    let database = run_database(&state_root, &run_id);
    let store = SqliteRunStore::open_existing(&database).expect("Run store should reopen");
    let after_mark = store
        .load()
        .expect("snapshot should load")
        .expect("snapshot should exist");
    let status = &after_mark
        .state
        .agent_loop
        .as_ref()
        .and_then(|agent| agent.model_calls.as_ref())
        .and_then(|calls| calls.active_call.as_ref())
        .expect("unknown call should remain active")
        .status;
    assert!(matches!(
        status,
        xgeny_workgraph::ModelCallStatus::Unknown { .. }
    ));
    drop(store);

    let repeated = xgeny(&state_root)
        .args(["resume", &run_id])
        .bounded_output()
        .expect("repeated recovery process should run");
    assert_exit(&repeated, 30);
    let after_repeat = SqliteRunStore::open_existing(&database)
        .expect("Run store should reopen again")
        .load()
        .expect("snapshot should load again")
        .expect("snapshot should still exist");
    assert_eq!(after_repeat.state, after_mark.state);
    assert_eq!(after_repeat.records, after_mark.records);
}

fn recovery_report(state_root: &Path, run_id: &str) -> Value {
    let output = xgeny(state_root)
        // Offline recovery must not parse or resolve model configuration.
        .env("XGENY_OPENAI_INFERENCE_TIMEOUT", "invalid-unused-value")
        .env("XGENY_MODEL_PROFILE", "missing-unused-profile")
        .args(["recover", run_id])
        .bounded_output()
        .expect("recovery inspection should return");
    assert_exit(&output, 0);
    serde_json::from_slice(&output.stdout).expect("bounded recovery report should be JSON")
}

#[test]
#[allow(clippy::too_many_lines)]
fn explicit_discard_of_reserved_call_is_offline_and_lease_guarded() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("state");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(workspace.join("README.md"), FILE_MARKER).expect("fixture should write");
    let server = BlockingServer::spawn();
    let mut child = ChildGuard::new(
        xgeny(&state_root)
            .args([
                "run",
                "goal",
                "--workspace",
                path_text(&workspace),
                "--base-url",
                &server.base_url,
                "--model",
                MODEL,
                "--tokenizer",
                TOKENIZER,
                "--allow-file",
                "README.md",
                "--allow-remote-model-egress",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("blocked process starts"),
    );
    server
        .request
        .recv_timeout(TEST_TIMEOUT)
        .expect("request arrives");
    child.terminate();
    let run_id = extract_run_id(&child.stderr_text());
    server.release.send(()).expect("server releases");
    server.handle.join().expect("server stops");
    let database = run_database(&state_root, &run_id);
    let before = SqliteRunStore::open_existing_read_only(&database)
        .expect("store opens")
        .load()
        .expect("snapshot loads")
        .expect("run exists");
    let report = recovery_report(&state_root, &run_id);
    assert_eq!(report["active_call"]["status"]["status"], "reserved");
    let call_id = report["active_call"]["call_id"].as_str().expect("call ID");
    let lease = xgeny_runtime::LocalRunLease::try_acquire(
        &run_id,
        state_root.join("runs").join(&run_id).join("run.lock"),
    )
    .expect("test holds lease");
    for options in [
        vec!["recover", &run_id],
        vec!["recover", &run_id, "--discard-model-call", call_id],
    ] {
        let busy = xgeny(&state_root)
            .args(options)
            .bounded_output()
            .expect("busy returns");
        assert_exit(&busy, 75);
    }
    drop(lease);

    // Integrity preflight cannot mutate the journal or overwrite a damaged manifest.
    let manifest_path = state_root.join("runs").join(&run_id).join("manifest.json");
    let manifest = fs::read(&manifest_path).expect("manifest readable");
    fs::write(&manifest_path, b"{}").expect("corrupt fixture manifest");
    let corrupt = xgeny(&state_root)
        .args(["recover", &run_id, "--discard-model-call", call_id])
        .bounded_output()
        .expect("corrupt returns");
    assert_exit(&corrupt, 70);
    assert_eq!(fs::read(&manifest_path).expect("manifest readable"), b"{}");
    fs::write(&manifest_path, manifest).expect("restore fixture manifest");
    let unchanged = SqliteRunStore::open_existing_read_only(&database)
        .expect("store opens")
        .load()
        .expect("snapshot loads")
        .expect("run exists");
    assert_eq!(unchanged, before);

    // Recovery does not require or recreate a missing physical workspace.
    fs::rename(&workspace, fixture.path().join("moved-workspace")).expect("fixture moves");
    let result = xgeny(&state_root)
        .args(["recover", &run_id, "--discard-model-call", call_id])
        .bounded_output()
        .expect("discard returns");
    assert_exit(&result, 0);
    let after = SqliteRunStore::open_existing_read_only(&database)
        .expect("store opens")
        .load()
        .expect("snapshot loads")
        .expect("run exists");
    assert_eq!(after.records.len(), before.records.len() + 1);
    assert!(after.state.steps.is_empty());
    let closed = recovery_report(&state_root, &run_id);
    assert_eq!(closed["reserved_calls"], 1);
    assert_eq!(closed["settled_calls"], 1);
    assert_eq!(closed["unknown_calls"], 0);
    assert!(closed["active_call"].is_null());
    assert!(!workspace.exists());
}

#[test]
fn recovery_rejects_missing_runs_without_creating_state_or_echoing_arguments() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("missing-state");
    for run_id in ["../invalid-run", "run-00000000000000000000000000000000"] {
        let result = xgeny(&state_root)
            .args(["recover", run_id, "--discard-model-call", "invalid-call"])
            .bounded_output()
            .expect("invalid recovery returns");
        assert_exit(&result, 64);
        assert!(result.stdout.is_empty());
        assert_eq!(
            stderr(&result).trim(),
            "XGENY_ERROR code=configuration_mismatch"
        );
    }
    assert!(!state_root.exists());
}

#[test]
#[allow(clippy::too_many_lines)]
fn explicit_recovery_never_refunds_exhausted_budget_or_discards_a_newer_call() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("state");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace creates");
    fs::write(workspace.join("README.md"), FILE_MARKER).expect("source writes");
    let unavailable = OneTurnServer::spawn_status(503, Vec::new());
    let base_url = unavailable.base_url.clone();
    let first = xgeny(&state_root)
        .args([
            "run",
            "goal",
            "--workspace",
            path_text(&workspace),
            "--base-url",
            &base_url,
            "--model",
            MODEL,
            "--tokenizer",
            TOKENIZER,
            "--allow-file",
            "README.md",
            "--allow-remote-model-egress",
        ])
        .bounded_output()
        .expect("unavailable call returns");
    assert_exit(&first, 30);
    unavailable
        .request
        .recv_timeout(TEST_TIMEOUT)
        .expect("first request arrives");
    unavailable.handle.join().expect("first server finishes");
    let run_id = extract_run_id(&stderr(&first));
    let initial = recovery_report(&state_root, &run_id);
    assert_eq!(initial["max_model_calls"], 4);
    let mut previous_id = String::new();
    for count in 1..=4 {
        let active = recovery_report(&state_root, &run_id);
        assert_eq!(active["reserved_calls"], count);
        let id = active["active_call"]["call_id"]
            .as_str()
            .expect("active call");
        assert_ne!(id, previous_id);
        if !previous_id.is_empty() {
            let stale = xgeny(&state_root)
                .args(["recover", &run_id, "--discard-model-call", &previous_id])
                .bounded_output()
                .expect("stale request returns");
            assert_exit(&stale, 64);
            assert_eq!(recovery_report(&state_root, &run_id), active);
        }
        let discard = xgeny(&state_root)
            .args(["recover", &run_id, "--discard-model-call", id])
            .bounded_output()
            .expect("explicit discard returns");
        assert_exit(&discard, 0);
        let closed = recovery_report(&state_root, &run_id);
        assert_eq!(closed["reserved_calls"], count);
        assert_eq!(closed["settled_calls"], count);
        previous_id = id.to_owned();
        if count < 4 {
            let next_server = OneTurnServer::spawn_status(503, Vec::new());
            let next = resume_process(
                &state_root,
                &run_id,
                &workspace,
                &next_server.base_url,
                "README.md",
            );
            assert_exit(&next, 30);
            next_server
                .request
                .recv_timeout(TEST_TIMEOUT)
                .expect("one fresh request arrives");
            next_server.handle.join().expect("next server finishes");
        }
    }
    let exhausted = recovery_report(&state_root, &run_id);
    let listener = TcpListener::bind("127.0.0.1:0").expect("new endpoint listens");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let fresh_endpoint = format!(
        "http://{}/v1",
        listener.local_addr().expect("local address")
    );
    let stopped = resume_process(
        &state_root,
        &run_id,
        &workspace,
        &fresh_endpoint,
        "README.md",
    );
    assert_exit(&stopped, 10);
    assert_eq!(
        listener
            .accept()
            .expect_err("exhausted budget sends no request")
            .kind(),
        io::ErrorKind::WouldBlock
    );
    assert_eq!(recovery_report(&state_root, &run_id), exhausted);
}

#[test]
#[allow(clippy::too_many_lines)]
fn explicit_recovery_preserves_budget_and_verified_effect_before_separate_resume() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("state");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(workspace.join("README.md"), FILE_MARKER).expect("fixture should write");
    let server = OneTurnServer::spawn(plan_response());
    let output = xgeny(&state_root)
        .args([
            "run",
            "goal",
            "--workspace",
            path_text(&workspace),
            "--base-url",
            &server.base_url,
            "--model",
            MODEL,
            "--tokenizer",
            TOKENIZER,
            "--allow-file",
            "README.md",
            "--allow-read",
            "--allow-remote-model-egress",
        ])
        .bounded_output()
        .expect("one verified read then unavailable provider should return");
    assert_exit(&output, 30);
    server
        .request
        .recv_timeout(TEST_TIMEOUT)
        .expect("first request should arrive");
    server.handle.join().expect("server should finish");
    let run_id = extract_run_id(&stderr(&output));
    let database = run_database(&state_root, &run_id);
    let before = SqliteRunStore::open_existing_read_only(&database)
        .expect("store should open")
        .load()
        .expect("snapshot should load")
        .expect("run exists");
    assert_eq!(before.state.steps.len(), 1);
    assert!(
        before
            .state
            .steps
            .values()
            .all(|step| step.status == xgeny_workgraph::StepStatus::Completed)
    );
    let report = recovery_report(&state_root, &run_id);
    assert_eq!(report["reserved_calls"], 2);
    assert_eq!(report["settled_calls"], 1);
    assert_eq!(report["active_call"]["status"]["status"], "unknown");
    let call_id = report["active_call"]["call_id"]
        .as_str()
        .expect("active ID exists");
    for wrong_id in [
        "",
        "not-a-call",
        "control\nvalue",
        &"x".repeat(4096),
        &format!("model-call-{}", "0".repeat(64)),
    ] {
        let wrong = xgeny(&state_root)
            .args(["recover", &run_id, "--discard-model-call", wrong_id])
            .bounded_output()
            .expect("wrong ID returns");
        assert_exit(&wrong, 64);
        assert_eq!(
            stderr(&wrong).trim(),
            "XGENY_ERROR code=configuration_mismatch"
        );
        assert!(wrong.stdout.is_empty());
    }
    let unchanged = SqliteRunStore::open_existing_read_only(&database)
        .expect("store should open")
        .load()
        .expect("snapshot loads")
        .expect("run exists");
    assert_eq!(unchanged, before);

    let discarded = xgeny(&state_root)
        .env("XGENY_OPENAI_INFERENCE_TIMEOUT", "unused-invalid")
        .args(["recover", &run_id, "--discard-model-call", call_id])
        .bounded_output()
        .expect("discard returns");
    assert_exit(&discarded, 0);
    let closed: Value = serde_json::from_slice(&discarded.stdout).expect("JSON report");
    assert_eq!(closed["discarded_call_id"], call_id);
    assert_eq!(closed["reserved_calls"], 2);
    assert_eq!(closed["settled_calls"], 2);
    assert_eq!(closed["max_model_calls"], report["max_model_calls"]);
    assert!(closed["active_call"].is_null());
    let after = SqliteRunStore::open_existing_read_only(&database)
        .expect("store should open")
        .load()
        .expect("snapshot loads")
        .expect("run exists");
    assert_eq!(after.records.len(), before.records.len() + 1);
    assert_eq!(after.state.steps, before.state.steps);
    assert!(matches!(
        after.records.last().expect("settlement exists").event.body,
        RunEventBody::ModelCallSettled {
            settlement: xgeny_workgraph::ModelCallSettlement::Abandoned {
                reason: xgeny_workgraph::ModelCallAbandonmentReason::RecoveryDiscarded
            },
            ..
        }
    ));
    let repeated = xgeny(&state_root)
        .args(["recover", &run_id, "--discard-model-call", call_id])
        .bounded_output()
        .expect("repeat returns");
    assert_exit(&repeated, 64);
    let no_egress = xgeny(&state_root)
        .args(["resume", &run_id])
        .bounded_output()
        .expect("resume requires separate consent");
    assert_exit(&no_egress, 10);
    let unchanged = SqliteRunStore::open_existing_read_only(&database)
        .expect("store should open")
        .load()
        .expect("snapshot loads")
        .expect("run exists");
    assert_eq!(unchanged, after);

    // Remove the source to prove the already verified effect is not re-executed.
    fs::remove_file(workspace.join("README.md")).expect("fixture source can be removed");
    let completion = OneTurnServer::spawn(completion_response());
    let resumed = resume_process(
        &state_root,
        &run_id,
        &workspace,
        &completion.base_url,
        "README.md",
    );
    assert_exit(&resumed, 0);
    let request = completion
        .request
        .recv_timeout(TEST_TIMEOUT)
        .expect("fresh reservation calls provider");
    assert!(String::from_utf8_lossy(&request).contains(FILE_MARKER));
    completion
        .handle
        .join()
        .expect("completion server should finish");
    let final_report = recovery_report(&state_root, &run_id);
    assert_eq!(final_report["reserved_calls"], 3);
    assert_eq!(final_report["settled_calls"], 3);
    let store = SqliteRunStore::open_existing_read_only(&database).expect("final store opens");
    assert_eq!(
        store
            .load_execution_receipts()
            .expect("receipts load")
            .len(),
        1
    );
    let stale = xgeny(&state_root)
        .args(["recover", &run_id, "--discard-model-call", call_id])
        .bounded_output()
        .expect("completed run cannot discard");
    assert_exit(&stale, 64);
}

#[test]
#[allow(clippy::too_many_lines)]
fn outcome_commit_failure_is_immediately_uncertain_and_offline_resume_never_reexecutes() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("state");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(workspace.join("README.md"), FILE_MARKER).expect("fixture should write");
    let server = OneTurnServer::spawn(plan_response());

    let planned = xgeny(&state_root)
        .args([
            "run",
            "--workspace",
            path_text(&workspace),
            "--base-url",
            &server.base_url,
            "--model",
            MODEL,
            "--tokenizer",
            TOKENIZER,
            "--allow-file",
            "README.md",
            "--allow-remote-model-egress",
            "goal",
        ])
        .bounded_output()
        .expect("planning process should run");
    assert_exit(&planned, 10);
    let run_id = extract_run_id(&stderr(&planned));
    server
        .request
        .recv_timeout(TEST_TIMEOUT)
        .expect("planner request should arrive");
    server.handle.join().expect("planner server should finish");

    let database = run_database(&state_root, &run_id);
    let connection = rusqlite::Connection::open(&database).expect("fault fixture should open");
    connection
        .execute_batch(
            r#"
            CREATE TRIGGER test_abort_effect_succeeded
            BEFORE INSERT ON run_events
            WHEN instr(CAST(NEW.event_json AS TEXT), '"type":"effect_succeeded"') > 0
            BEGIN
                SELECT RAISE(ABORT, 'injected outcome commit failure');
            END;
            "#,
        )
        .expect("outcome fault trigger should install");
    drop(connection);

    let first_failure = xgeny(&state_root)
        .args([
            "resume",
            &run_id,
            "--workspace",
            path_text(&workspace),
            "--allow-file",
            "README.md",
            "--allow-read",
        ])
        .bounded_output()
        .expect("faulted read process should return");
    assert_exit(&first_failure, 30);
    assert!(stderr(&first_failure).contains("reason=effect_outcome_unknown"));

    let executing_store =
        SqliteRunStore::open_existing(&database).expect("Executing store should reopen");
    let executing = executing_store
        .load()
        .expect("Executing snapshot should load")
        .expect("Executing snapshot should exist");
    assert!(
        executing
            .state
            .steps
            .values()
            .any(|step| { step.status == xgeny_workgraph::StepStatus::Executing })
    );
    assert_eq!(
        executing
            .records
            .iter()
            .filter(|record| matches!(
                record.event.body,
                RunEventBody::EffectExecutionStarted { .. }
            ))
            .count(),
        1
    );
    drop(executing_store);

    let connection = rusqlite::Connection::open(&database).expect("fault fixture should reopen");
    connection
        .execute_batch("DROP TRIGGER test_abort_effect_succeeded;")
        .expect("outcome fault trigger should remove");
    drop(connection);

    let recovered = xgeny(&state_root)
        .args(["resume", &run_id])
        .bounded_output()
        .expect("offline effect recovery should run");
    assert_exit(&recovered, 30);
    assert!(stderr(&recovered).contains("reason=effect_outcome_unknown"));
    assert_eq!(
        recovery_report(&state_root, &run_id)["effect_recovery_required"],
        true
    );
    let after_mark_store =
        SqliteRunStore::open_existing(&database).expect("unknown store should reopen");
    let after_mark = after_mark_store
        .load()
        .expect("unknown snapshot should load")
        .expect("unknown snapshot should exist");
    assert!(
        after_mark
            .state
            .steps
            .values()
            .any(|step| { step.status == xgeny_workgraph::StepStatus::EffectUnknown })
    );
    assert_eq!(
        after_mark
            .records
            .iter()
            .filter(|record| matches!(record.event.body, RunEventBody::EffectBecameUnknown { .. }))
            .count(),
        1
    );
    drop(after_mark_store);

    let repeated = xgeny(&state_root)
        .args(["resume", &run_id])
        .bounded_output()
        .expect("repeated offline recovery should return");
    assert_exit(&repeated, 30);
    let after_repeat = SqliteRunStore::open_existing(&database)
        .expect("unknown store should reopen again")
        .load()
        .expect("repeated snapshot should load")
        .expect("repeated snapshot should exist");
    assert_eq!(after_repeat.state, after_mark.state);
    assert_eq!(after_repeat.records, after_mark.records);
}

fn resume_process(
    state_root: &Path,
    run_id: &str,
    workspace: &Path,
    base_url: &str,
    allow_file: &str,
) -> Output {
    xgeny(state_root)
        .env("XGENY_OPENAI_BASE_URL", base_url)
        .args([
            "resume",
            run_id,
            "--workspace",
            path_text(workspace),
            "--allow-file",
            allow_file,
            "--allow-remote-model-egress",
        ])
        .bounded_output()
        .expect("resume process should run")
}

fn xgeny(state_root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xgeny"));
    command
        .env("XGENY_STATE_HOME", state_root)
        .env_remove("XGENY_OPENAI_API_KEY");
    command
}

fn run_database(state_root: &Path, run_id: &str) -> PathBuf {
    state_root.join("runs").join(run_id).join("run.sqlite3")
}

fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn path_text(path: &Path) -> &str {
    path.to_str().expect("test paths should be UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr should be UTF-8")
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected process result; stdout={:?}, stderr={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn extract_run_id(stderr: &str) -> String {
    let run_id = stderr
        .split_whitespace()
        .find_map(|field| field.strip_prefix("run_id="))
        .expect("status should contain a Run ID");
    assert_eq!(run_id.len(), 36);
    run_id.to_owned()
}

fn planning_context(request: &[u8]) -> Value {
    let header_end = find_header_end(request).expect("HTTP headers should end");
    let body: Value = serde_json::from_slice(&request[header_end + 4..])
        .expect("planner request body should be JSON");
    let prompt: Value = serde_json::from_str(
        body["messages"][1]["content"]
            .as_str()
            .expect("planner user message should be text"),
    )
    .expect("planner prompt should be JSON");
    prompt["planningContext"].clone()
}

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream.read(&mut chunk).expect("request should read");
        assert_ne!(read, 0, "request ended before headers completed");
        request.extend_from_slice(&chunk[..read]);
        if let Some(header_end) = find_header_end(&request) {
            let header =
                std::str::from_utf8(&request[..header_end]).expect("HTTP headers should be UTF-8");
            let content_length = header
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("length should parse"))
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

fn accept_with_timeout(listener: &TcpListener) -> io::Result<TcpStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + TEST_TIMEOUT;
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
                        "planner call did not connect before the test deadline",
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

fn provider_response(content: &Value) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "id": RAW_RESPONSE_SENTINEL,
        "model": MODEL,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content.to_string()},
            "finish_reason": "stop"
        }]
    }))
    .expect("provider response should serialize")
}

fn plan_response() -> Vec<u8> {
    provider_response(&json!({
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

fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}
