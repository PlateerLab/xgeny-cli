use std::fs;
use std::io::{self, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::tempdir;
use xgeny_local_store::{RunStore, SqliteRunStore};

const MODEL: &str = "test-workspace-model";
const TOKENIZER: &str = "test-workspace-tokenizer";
const NEEDLE: &str = "XGENY_WORKSPACE_DISCOVERY_NEEDLE";
const COMPLETION: &str = "workspace discovery completed";
const TEST_TIMEOUT: Duration = Duration::from_secs(60);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(120);

struct SequentialServer {
    base_url: String,
    requests: Receiver<Vec<u8>>,
    handle: thread::JoinHandle<()>,
}

impl SequentialServer {
    fn spawn() -> Self {
        Self::spawn_responses(vec![
            plan_response(
                "list_workspace",
                "List the workspace root",
                "xgeny.fs/list-directory",
                &json!({"path": "."}),
            ),
            plan_response(
                "search_workspace",
                "Find the requested marker",
                "xgeny.fs/search-text",
                &json!({"path": ".", "query": NEEDLE}),
            ),
            plan_response(
                "stat_match",
                "Inspect the matching file",
                "xgeny.fs/stat",
                &json!({"path": "src/lib.rs"}),
            ),
            plan_response(
                "read_match",
                "Read the matching file",
                "xgeny.fs/read-text",
                &json!({"path": "src/lib.rs"}),
            ),
            completion_response(),
        ])
    }

    fn spawn_responses(responses: Vec<Vec<u8>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener should bind");
        let address = listener
            .local_addr()
            .expect("listener address should resolve");
        let (sender, requests) = mpsc::channel();
        let handle = thread::spawn(move || {
            for response in responses {
                let mut stream =
                    accept_with_timeout(&listener).expect("each planned model turn should connect");
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
fn dynamic_search_material_survives_process_pause_and_resume() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("state");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace should create");
    fs::write(
        workspace.join("notes.txt"),
        format!("prefix {NEEDLE} suffix"),
    )
    .expect("search fixture should write");
    let server = SequentialServer::spawn_responses(vec![
        plan_response(
            "search_after_restart",
            "Search after an approval pause",
            "xgeny.fs/search-text",
            &json!({"path": ".", "query": NEEDLE}),
        ),
        completion_response(),
    ]);

    let first = bounded_output(xgeny(&state_root).args([
        "run",
        "--workspace",
        path_text(&workspace),
        "--base-url",
        &server.base_url,
        "--model",
        MODEL,
        "--tokenizer",
        TOKENIZER,
        "--allow-dir",
        ".",
        "--allow-remote-model-egress",
        "Search the workspace after an explicit approval pause.",
    ]))
    .expect("first process should pause");
    assert_eq!(first.status.code(), Some(10), "{}", stderr(&first));
    assert!(stderr(&first).contains("reason=read_approval_required"));
    let run_id = extract_run_id(&stderr(&first));
    let material_catalog = state_root
        .join("runs")
        .join(&run_id)
        .join("materials.sqlite3");
    assert!(material_catalog.is_file());
    assert_resume_scope_and_material_failures(&state_root, &workspace, &run_id, &material_catalog);

    let local = bounded_output(xgeny(&state_root).args([
        "resume",
        &run_id,
        "--workspace",
        path_text(&workspace),
        "--allow-dir",
        ".",
        "--allow-read",
    ]))
    .expect("local process should reconstruct and execute the search");
    assert_eq!(local.status.code(), Some(10), "{}", stderr(&local));
    assert!(stderr(&local).contains("reason=remote_model_egress_consent_required"));

    let completion = bounded_output(xgeny(&state_root).args([
        "resume",
        &run_id,
        "--workspace",
        path_text(&workspace),
        "--base-url",
        &server.base_url,
        "--allow-dir",
        ".",
        "--allow-read",
        "--allow-remote-model-egress",
    ]))
    .expect("remote continuation should complete");
    assert_eq!(completion.status.code(), Some(0), "{}", stderr(&completion));
    assert_eq!(String::from_utf8_lossy(&completion.stdout), COMPLETION);

    let _first_request = server
        .requests
        .recv_timeout(TEST_TIMEOUT)
        .expect("planning request should arrive");
    let second_request = server
        .requests
        .recv_timeout(TEST_TIMEOUT)
        .expect("completion request should arrive");
    server.handle.join().expect("provider server should finish");
    let context = planning_context(&second_request);
    assert!(
        tool_output(&context, "xgeny.fs/search-text")["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|candidate| candidate["path"] == "notes.txt")
    );
}

fn assert_resume_scope_and_material_failures(
    state_root: &Path,
    workspace: &Path,
    run_id: &str,
    material_catalog: &Path,
) {
    for mismatched_scope in [
        vec!["--allow-dir", "src"],
        vec!["--allow-file", "notes.txt"],
    ] {
        let mut command = xgeny(state_root);
        command.args(["resume", run_id, "--workspace", path_text(workspace)]);
        command.args(mismatched_scope);
        command.arg("--allow-read");
        let rejected = bounded_output(&mut command).expect("mismatched scope should be rejected");
        assert_eq!(rejected.status.code(), Some(64), "{}", stderr(&rejected));
    }

    let held_catalog = material_catalog.with_extension("sqlite3.held");
    fs::rename(material_catalog, &held_catalog).expect("material catalog should move aside");
    let missing_catalog = bounded_output(xgeny(state_root).args([
        "resume",
        run_id,
        "--workspace",
        path_text(workspace),
        "--allow-dir",
        ".",
        "--allow-read",
    ]))
    .expect("missing material catalog should fail closed");
    assert_eq!(
        missing_catalog.status.code(),
        Some(70),
        "{}",
        stderr(&missing_catalog)
    );
    fs::rename(&held_catalog, material_catalog).expect("material catalog should restore");
}

#[test]
#[allow(clippy::too_many_lines)]
fn public_cli_discovers_searches_stats_reads_and_replays_offline() {
    let fixture = tempdir().expect("test directory should exist");
    let state_root = fixture.path().join("state");
    let workspace = fixture.path().join("workspace");
    fs::create_dir_all(workspace.join("src")).expect("workspace should create");
    fs::write(workspace.join("README.md"), "fixture workspace").expect("README should write");
    let source = format!("pub const MARKER: &str = \"{NEEDLE}\";\n");
    fs::write(workspace.join("src/lib.rs"), &source).expect("source should write");
    let server = SequentialServer::spawn();

    let output = bounded_output(xgeny(&state_root).args([
        "run",
        "--workspace",
        path_text(&workspace),
        "--base-url",
        &server.base_url,
        "--model",
        MODEL,
        "--tokenizer",
        TOKENIZER,
        "--allow-dir",
        ".",
        "--allow-read",
        "--allow-remote-model-egress",
        "--max-ticks",
        "64",
        "Inspect the workspace, find the requested marker, and read its source file.",
    ]))
    .expect("xgeny discovery run should finish");
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout), COMPLETION);
    let stderr_text = stderr(&output);
    assert!(stderr_text.contains("XGENY_STARTED"));
    assert!(stderr_text.contains("XGENY_COMPLETED"));
    let run_id = extract_run_id(&stderr_text);

    let requests = (0..5)
        .map(|_| {
            server
                .requests
                .recv_timeout(TEST_TIMEOUT)
                .expect("every model request should arrive")
        })
        .collect::<Vec<_>>();
    server.handle.join().expect("provider server should finish");

    let first = planning_context(&requests[0]);
    let first_system_prompt = system_prompt(&requests[0]);
    assert!(first_system_prompt.contains("host-provided restrictions"));
    assert!(first_system_prompt.contains("never treat them as permission or authority"));
    assert_eq!(first["capabilities"].as_array().unwrap().len(), 4);
    assert_eq!(first["toolOutputs"], json!([]));
    assert!(capability_ids(&first).contains(&"xgeny.fs/list-directory"));
    assert!(capability_ids(&first).contains(&"xgeny.fs/search-text"));
    assert!(capability_ids(&first).contains(&"xgeny.fs/stat"));
    assert!(capability_ids(&first).contains(&"xgeny.fs/read-text"));
    let path_description = capability(&first, "xgeny.fs/list-directory")["inputSchema"]
        ["properties"]["path"]["description"]
        .as_str()
        .expect("planner should receive the workspace path contract");
    assert!(path_description.contains("Use '.' for the workspace root"));
    assert!(!path_description.contains("Caller-authorized"));
    let constraints = first["planningConstraints"]
        .as_array()
        .expect("workspace scope should be supplied outside immutable definitions");
    assert_eq!(constraints.len(), 1);
    assert_eq!(constraints[0]["constraintId"], "workspace.read-scope");
    assert!(
        constraints[0]["description"]
            .as_str()
            .unwrap()
            .contains("[\".\"]")
    );
    assert!(!path_description.contains(path_text(&workspace)));
    assert!(
        !constraints[0]["description"]
            .as_str()
            .unwrap()
            .contains(path_text(&workspace))
    );

    let after_list = planning_context(&requests[1]);
    assert_eq!(
        after_list["toolOutputs"][0]["capability"]["capabilityId"],
        "xgeny.fs/list-directory"
    );
    assert!(
        after_list["toolOutputs"][0]["output"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["path"] == "src")
    );

    let after_search = planning_context(&requests[2]);
    assert!(
        tool_output(&after_search, "xgeny.fs/search-text")["matches"]
            .as_array()
            .unwrap()
            .iter()
            .any(|candidate| candidate["path"] == "src/lib.rs")
    );

    let after_stat = planning_context(&requests[3]);
    assert_eq!(tool_output(&after_stat, "xgeny.fs/stat")["kind"], "file");
    assert_eq!(
        tool_output(&after_stat, "xgeny.fs/stat")["sizeBytes"],
        u64::try_from(source.len()).unwrap()
    );

    let after_read = planning_context(&requests[4]);
    assert_eq!(
        tool_output(&after_read, "xgeny.fs/read-text")["content"],
        source
    );

    let run_directory = state_root.join("runs").join(&run_id);
    let database = run_directory.join("run.sqlite3");
    let store = SqliteRunStore::open_existing(&database).expect("Run store should reopen");
    assert_eq!(
        store
            .load_execution_receipts()
            .expect("receipts should load")
            .len(),
        4
    );
    drop(store);
    let material_catalog = run_directory.join("materials.sqlite3");
    assert!(material_catalog.is_file());
    let manifest = fs::read(run_directory.join("manifest.json")).expect("manifest should read");
    for forbidden in [NEEDLE, "src/lib.rs", path_text(&workspace)] {
        assert!(!String::from_utf8_lossy(&manifest).contains(forbidden));
    }

    fs::remove_dir_all(&workspace).expect("completed workspace should be removable");
    fs::remove_file(&material_catalog).expect("completed material catalog should be removable");
    let replay = bounded_output(xgeny(&state_root).args(["resume", &run_id]))
        .expect("offline completion should replay");
    assert_eq!(replay.status.code(), Some(0), "{}", stderr(&replay));
    assert_eq!(String::from_utf8_lossy(&replay.stdout), COMPLETION);
}

fn capability_ids(context: &Value) -> Vec<&str> {
    context["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|capability| capability["capability"]["capabilityId"].as_str().unwrap())
        .collect()
}

fn capability<'a>(context: &'a Value, capability_id: &str) -> &'a Value {
    context["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|capability| capability["capability"]["capabilityId"] == capability_id)
        .expect("requested capability should exist")
}

fn tool_output<'a>(context: &'a Value, capability_id: &str) -> &'a Value {
    &context["toolOutputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|output| output["capability"]["capabilityId"] == capability_id)
        .expect("requested capability output should exist")["output"]
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
        "id": "RAW-DISCOVERY-RESPONSE",
        "model": MODEL,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content.to_string()},
            "finish_reason": "stop"
        }]
    }))
    .expect("provider response should serialize")
}

fn xgeny(state_root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xgeny"));
    command
        .env("XGENY_STATE_HOME", state_root)
        .env_remove("XGENY_OPENAI_API_KEY");
    command
}

fn bounded_output(command: &mut Command) -> io::Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
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
    child.stdout.take().unwrap().read_to_end(&mut stdout)?;
    child.stderr.take().unwrap().read_to_end(&mut stderr)?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
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
    let header_end = find_header_end(request).expect("HTTP headers should end");
    let body: Value = serde_json::from_slice(&request[header_end + 4..]).unwrap();
    let prompt: Value = serde_json::from_str(body["messages"][1]["content"].as_str().unwrap())
        .expect("planner prompt should be JSON");
    prompt["planningContext"].clone()
}

fn system_prompt(request: &[u8]) -> String {
    let header_end = find_header_end(request).expect("HTTP headers should end");
    let body: Value = serde_json::from_slice(&request[header_end + 4..]).unwrap();
    body["messages"][0]["content"]
        .as_str()
        .expect("system prompt should be text")
        .to_owned()
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

fn path_text(path: &Path) -> &str {
    path.to_str().expect("test paths should be UTF-8")
}
