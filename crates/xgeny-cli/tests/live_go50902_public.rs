use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::tempdir;
use xgeny_local_store::{RunStore, SqliteRunStore};
use xgeny_workgraph::{
    ModelCallRejectionReason, ModelCallSettlement, RunEventBody, StepStatus, ToolOutputRecord,
    VerificationDisposition,
};

const LIVE_CONFIRMATION: &str = "xgeny-go50902-public-cli-v1";
const WORKSPACE_LIVE_CONFIRMATION: &str = "xgeny-go50902-workspace-discovery-v1";
const CODING_LIVE_CONFIRMATION: &str = "xgeny-go50902-coding-loop-v1";
const MODEL: &str = "qwen3.8-27b";
const TOKENIZER: &str = "Qwen/Qwen3.8-27B-FP8";
const PLANNER_ID: &str = "xgeny.live.go50902";
const SSH_TARGET: &str = "go50902";
const MAX_KNOWN_HOSTS_BYTES: u64 = 64 * 1024;
const SSH_READY_TIMEOUT: Duration = Duration::from_secs(20);
const XGENY_PROCESS_TIMEOUT: Duration = Duration::from_secs(420);
const WORKSPACE_SEARCH_KEY: &str = "XGENY_WORKSPACE_LIVE_TARGET";
const CODING_SEARCH_KEY: &str = "XGENY_RC3_CODING_TARGET";
const CODING_COMPLETION: &str = "XGENY-RC3-QWEN-CODING-PASS";

struct TunnelGuard {
    child: Option<Child>,
}

impl TunnelGuard {
    fn start(local_address: SocketAddr, known_hosts: &Path) -> Self {
        let listener = TcpListener::bind(local_address)
            .unwrap_or_else(|_| panic!("live tunnel local address must be unused"));
        drop(listener);
        let forward = format!("127.0.0.1:{}:127.0.0.1:8000", local_address.port());
        let known_hosts_option = format!("UserKnownHostsFile={}", path_text(known_hosts));

        let child = Command::new("ssh")
            .args([
                "-N",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                &known_hosts_option,
                "-o",
                "GlobalKnownHostsFile=/dev/null",
                "-o",
                "KnownHostsCommand=none",
                "-o",
                "VerifyHostKeyDNS=no",
                "-o",
                "UpdateHostKeys=no",
                "-o",
                "HostKeyAlias=go50902",
                "-o",
                "CheckHostIP=no",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ControlMaster=no",
                "-o",
                "GatewayPorts=no",
                "-o",
                "ConnectTimeout=15",
                "-o",
                "LogLevel=ERROR",
                "-L",
                &forward,
                SSH_TARGET,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|_| panic!("live SSH tunnel could not start"));
        let mut tunnel = Self { child: Some(child) };
        let deadline = Instant::now() + SSH_READY_TIMEOUT;
        loop {
            tunnel.require_running();
            match TcpListener::bind(local_address) {
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => return tunnel,
                Ok(listener) => drop(listener),
                Err(error) => panic!(
                    "live tunnel readiness could not be checked: {:?}",
                    error.kind()
                ),
            }
            assert!(
                Instant::now() < deadline,
                "live SSH tunnel did not become ready"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn require_running(&mut self) {
        let child = self
            .child
            .as_mut()
            .unwrap_or_else(|| panic!("live SSH tunnel is not owned by the test"));
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(_)) | Err(_) => panic!("live SSH tunnel exited unexpectedly"),
        }
    }

    fn stop(&mut self) {
        self.require_running();
        let child = self
            .child
            .as_mut()
            .unwrap_or_else(|| panic!("live SSH tunnel is not owned by the test"));
        child
            .kill()
            .unwrap_or_else(|_| panic!("live SSH tunnel could not be stopped"));
        child
            .wait()
            .unwrap_or_else(|_| panic!("live SSH tunnel stop could not be confirmed"));
        self.child.take();
    }

    fn is_stopped(&self) -> bool {
        self.child.is_none()
    }
}

impl Drop for TunnelGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
#[ignore = "requires explicit go50902 SSH and remote-model consent"]
#[allow(clippy::too_many_lines)]
fn public_cli_two_turn_read_and_offline_replay() {
    require_live_confirmation(LIVE_CONFIRMATION);
    let base_url = required_env("XGENY_LIVE_OPENAI_BASE_URL");
    let local_address = loopback_address(&base_url);

    let fixture = tempdir().unwrap_or_else(|_| panic!("live fixture could not be created"));
    let known_hosts = snapshot_known_hosts(fixture.path());
    let state_root = fixture.path().join("state");
    let workspace = fixture.path().join("workspace");
    fs::create_dir(&workspace).unwrap_or_else(|_| panic!("live workspace could not be created"));
    let relative_file = format!("live-{}.txt", random_hex());
    let sentinel = format!("XGENY-LIVE-SENTINEL-{}", random_hex());
    let goal = format!(
        "Use xgeny.fs/read-text to read the exact local file named {relative_file}. After the read completes, return only its complete content as the summary, with no prefix, suffix, Markdown, quotation marks, or explanation."
    );
    require(
        !goal.contains(&sentinel),
        "live goal must not contain the file observation",
    );
    fs::write(workspace.join(&relative_file), sentinel.as_bytes())
        .unwrap_or_else(|_| panic!("live source fixture could not be written"));

    let mut tunnel = TunnelGuard::start(local_address, &known_hosts);

    let first = run_redacted(
        xgeny(&state_root)
            .arg("run")
            .arg("--workspace")
            .arg(&workspace)
            .arg("--base-url")
            .arg(&base_url)
            .arg("--model")
            .arg(MODEL)
            .arg("--tokenizer")
            .arg(TOKENIZER)
            .arg("--planner-id")
            .arg(PLANNER_ID)
            .arg("--allow-file")
            .arg(&relative_file)
            .arg("--allow-remote-model-egress")
            .arg("--max-ticks")
            .arg("32")
            .arg(&goal),
        "first model turn",
    );
    require_exit(&first, 10, "first model turn");
    require(
        first.stdout.is_empty(),
        "first model turn stdout must be empty",
    );
    require_contains(
        &first.stderr,
        b"reason=read_approval_required",
        "first model turn must pause for read approval",
    );
    let run_id = extract_run_id(&first.stderr);
    tunnel.stop();
    require(
        tunnel.is_stopped(),
        "first live tunnel must stop before the local-only read",
    );

    let local_read = run_redacted(
        xgeny(&state_root)
            .arg("resume")
            .arg(&run_id)
            .arg("--workspace")
            .arg(&workspace)
            .arg("--allow-file")
            .arg(&relative_file)
            .arg("--allow-read")
            .arg("--max-ticks")
            .arg("32"),
        "local read turn",
    );
    require_exit(&local_read, 10, "local read turn");
    require(
        local_read.stdout.is_empty(),
        "local read stdout must be empty",
    );
    require_contains(
        &local_read.stderr,
        b"reason=remote_model_egress_consent_required",
        "local read turn must pause before remote egress",
    );

    let database = run_database(&state_root, &run_id);
    let read_store = SqliteRunStore::open_existing_read_only(&database)
        .unwrap_or_else(|_| panic!("live store could not reopen after the local read"));
    let read_snapshot = read_store
        .load()
        .unwrap_or_else(|_| panic!("live snapshot could not load after the local read"))
        .unwrap_or_else(|| panic!("live snapshot was missing after the local read"));
    require(
        read_snapshot.state.steps.len() == 1
            && read_snapshot
                .state
                .steps
                .values()
                .all(|step| step.status == StepStatus::Completed),
        "live read Step must be receipt-completed",
    );
    let intent = read_snapshot
        .state
        .steps
        .values()
        .next()
        .and_then(|step| step.intent.as_ref())
        .unwrap_or_else(|| panic!("live read intent was missing"));
    let output = read_store
        .load_tool_output(&intent.effect_id)
        .unwrap_or_else(|_| panic!("live ToolOutput could not load"))
        .unwrap_or_else(|| panic!("live ToolOutput was missing"));
    require(
        output.output()["content"].as_str() == Some(sentinel.as_str()),
        "live ToolOutput did not contain the exact file observation",
    );
    require(
        read_store
            .load_execution_receipts()
            .unwrap_or_else(|_| panic!("live Receipt could not load"))
            .len()
            == 1,
        "live read must have one verified Receipt",
    );
    drop(read_store);

    fs::remove_file(workspace.join(&relative_file))
        .unwrap_or_else(|_| panic!("live source fixture could not be removed"));
    require(
        !workspace.join(&relative_file).exists(),
        "live source must be absent before the second model turn",
    );
    let mut completion_tunnel = TunnelGuard::start(local_address, &known_hosts);

    let completion = run_redacted(
        xgeny(&state_root)
            .arg("resume")
            .arg(&run_id)
            .arg("--workspace")
            .arg(&workspace)
            .arg("--base-url")
            .arg(&base_url)
            .arg("--allow-file")
            .arg(&relative_file)
            .arg("--allow-remote-model-egress")
            .arg("--max-ticks")
            .arg("32"),
        "second model turn",
    );
    require_exit(&completion, 0, "second model turn");
    require(
        completion.stdout == sentinel.as_bytes(),
        "live completion did not equal the exact durable observation",
    );
    require_contains(
        &completion.stderr,
        b"XGENY_COMPLETED",
        "second model turn did not report completion",
    );

    completion_tunnel.stop();
    require(
        completion_tunnel.is_stopped(),
        "second live tunnel must stop before offline replay",
    );
    fs::remove_dir_all(&workspace)
        .unwrap_or_else(|_| panic!("live workspace could not be removed"));
    require(
        !workspace.exists(),
        "live workspace must be absent before offline replay",
    );

    let completed_store = SqliteRunStore::open_existing_read_only(&database)
        .unwrap_or_else(|_| panic!("completed live store could not reopen"));
    let before_replay = completed_store
        .load()
        .unwrap_or_else(|_| panic!("completed live snapshot could not load"))
        .unwrap_or_else(|| panic!("completed live snapshot was missing"));
    let receipt_count = completed_store
        .load_execution_receipts()
        .unwrap_or_else(|_| panic!("completed live Receipt could not load"))
        .len();
    drop(completed_store);
    verify_durable_live_result(&before_replay, receipt_count);

    let replay = run_redacted(
        xgeny(&state_root).arg("resume").arg(&run_id),
        "offline replay",
    );
    require_exit(&replay, 0, "offline replay");
    require(
        replay.stdout == completion.stdout,
        "offline replay did not byte-match the durable completion",
    );
    require_contains(
        &replay.stderr,
        b"XGENY_COMPLETED",
        "offline replay did not report completion",
    );

    let after_replay = SqliteRunStore::open_existing_read_only(&database)
        .unwrap_or_else(|_| panic!("live store could not reopen after replay"))
        .load()
        .unwrap_or_else(|_| panic!("live snapshot could not load after replay"))
        .unwrap_or_else(|| panic!("live snapshot was missing after replay"));
    require(
        before_replay.state == after_replay.state && before_replay.records == after_replay.records,
        "offline replay mutated the durable journal",
    );

    let workspace_text = path_text(&workspace);
    let state_text = path_text(&state_root);
    for observed in [&first, &local_read, &completion, &replay] {
        require_output_absent(observed, base_url.as_bytes());
        require_output_absent(observed, workspace_text.as_bytes());
        require_output_absent(observed, state_text.as_bytes());
        require_output_absent(observed, relative_file.as_bytes());
        require(
            !contains_bytes(&observed.stderr, sentinel.as_bytes()),
            "live stderr exposed the file observation or model payload",
        );
    }
    let run_directory = state_root.join("runs").join(&run_id);
    let manifest = fs::read(run_directory.join("manifest.json"))
        .unwrap_or_else(|_| panic!("live manifest could not be read"));
    for forbidden in [
        base_url.as_bytes(),
        workspace_text.as_bytes(),
        state_text.as_bytes(),
        relative_file.as_bytes(),
        sentinel.as_bytes(),
    ] {
        require(
            !contains_bytes(&manifest, forbidden),
            "live manifest retained a forbidden runtime value",
        );
    }
    for forbidden in [
        base_url.as_bytes(),
        workspace_text.as_bytes(),
        state_text.as_bytes(),
    ] {
        require_run_directory_absent(&run_directory, forbidden);
    }

    fs::remove_dir_all(&state_root)
        .unwrap_or_else(|_| panic!("temporary live state could not be removed"));
    require(
        !state_root.exists(),
        "temporary live state must be removed after verification",
    );
}

#[test]
#[ignore = "requires explicit go50902 SSH and remote-model consent"]
#[allow(clippy::too_many_lines)]
fn public_cli_workspace_discovery_and_offline_replay() {
    require_live_confirmation(WORKSPACE_LIVE_CONFIRMATION);
    let base_url = required_env("XGENY_LIVE_OPENAI_BASE_URL");
    let local_address = loopback_address(&base_url);

    let fixture = tempdir().unwrap_or_else(|_| panic!("live fixture could not be created"));
    let known_hosts = snapshot_known_hosts(fixture.path());
    let state_root = fixture.path().join("state");
    let workspace = fixture.path().join("workspace");
    let target_directory = workspace.join("knowledge");
    fs::create_dir_all(workspace.join("src"))
        .unwrap_or_else(|_| panic!("live workspace could not be created"));
    fs::create_dir(&target_directory)
        .unwrap_or_else(|_| panic!("live target directory could not be created"));
    let readme = b"A controlled workspace discovery fixture.\n";
    let decoy_source = b"fn main() { println!(\"fixture\"); }\n";
    fs::write(workspace.join("README.md"), readme)
        .unwrap_or_else(|_| panic!("live decoy README could not be written"));
    fs::write(workspace.join("src/main.rs"), decoy_source)
        .unwrap_or_else(|_| panic!("live decoy source could not be written"));

    let target_name = format!("entry-{}.txt", random_hex());
    let target_relative = format!("knowledge/{target_name}");
    let locator = format!("{WORKSPACE_SEARCH_KEY}_{}", random_hex());
    let sentinel = format!("XGENY-LIVE-WORKSPACE-VALUE-{}", random_hex());
    let source = format!("{locator}\nRESULT={sentinel}\n");
    require(
        source.len() != readme.len() && source.len() != decoy_source.len(),
        "live target size must differ from every decoy file",
    );
    fs::write(target_directory.join(&target_name), source.as_bytes())
        .unwrap_or_else(|_| panic!("live target source could not be written"));
    let goal = format!(
        "Investigate this workspace using exactly these read-only observations in order: call xgeny.fs/list-directory on '.', call xgeny.fs/search-text from '.' with the literal query '{locator}', call xgeny.fs/stat on the matching path, and call xgeny.fs/read-text on that same path. Do not guess a path or value. The search preview does not contain the requested result. After all four observations complete, return only the value after 'RESULT=' as the summary, with no prefix, suffix, Markdown, quotation marks, or explanation."
    );
    require(
        !goal.contains(&sentinel) && !goal.contains(&target_name),
        "live discovery goal must not contain the target path or observation",
    );

    let mut tunnel = TunnelGuard::start(local_address, &known_hosts);
    let completion = run_redacted(
        xgeny(&state_root)
            .arg("run")
            .arg("--workspace")
            .arg(&workspace)
            .arg("--base-url")
            .arg(&base_url)
            .arg("--model")
            .arg(MODEL)
            .arg("--tokenizer")
            .arg(TOKENIZER)
            .arg("--planner-id")
            .arg(PLANNER_ID)
            .arg("--allow-dir")
            .arg(".")
            .arg("--allow-read")
            .arg("--allow-remote-model-egress")
            .arg("--max-ticks")
            .arg("96")
            .arg(&goal),
        "workspace discovery",
    );
    require_workspace_completion(&completion, &state_root);
    require(
        completion.stdout == sentinel.as_bytes(),
        "live workspace completion did not equal the exact discovered value",
    );
    require_contains(
        &completion.stderr,
        b"XGENY_COMPLETED",
        "live workspace discovery did not report completion",
    );
    let run_id = extract_run_id(&completion.stderr);
    tunnel.stop();
    require(
        tunnel.is_stopped(),
        "live workspace tunnel must stop before offline verification",
    );

    let run_directory = state_root.join("runs").join(&run_id);
    let database = run_directory.join("run.sqlite3");
    let material_catalog = run_directory.join("materials.sqlite3");
    require(
        material_catalog.is_file(),
        "live workspace material catalog was missing",
    );
    let store = SqliteRunStore::open_existing_read_only(&database)
        .unwrap_or_else(|_| panic!("live workspace store could not reopen"));
    let before_replay = store
        .load()
        .unwrap_or_else(|_| panic!("live workspace snapshot could not load"))
        .unwrap_or_else(|| panic!("live workspace snapshot was missing"));
    verify_durable_workspace_result(
        &store,
        &before_replay,
        &target_relative,
        &locator,
        &sentinel,
        &source,
    );
    drop(store);

    let workspace_text = path_text(&workspace);
    let state_text = path_text(&state_root);
    let manifest = fs::read(run_directory.join("manifest.json"))
        .unwrap_or_else(|_| panic!("live workspace manifest could not be read"));
    for forbidden in [
        base_url.as_bytes(),
        workspace_text.as_bytes(),
        state_text.as_bytes(),
        target_relative.as_bytes(),
        locator.as_bytes(),
        sentinel.as_bytes(),
    ] {
        require(
            !contains_bytes(&manifest, forbidden),
            "live workspace manifest retained a forbidden runtime value",
        );
    }

    fs::remove_dir_all(&workspace)
        .unwrap_or_else(|_| panic!("live workspace could not be removed"));
    fs::remove_file(&material_catalog)
        .unwrap_or_else(|_| panic!("live workspace material catalog could not be removed"));
    require(
        !workspace.exists() && !material_catalog.exists(),
        "live workspace inputs must be absent before offline replay",
    );

    let replay = run_redacted(
        xgeny(&state_root).arg("resume").arg(&run_id),
        "workspace offline replay",
    );
    require_exit(&replay, 0, "workspace offline replay");
    require(
        replay.stdout == completion.stdout,
        "live workspace offline replay did not byte-match completion",
    );
    require_contains(
        &replay.stderr,
        b"XGENY_COMPLETED",
        "live workspace offline replay did not report completion",
    );

    let after_replay = SqliteRunStore::open_existing_read_only(&database)
        .unwrap_or_else(|_| panic!("live workspace store could not reopen after replay"))
        .load()
        .unwrap_or_else(|_| panic!("live workspace snapshot could not load after replay"))
        .unwrap_or_else(|| panic!("live workspace snapshot was missing after replay"));
    require(
        before_replay.state == after_replay.state && before_replay.records == after_replay.records,
        "live workspace offline replay mutated the durable journal",
    );

    for observed in [&completion, &replay] {
        require_output_absent(observed, base_url.as_bytes());
        require_output_absent(observed, workspace_text.as_bytes());
        require_output_absent(observed, state_text.as_bytes());
        require_output_absent(observed, target_relative.as_bytes());
        require_output_absent(observed, locator.as_bytes());
        require(
            !contains_bytes(&observed.stderr, sentinel.as_bytes()),
            "live workspace stderr exposed the discovered value",
        );
    }
    for forbidden in [
        base_url.as_bytes(),
        workspace_text.as_bytes(),
        state_text.as_bytes(),
    ] {
        require_run_directory_absent(&run_directory, forbidden);
    }

    fs::remove_dir_all(&state_root)
        .unwrap_or_else(|_| panic!("temporary live workspace state could not be removed"));
    require(
        !state_root.exists(),
        "temporary live workspace state must be removed after verification",
    );
}

#[test]
#[ignore = "requires explicit go50902 SSH, cargo path, and remote-model consent"]
#[allow(clippy::too_many_lines)]
fn public_cli_qwen_edits_fixes_and_reverifies_rust_project() {
    require_live_confirmation(CODING_LIVE_CONFIRMATION);
    let base_url = required_env("XGENY_LIVE_OPENAI_BASE_URL");
    let local_address = loopback_address(&base_url);
    let cargo_path = PathBuf::from(required_env("XGENY_LIVE_CARGO_PATH"));
    require(
        cargo_path.is_absolute() && cargo_path.is_file(),
        "live cargo path must be an absolute file",
    );

    let fixture = tempdir().unwrap_or_else(|_| panic!("live fixture could not be created"));
    let known_hosts = snapshot_known_hosts(fixture.path());
    let state_root = fixture.path().join("state");
    let workspace = fixture.path().join("workspace");
    fs::create_dir_all(workspace.join("src"))
        .unwrap_or_else(|_| panic!("live coding source directory could not be created"));
    fs::create_dir(workspace.join("tests"))
        .unwrap_or_else(|_| panic!("live coding test directory could not be created"));

    let locator = format!("{CODING_SEARCH_KEY}_{}", random_hex());
    let original_source =
        format!("// {locator}\npub fn release_candidate_value() -> u32 {{\n    40\n}}\n");
    let stage_one_source =
        format!("// {locator}\npub fn release_candidate_value() -> u32 {{\n    41\n}}\n");
    let final_source =
        format!("// {locator}\npub fn release_candidate_value() -> u32 {{\n    42\n}}\n");
    fs::write(
        workspace.join("Cargo.toml"),
        b"[package]\nname = \"xgeny-rc3-live-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap_or_else(|_| panic!("live coding Cargo manifest could not be written"));
    fs::write(workspace.join("src/lib.rs"), original_source.as_bytes())
        .unwrap_or_else(|_| panic!("live coding source could not be written"));
    fs::write(
        workspace.join("tests/acceptance.rs"),
        b"use xgeny_rc3_live_fixture::release_candidate_value;\n\n#[test]\nfn release_candidate_value_is_ready() {\n    assert_eq!(release_candidate_value(), 42);\n}\n",
    )
    .unwrap_or_else(|_| panic!("live coding acceptance test could not be written"));

    let goal = coding_goal(&locator);
    require(
        !coding_goal_discloses_acceptance(&goal, &locator),
        "live coding goal must not reveal the acceptance value or test path",
    );
    let executable_spec = format!("cargo={}", path_text(&cargo_path));

    let mut tunnel = TunnelGuard::start(local_address, &known_hosts);
    let completion = run_redacted(
        xgeny(&state_root)
            .arg("run")
            .arg("--workspace")
            .arg(&workspace)
            .arg("--base-url")
            .arg(&base_url)
            .arg("--model")
            .arg(MODEL)
            .arg("--tokenizer")
            .arg(TOKENIZER)
            .arg("--planner-id")
            .arg(PLANNER_ID)
            .arg("--allow-dir")
            .arg(".")
            .arg("--allow-executable")
            .arg(&executable_spec)
            .arg("--allow-read")
            .arg("--allow-write")
            .arg("--allow-execute")
            .arg("--allow-remote-model-egress")
            .arg("--max-ticks")
            .arg("256")
            .arg(&goal),
        "qwen coding loop",
    );
    require_coding_completion(&completion, &state_root);
    require(
        completion.stdout == CODING_COMPLETION.as_bytes(),
        "live coding completion was not the exact acceptance summary",
    );
    require_contains(
        &completion.stderr,
        b"XGENY_COMPLETED",
        "live coding loop did not report completion",
    );
    let run_id = extract_run_id(&completion.stderr);
    tunnel.stop();
    require(
        tunnel.is_stopped(),
        "live coding tunnel must stop before durable verification",
    );
    require(
        fs::read_to_string(workspace.join("src/lib.rs")).is_ok_and(|source| source == final_source),
        "live coding source did not contain the verified correction",
    );
    require(
        final_source != original_source && final_source != stage_one_source,
        "live coding fixture stages must remain distinct",
    );

    let run_directory = state_root.join("runs").join(&run_id);
    let database = run_directory.join("run.sqlite3");
    let material_catalog = run_directory.join("materials.sqlite3");
    let store = SqliteRunStore::open_existing_read_only(&database)
        .unwrap_or_else(|_| panic!("live coding store could not reopen"));
    let before_replay = store
        .load()
        .unwrap_or_else(|_| panic!("live coding snapshot could not load"))
        .unwrap_or_else(|| panic!("live coding snapshot was missing"));
    verify_durable_coding_result(&store, &before_replay, &material_catalog, &locator);
    drop(store);

    let workspace_text = path_text(&workspace);
    let state_text = path_text(&state_root);
    let cargo_text = path_text(&cargo_path);
    let manifest = fs::read(run_directory.join("manifest.json"))
        .unwrap_or_else(|_| panic!("live coding manifest could not be read"));
    for forbidden in [
        base_url.as_bytes(),
        workspace_text.as_bytes(),
        state_text.as_bytes(),
        cargo_text.as_bytes(),
        locator.as_bytes(),
    ] {
        require(
            !contains_bytes(&manifest, forbidden),
            "live coding manifest retained a forbidden runtime value",
        );
    }

    fs::remove_dir_all(&workspace)
        .unwrap_or_else(|_| panic!("live coding workspace could not be removed"));
    fs::remove_file(&material_catalog)
        .unwrap_or_else(|_| panic!("live coding material catalog could not be removed"));
    require(
        !workspace.exists() && !material_catalog.exists(),
        "live coding inputs must be absent before offline replay",
    );
    let replay = run_redacted(
        xgeny(&state_root).arg("resume").arg(&run_id),
        "coding offline replay",
    );
    require_exit(&replay, 0, "coding offline replay");
    require(
        replay.stdout == completion.stdout,
        "live coding offline replay did not byte-match completion",
    );
    let after_replay = SqliteRunStore::open_existing_read_only(&database)
        .unwrap_or_else(|_| panic!("live coding store could not reopen after replay"))
        .load()
        .unwrap_or_else(|_| panic!("live coding snapshot could not load after replay"))
        .unwrap_or_else(|| panic!("live coding snapshot was missing after replay"));
    require(
        before_replay.state == after_replay.state && before_replay.records == after_replay.records,
        "live coding offline replay mutated the durable journal",
    );

    for observed in [&completion, &replay] {
        for forbidden in [
            base_url.as_bytes(),
            workspace_text.as_bytes(),
            state_text.as_bytes(),
            cargo_text.as_bytes(),
            locator.as_bytes(),
        ] {
            require_output_absent(observed, forbidden);
        }
    }
    // Compiler diagnostics can legitimately retain the workspace path inside
    // private durable ToolOutput. Endpoint, state, and executable host paths
    // are not process observations and must remain absent from the Run files.
    for forbidden in [
        base_url.as_bytes(),
        state_text.as_bytes(),
        cargo_text.as_bytes(),
    ] {
        require_run_directory_absent(&run_directory, forbidden);
    }
    fs::remove_dir_all(&state_root)
        .unwrap_or_else(|_| panic!("temporary live coding state could not be removed"));
    require(
        !state_root.exists(),
        "temporary live coding state must be removed after verification",
    );
}

#[allow(clippy::too_many_lines)]
fn verify_durable_coding_result(
    store: &SqliteRunStore,
    snapshot: &xgeny_local_store::RunSnapshot,
    material_catalog: &Path,
    locator: &str,
) {
    let calls = snapshot
        .state
        .agent_loop
        .as_ref()
        .and_then(|agent| agent.model_calls.as_ref())
        .unwrap_or_else(|| panic!("live coding model-call lifecycle was missing"));
    assert_eq!(
        (
            calls.reserved_calls,
            calls.settled_calls,
            calls.unknown_calls,
            calls.active_call.is_none(),
        ),
        (8, 8, 0, true),
        "live coding model-call lifecycle was not exactly 8/8/0 with no active call",
    );
    require(
        snapshot.state.steps.len() == 7
            && snapshot.state.steps.values().all(|step| {
                step.status == StepStatus::Completed
                    && step.attempts == 1
                    && step.execution_receipt_id.is_some()
                    && step.execution_receipt_digest.is_some()
            }),
        "live coding Steps were not exactly seven once-executed Receipt completions",
    );
    require(
        store
            .load_execution_receipts()
            .unwrap_or_else(|_| panic!("live coding Receipts could not load"))
            .len()
            == 7,
        "live coding Run did not contain exactly seven Receipts",
    );

    let mut completed = Vec::new();
    for record in &snapshot.records {
        let RunEventBody::EffectSucceeded {
            step_id, effect_id, ..
        } = &record.event.body
        else {
            continue;
        };
        let step = snapshot
            .state
            .steps
            .get(step_id)
            .unwrap_or_else(|| panic!("live coding completed Step was missing"));
        let intent = step
            .intent
            .as_ref()
            .unwrap_or_else(|| panic!("live coding completed intent was missing"));
        require(
            intent.effect_id == *effect_id,
            "live coding effect identity did not match its Step",
        );
        let output = store
            .load_tool_output(effect_id)
            .unwrap_or_else(|_| panic!("live coding ToolOutput could not load"))
            .unwrap_or_else(|| panic!("live coding ToolOutput was missing"));
        completed.push((
            step_id.clone(),
            intent.invocation.capability_id.clone(),
            output,
        ));
    }
    let expected_capabilities = [
        "xgeny.fs/search-text",
        "xgeny.fs/read-text",
        "xgeny.fs/apply-patch",
        "xgeny.process/execute",
        "xgeny.fs/apply-patch",
        "xgeny.process/execute",
        "xgeny.process/execute",
    ];
    let completed_capabilities = completed
        .iter()
        .map(|(_, capability, _)| capability.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        completed_capabilities, expected_capabilities,
        "live coding capability order did not match the seven-Step contract",
    );
    for (_, capability, output) in &completed {
        assert_eq!(
            output.capability_id(),
            capability,
            "live coding ToolOutput capability did not match its Step intent",
        );
    }
    require(
        completed[0].2.output()["matches"]
            .as_array()
            .is_some_and(|matches| {
                matches.iter().any(|candidate| {
                    candidate["path"] == "src/lib.rs"
                        && candidate["preview"]
                            .as_str()
                            .is_some_and(|preview| preview.contains(locator))
                })
            }),
        "live coding search did not locate the controlled source",
    );
    require(
        completed[1].2.output()["content"]
            .as_str()
            .is_some_and(|content| content.contains(locator) && content.contains("    40")),
        "live coding read did not observe the original controlled source",
    );
    require(
        completed[2].2.output()["changed"] == true && completed[4].2.output()["changed"] == true,
        "live coding patches did not both report a real change",
    );

    let process_outputs = [&completed[3].2, &completed[5].2, &completed[6].2];
    require(
        process_outputs.iter().all(|output| {
            output.output()["outcome"] == "exited"
                && output.output()["stdoutTruncated"] == false
                && output.output()["stderrTruncated"] == false
        }),
        "live coding process results were not complete exited observations",
    );
    require(
        process_outputs[0].output()["success"] == false
            && process_outputs[0].output()["exitCode"].as_i64().is_some(),
        "live coding first cargo test did not produce a durable nonzero result",
    );
    let failed_test_output = format!(
        "{}\n{}",
        process_outputs[0].output()["stdout"].as_str().unwrap_or(""),
        process_outputs[0].output()["stderr"].as_str().unwrap_or("")
    );
    require(
        failed_test_output.contains("41") && failed_test_output.contains("42"),
        "live coding failed test output did not expose the correction evidence",
    );
    require(
        process_outputs[1].output()["success"] == true
            && process_outputs[2].output()["success"] == true,
        "live coding re-test and build were not both successful",
    );

    let process_recipes = load_process_recipes(material_catalog);
    let process_step_ids = [&completed[3].0, &completed[5].0, &completed[6].0];
    let expected_args = [
        ["test", "--offline"].as_slice(),
        ["test", "--offline"].as_slice(),
        ["build", "--offline"].as_slice(),
    ];
    require(
        process_recipes.len() == 3
            && process_step_ids
                .iter()
                .zip(expected_args)
                .all(|(step_id, expected)| {
                    process_recipes
                        .get(*step_id)
                        .is_some_and(|actual| actual == expected)
                }),
        "live coding private process recipes did not preserve test/test/build argv order",
    );

    let accepted_plans = snapshot
        .records
        .iter()
        .filter_map(|record| {
            if let RunEventBody::PlanAccepted { steps, .. } = &record.event.body {
                Some(steps)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    require(
        accepted_plans.len() == 7
            && accepted_plans
                .iter()
                .all(|steps| steps.len() == 1 && steps[0].depends_on.is_empty()),
        "live coding Plans were not seven sequential single-Step plans",
    );
    for (actual, expected, message) in [
        (
            count_events(&snapshot.records, |body| {
                matches!(body, RunEventBody::ModelCallReserved { .. })
            }),
            8,
            "live coding reservation count was not eight",
        ),
        (
            count_events(&snapshot.records, |body| {
                matches!(body, RunEventBody::EffectExecutionStarted { .. })
            }),
            7,
            "live coding effect start count was not seven",
        ),
        (
            count_events(&snapshot.records, |body| {
                matches!(
                    body,
                    RunEventBody::VerificationRecorded {
                        disposition: VerificationDisposition::Passed,
                        ..
                    }
                )
            }),
            7,
            "live coding passed verification count was not seven",
        ),
        (
            count_events(&snapshot.records, |body| {
                matches!(body, RunEventBody::CompletionCandidateRecorded { .. })
            }),
            1,
            "live coding completion count was not one",
        ),
    ] {
        require(actual == expected, message);
    }
    require(
        count_events(&snapshot.records, |body| {
            matches!(
                body,
                RunEventBody::ModelCallBecameUnknown { .. }
                    | RunEventBody::ModelCallSettled { .. }
                    | RunEventBody::StepPlanned { .. }
                    | RunEventBody::InvocationMaterialUnavailable { .. }
                    | RunEventBody::EffectFailed { .. }
                    | RunEventBody::EffectBecameUnknown { .. }
                    | RunEventBody::ReconciliationStarted { .. }
                    | RunEventBody::ReconciliationResolved { .. }
                    | RunEventBody::ManualInterventionRequired { .. }
                    | RunEventBody::VerificationPassed { .. }
                    | RunEventBody::VerificationFailed { .. }
                    | RunEventBody::VerificationRecorded {
                        disposition: VerificationDisposition::Failed
                            | VerificationDisposition::Inconclusive,
                        ..
                    }
            )
        }) == 0,
        "live coding journal contained a failure, uncertainty, or legacy transition",
    );
}

fn load_process_recipes(material_catalog: &Path) -> BTreeMap<String, Vec<String>> {
    let connection = rusqlite::Connection::open_with_flags(
        material_catalog,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap_or_else(|_| panic!("live coding material catalog could not open read-only"));
    let mut statement = connection
        .prepare("SELECT record FROM material_recipe")
        .unwrap_or_else(|_| panic!("live coding material records could not be queried"));
    let rows = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap_or_else(|_| panic!("live coding material query could not execute"));
    let mut process = BTreeMap::new();
    for row in rows {
        let bytes = row.unwrap_or_else(|_| panic!("live coding material row could not load"));
        let record: serde_json::Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| panic!("live coding material row was not valid JSON"));
        if record["domain"] != "xgeny.cli.process-recipe/v1" {
            continue;
        }
        let step_id = record["stepId"]
            .as_str()
            .unwrap_or_else(|| panic!("live coding process recipe Step was missing"))
            .to_owned();
        let args = record["arguments"]["args"]
            .as_array()
            .unwrap_or_else(|| panic!("live coding process recipe argv was missing"))
            .iter()
            .map(|argument| {
                argument
                    .as_str()
                    .unwrap_or_else(|| panic!("live coding process argv was not text"))
                    .to_owned()
            })
            .collect::<Vec<_>>();
        require(
            process.insert(step_id, args).is_none(),
            "live coding process recipe Step was duplicated",
        );
    }
    process
}

#[allow(clippy::too_many_lines)] // Keep every redacted live invariant explicit in one verifier.
fn verify_durable_workspace_result(
    store: &SqliteRunStore,
    snapshot: &xgeny_local_store::RunSnapshot,
    target_relative: &str,
    locator: &str,
    sentinel: &str,
    source: &str,
) {
    let calls = snapshot
        .state
        .agent_loop
        .as_ref()
        .and_then(|agent| agent.model_calls.as_ref())
        .unwrap_or_else(|| panic!("live workspace model-call lifecycle was missing"));
    require(
        (2..=8).contains(&calls.reserved_calls)
            && calls.reserved_calls == calls.settled_calls
            && calls.unknown_calls == 0
            && calls.active_call.is_none(),
        "live workspace model-call lifecycle was not fully settled and bounded",
    );

    let step_count = snapshot.state.steps.len();
    require(
        (4..=8).contains(&step_count)
            && snapshot.state.steps.values().all(|step| {
                step.status == StepStatus::Completed
                    && step.attempts == 1
                    && step.execution_receipt_id.is_some()
                    && step.execution_receipt_digest.is_some()
            }),
        "live workspace Steps were not bounded, once-executed, and receipt-completed",
    );
    let outputs = snapshot
        .state
        .steps
        .values()
        .map(|step| {
            let intent = step
                .intent
                .as_ref()
                .unwrap_or_else(|| panic!("live workspace effect intent was missing"));
            store
                .load_tool_output(&intent.effect_id)
                .unwrap_or_else(|_| panic!("live workspace ToolOutput could not load"))
                .unwrap_or_else(|| panic!("live workspace ToolOutput was missing"))
        })
        .collect::<Vec<ToolOutputRecord>>();
    require(
        outputs.len() == step_count
            && store
                .load_execution_receipts()
                .unwrap_or_else(|_| panic!("live workspace Receipts could not load"))
                .len()
                == step_count,
        "live workspace output and Receipt cardinality did not match completed Steps",
    );

    let observed_capabilities = outputs
        .iter()
        .map(ToolOutputRecord::capability_id)
        .collect::<BTreeSet<_>>();
    let expected_capabilities = BTreeSet::from([
        "xgeny.fs/list-directory",
        "xgeny.fs/read-text",
        "xgeny.fs/search-text",
        "xgeny.fs/stat",
    ]);
    require(
        observed_capabilities == expected_capabilities,
        "live workspace did not execute every required discovery capability",
    );
    require(
        outputs.iter().any(|output| {
            output.capability_id() == "xgeny.fs/list-directory"
                && output.output()["entries"]
                    .as_array()
                    .is_some_and(|entries| {
                        entries.iter().any(|entry| {
                            entry["path"].as_str() == Some("knowledge")
                                && entry["kind"].as_str() == Some("directory")
                        })
                    })
        }),
        "live workspace list observation did not contain the target directory",
    );
    require(
        outputs.iter().any(|output| {
            output.capability_id() == "xgeny.fs/search-text"
                && output.output()["matches"]
                    .as_array()
                    .is_some_and(|matches| {
                        matches.iter().any(|candidate| {
                            candidate["path"].as_str() == Some(target_relative)
                                && candidate["preview"].as_str() == Some(locator)
                                && !candidate["preview"]
                                    .as_str()
                                    .is_some_and(|preview| preview.contains(sentinel))
                        })
                    })
        }),
        "live workspace search observation did not locate the target",
    );
    let source_size = u64::try_from(source.len())
        .unwrap_or_else(|_| panic!("live workspace source size did not fit u64"));
    require(
        outputs.iter().any(|output| {
            output.capability_id() == "xgeny.fs/stat"
                && output.output()["kind"].as_str() == Some("file")
                && output.output()["sizeBytes"].as_u64() == Some(source_size)
        }),
        "live workspace stat observation did not match the target file",
    );
    require(
        outputs.iter().any(|output| {
            output.capability_id() == "xgeny.fs/read-text"
                && output.output()["content"].as_str() == Some(source)
        }),
        "live workspace read observation did not contain the exact target source",
    );

    let model_calls = usize::try_from(calls.reserved_calls)
        .unwrap_or_else(|_| panic!("live workspace model-call count did not fit usize"));
    let accepted_plans = snapshot
        .records
        .iter()
        .filter_map(|record| {
            if let RunEventBody::PlanAccepted { steps, .. } = &record.event.body {
                Some(steps)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let plan_count = accepted_plans.len();
    require(
        plan_count == step_count
            && accepted_plans
                .iter()
                .all(|steps| steps.len() == 1 && steps[0].depends_on.is_empty()),
        "live workspace accepted Plans were not strictly sequential single-Step plans",
    );
    require(
        plan_count + 1 == model_calls,
        "live workspace model turns did not end in exactly one completion",
    );
    for (actual, expected, message) in [
        (
            count_events(&snapshot.records, |body| {
                matches!(body, RunEventBody::ModelCallReserved { .. })
            }),
            model_calls,
            "live workspace model-call reservation count was inconsistent",
        ),
        (
            count_events(&snapshot.records, |body| {
                matches!(body, RunEventBody::EffectIntentCommitted { .. })
            }),
            step_count,
            "live workspace effect intent count was inconsistent",
        ),
        (
            count_events(&snapshot.records, |body| {
                matches!(body, RunEventBody::EffectExecutionStarted { .. })
            }),
            step_count,
            "live workspace effect start count was inconsistent",
        ),
        (
            count_events(&snapshot.records, |body| {
                matches!(body, RunEventBody::EffectSucceeded { .. })
            }),
            step_count,
            "live workspace effect success count was inconsistent",
        ),
        (
            count_events(&snapshot.records, |body| {
                matches!(
                    body,
                    RunEventBody::VerificationRecorded {
                        disposition: VerificationDisposition::Passed,
                        ..
                    }
                )
            }),
            step_count,
            "live workspace passed verification count was inconsistent",
        ),
        (
            count_events(&snapshot.records, |body| {
                matches!(body, RunEventBody::CompletionCandidateRecorded { .. })
            }),
            1,
            "live workspace completion count was inconsistent",
        ),
    ] {
        require(actual == expected, message);
    }
    require(
        count_events(&snapshot.records, |body| {
            matches!(
                body,
                RunEventBody::ModelCallBecameUnknown { .. }
                    | RunEventBody::ModelCallSettled { .. }
                    | RunEventBody::StepPlanned { .. }
                    | RunEventBody::InvocationMaterialUnavailable { .. }
                    | RunEventBody::EffectFailed { .. }
                    | RunEventBody::EffectBecameUnknown { .. }
                    | RunEventBody::ReconciliationStarted { .. }
                    | RunEventBody::ReconciliationResolved { .. }
                    | RunEventBody::ManualInterventionRequired { .. }
                    | RunEventBody::VerificationPassed { .. }
                    | RunEventBody::VerificationFailed { .. }
                    | RunEventBody::VerificationRecorded {
                        disposition: VerificationDisposition::Failed
                            | VerificationDisposition::Inconclusive,
                        ..
                    }
            )
        }) == 0,
        "live workspace journal contained a failure, uncertainty, or legacy transition",
    );
}

fn require_workspace_completion(output: &Output, state_root: &Path) {
    if output.status.code() == Some(0) {
        return;
    }
    if contains_bytes(&output.stderr, b"reason=failed_work")
        || contains_bytes(&output.stderr, b"reason=model_rejected")
    {
        let run_id = extract_run_id(&output.stderr);
        if let Ok(store) =
            SqliteRunStore::open_existing_read_only(run_database(state_root, &run_id))
            && let Ok(Some(snapshot)) = store.load()
        {
            if let Some(capability_id) = snapshot
                .state
                .steps
                .values()
                .find(|step| step.status == StepStatus::Failed)
                .and_then(|step| step.intent.as_ref())
                .map(|intent| intent.invocation.capability_id.as_str())
            {
                require(
                    false,
                    match capability_id {
                        "xgeny.fs/list-directory" => "live workspace list-directory effect failed",
                        "xgeny.fs/search-text" => "live workspace search-text effect failed",
                        "xgeny.fs/stat" => "live workspace stat effect failed",
                        "xgeny.fs/read-text" => "live workspace read-text effect failed",
                        _ => "live workspace unknown capability effect failed",
                    },
                );
            }
            if let Some(reason) = snapshot.records.iter().rev().find_map(|record| {
                if let RunEventBody::ModelCallSettled {
                    settlement: ModelCallSettlement::Rejected { reason },
                    ..
                } = record.event.body
                {
                    Some(reason)
                } else {
                    None
                }
            }) {
                require(
                    false,
                    match reason {
                        ModelCallRejectionReason::PlannerInvalidResponse => {
                            workspace_invalid_response_message(&snapshot)
                        }
                        ModelCallRejectionReason::ProviderLimit => {
                            "live workspace provider response exceeded a limit"
                        }
                        ModelCallRejectionReason::ProviderRejected => {
                            "live workspace provider rejected the request"
                        }
                        ModelCallRejectionReason::ProposalRejected => {
                            "live workspace proposal failed runtime validation"
                        }
                        ModelCallRejectionReason::MaterializationFailed => {
                            "live workspace proposal materialization failed"
                        }
                        ModelCallRejectionReason::StaleHead => {
                            "live workspace model response was stale"
                        }
                    },
                );
            }
        }
    }
    require_exit(output, 0, "workspace discovery");
}

fn require_coding_completion(output: &Output, state_root: &Path) {
    if output.status.code() == Some(0) {
        return;
    }
    if [
        b"reason=proposal_rejected".as_slice(),
        b"reason=model_rejected".as_slice(),
        b"reason=material_rejected".as_slice(),
        b"reason=admission_rejected".as_slice(),
        b"reason=failed_work".as_slice(),
    ]
    .iter()
    .any(|marker| contains_bytes(&output.stderr, marker))
    {
        let run_id = extract_run_id(&output.stderr);
        if let Ok(store) =
            SqliteRunStore::open_existing_read_only(run_database(state_root, &run_id))
            && let Ok(Some(snapshot)) = store.load()
        {
            require(false, coding_rejection_progress_message(&snapshot));
        }
    }
    require_exit(output, 0, "qwen coding loop");
}

fn coding_rejection_progress_message(snapshot: &xgeny_local_store::RunSnapshot) -> &'static str {
    let completed = snapshot
        .records
        .iter()
        .filter_map(|record| {
            let RunEventBody::EffectSucceeded { step_id, .. } = &record.event.body else {
                return None;
            };
            snapshot
                .state
                .steps
                .get(step_id)
                .and_then(|step| step.intent.as_ref())
                .map(|intent| intent.invocation.capability_id.as_str())
        })
        .collect::<Vec<_>>();
    coding_rejection_progress_for(&completed)
}

fn coding_goal_discloses_acceptance(goal: &str, locator: &str) -> bool {
    let without_locator = goal.replace(locator, "<locator>");
    without_locator.contains("42") || without_locator.contains("acceptance.rs")
}

fn coding_goal(locator: &str) -> String {
    format!(
        "Complete this controlled Rust coding task using exactly seven tool Steps in this order. Return exactly one concrete tool Step in each planning response, with no dependencies, and do not include a later Step until the predecessor's durable ToolOutput is visible. (1) Call xgeny.fs/search-text from '.' with the literal query '{locator}' to locate the source. (2) Call xgeny.fs/read-text on that matching source file. Do not list or read tests and do not guess their expectation. (3) Call xgeny.fs/apply-patch with the read digest to change only the returned value 40 to the deliberately incomplete value 41. (4) Call xgeny.process/execute with executable 'cargo', args [\"test\",\"--offline\"], cwd '.', empty env, timeoutMs 120000, and maxOutputBytes 32768. This test must be allowed to fail and its exact durable output is the only authority for the next correction. (5) After observing that failure, call xgeny.fs/apply-patch with the current digest to change only 41 to the value required by the failed assertion. (6) Call the same cargo test command again and require success=true. (7) Call xgeny.process/execute with executable 'cargo', args [\"build\",\"--offline\"], cwd '.', empty env, timeoutMs 120000, and maxOutputBytes 32768 and require success=true. Only after all seven Steps finish in that order, return exactly '{CODING_COMPLETION}' as the summary, with no prefix, suffix, Markdown, quotation marks, or explanation."
    )
}

#[test]
fn coding_goal_acceptance_check_ignores_random_locator_content_only() {
    let locator = "XGENY_RC3_CODING_TARGET_42abcdef";
    assert!(!coding_goal_discloses_acceptance(
        &format!("search for '{locator}', then fix from durable test output"),
        locator,
    ));
    assert!(coding_goal_discloses_acceptance(
        &format!("search for '{locator}', then change the value to 42"),
        locator,
    ));
    assert!(coding_goal_discloses_acceptance(
        &format!("search for '{locator}', then read acceptance.rs"),
        locator,
    ));
}

#[test]
fn coding_goal_requires_one_observation_bound_step_per_turn() {
    let locator = "XGENY_RC3_CODING_TARGET_abcdef";
    let goal = coding_goal(locator);
    assert!(goal.contains("exactly one concrete tool Step in each planning response"));
    assert!(goal.contains("with no dependencies"));
    assert!(goal.contains("predecessor's durable ToolOutput is visible"));
    assert!(!coding_goal_discloses_acceptance(&goal, locator));
}

fn coding_rejection_progress_for(completed: &[&str]) -> &'static str {
    let expected = [
        "xgeny.fs/search-text",
        "xgeny.fs/read-text",
        "xgeny.fs/apply-patch",
        "xgeny.process/execute",
        "xgeny.fs/apply-patch",
        "xgeny.process/execute",
        "xgeny.process/execute",
    ];
    if completed.len() > expected.len()
        || !completed
            .iter()
            .zip(expected)
            .all(|(actual, expected)| *actual == expected)
    {
        return "live coding rejection followed an unexpected capability sequence";
    }
    match completed.len() {
        0 => "live coding proposal was rejected before search completed",
        1 => "live coding proposal was rejected after search",
        2 => "live coding proposal was rejected after source read",
        3 => "live coding proposal was rejected after the first patch",
        4 => "live coding proposal was rejected after the failing test",
        5 => "live coding proposal was rejected after the corrective patch",
        6 => "live coding proposal was rejected after the successful re-test",
        7 => "live coding completion was rejected after the successful build",
        _ => "live coding proposal rejection progress was invalid",
    }
}

#[test]
fn coding_rejection_diagnostic_is_stage_specific_and_content_free() {
    let expected = [
        "xgeny.fs/search-text",
        "xgeny.fs/read-text",
        "xgeny.fs/apply-patch",
        "xgeny.process/execute",
        "xgeny.fs/apply-patch",
        "xgeny.process/execute",
        "xgeny.process/execute",
    ];
    for completed in 0..=expected.len() {
        let message = coding_rejection_progress_for(&expected[..completed]);
        assert!(message.starts_with("live coding"));
        assert!(!message.contains(CODING_SEARCH_KEY));
        assert!(!message.contains(CODING_COMPLETION));
    }
    assert_eq!(
        coding_rejection_progress_for(&["xgeny.fs/read-text"]),
        "live coding rejection followed an unexpected capability sequence"
    );
}

fn workspace_invalid_response_message(snapshot: &xgeny_local_store::RunSnapshot) -> &'static str {
    let completed = snapshot
        .state
        .steps
        .values()
        .filter(|step| step.status == StepStatus::Completed)
        .filter_map(|step| step.intent.as_ref())
        .map(|intent| intent.invocation.capability_id.as_str())
        .collect::<BTreeSet<_>>();
    match (
        completed.contains("xgeny.fs/list-directory"),
        completed.contains("xgeny.fs/search-text"),
        completed.contains("xgeny.fs/stat"),
        completed.contains("xgeny.fs/read-text"),
    ) {
        (false, false, false, false) => {
            "live workspace planner returned an invalid initial response"
        }
        (true, false, false, false) => {
            "live workspace planner returned an invalid response after list"
        }
        (true, true, false, false) => {
            "live workspace planner returned an invalid response after search"
        }
        (true, true, true, false) => {
            "live workspace planner returned an invalid response after stat"
        }
        (true, true, true, true) => {
            "live workspace planner returned an invalid completion response"
        }
        _ => "live workspace planner returned an invalid response after unexpected progress",
    }
}

fn verify_durable_live_result(snapshot: &xgeny_local_store::RunSnapshot, receipt_count: usize) {
    let calls = snapshot
        .state
        .agent_loop
        .as_ref()
        .and_then(|agent| agent.model_calls.as_ref())
        .unwrap_or_else(|| panic!("live model-call lifecycle was missing"));
    require(
        calls.reserved_calls == 2
            && calls.settled_calls == 2
            && calls.unknown_calls == 0
            && calls.active_call.is_none(),
        "live model-call lifecycle counters were not exactly 2/2/0",
    );
    verify_live_event_counts(&snapshot.records);
    require(
        snapshot.state.steps.len() == 1
            && snapshot.state.steps.values().all(|step| {
                step.status == StepStatus::Completed
                    && step.attempts == 1
                    && step.execution_receipt_id.is_some()
                    && step.execution_receipt_digest.is_some()
            }),
        "live result must contain exactly one once-executed receipt-completed Step",
    );
    require(receipt_count == 1, "live result must contain one Receipt");
}

fn verify_live_event_counts(records: &[xgeny_workgraph::EventRecord]) {
    require(
        count_events(records, |body| {
            matches!(body, RunEventBody::ModelCallReserved { .. })
        }) == 2,
        "live journal must contain two model-call reservations",
    );
    require(
        count_events(records, |body| {
            matches!(body, RunEventBody::ModelCallBecameUnknown { .. })
        }) == 0,
        "live journal must not contain an unknown model call",
    );
    require(
        count_events(records, |body| {
            matches!(body, RunEventBody::ModelCallSettled { .. })
        }) == 0,
        "live journal must not contain a rejected or abandoned model call",
    );
    require(
        count_events(records, |body| {
            matches!(body, RunEventBody::PlanAccepted { .. })
        }) == 1,
        "live journal must contain one accepted Plan",
    );
    require(
        count_events(records, |body| {
            matches!(body, RunEventBody::StepPlanned { .. })
        }) == 0,
        "live journal must not use the legacy StepPlanned path",
    );
    require(
        count_events(records, |body| {
            matches!(body, RunEventBody::EffectIntentCommitted { .. })
        }) == 1,
        "live journal must contain one effect intent",
    );
    require(
        count_events(records, |body| {
            matches!(body, RunEventBody::EffectExecutionStarted { .. })
        }) == 1,
        "live journal must contain one effect start",
    );
    require(
        count_events(records, |body| {
            matches!(body, RunEventBody::EffectSucceeded { .. })
        }) == 1,
        "live journal must contain one successful file effect",
    );
    require(
        count_events(records, |body| {
            matches!(
                body,
                RunEventBody::VerificationRecorded {
                    disposition: VerificationDisposition::Passed,
                    ..
                }
            )
        }) == 1,
        "live journal must contain one receipt-bound passed verification",
    );
    require(
        count_events(records, |body| {
            matches!(body, RunEventBody::CompletionCandidateRecorded { .. })
        }) == 1,
        "live journal must contain one completion candidate",
    );
    require(
        count_events(records, |body| {
            matches!(
                body,
                RunEventBody::InvocationMaterialUnavailable { .. }
                    | RunEventBody::EffectFailed { .. }
                    | RunEventBody::EffectBecameUnknown { .. }
                    | RunEventBody::ReconciliationStarted { .. }
                    | RunEventBody::ReconciliationResolved { .. }
                    | RunEventBody::ManualInterventionRequired { .. }
                    | RunEventBody::VerificationPassed { .. }
                    | RunEventBody::VerificationFailed { .. }
                    | RunEventBody::VerificationRecorded {
                        disposition: VerificationDisposition::Failed
                            | VerificationDisposition::Inconclusive,
                        ..
                    }
            )
        }) == 0,
        "live journal must not contain an effect failure, uncertainty, or reconciliation",
    );
}

fn count_events(
    records: &[xgeny_workgraph::EventRecord],
    predicate: impl Fn(&RunEventBody) -> bool,
) -> usize {
    records
        .iter()
        .filter(|record| predicate(&record.event.body))
        .count()
}

fn xgeny(state_root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xgeny"));
    command
        .env("XGENY_STATE_HOME", state_root)
        .env_remove("XGENY_OPENAI_API_KEY")
        .env_remove("XGENY_LIVE_CONFIRM")
        .env_remove("XGENY_LIVE_KNOWN_HOSTS_FILE")
        .env_remove("XGENY_LIVE_OPENAI_BASE_URL")
        .env_remove("XGENY_LIVE_CARGO_PATH")
        .env_remove("XGENY_OPENAI_BASE_URL")
        .env_remove("XGENY_OPENAI_MODEL")
        .env_remove("XGENY_OPENAI_TOKENIZER");
    command
}

fn run_redacted(command: &mut Command, stage: &'static str) -> Output {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|_| panic!("live XGENy child could not start at {stage}"));
    let stdout_reader = drain_pipe(
        child
            .stdout
            .take()
            .unwrap_or_else(|| panic!("live stdout pipe was missing at {stage}")),
    );
    let stderr_reader = drain_pipe(
        child
            .stderr
            .take()
            .unwrap_or_else(|| panic!("live stderr pipe was missing at {stage}")),
    );
    let deadline = Instant::now() + XGENY_PROCESS_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                terminate_child(&mut child);
                discard_reader(stdout_reader);
                discard_reader(stderr_reader);
                panic!(
                    "live XGENy child status failed at {stage}: {:?}",
                    error.kind()
                );
            }
        }
        if Instant::now() >= deadline {
            terminate_child(&mut child);
            discard_reader(stdout_reader);
            discard_reader(stderr_reader);
            panic!("live XGENy child timed out at {stage}");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let stdout = finish_reader(stdout_reader, stage, "stdout");
    let stderr = finish_reader(stderr_reader, stage, "stderr");
    Output {
        status,
        stdout,
        stderr,
    }
}

fn drain_pipe(mut pipe: impl io::Read + Send + 'static) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn finish_reader(
    reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    stage: &'static str,
    stream: &'static str,
) -> Vec<u8> {
    match reader.join() {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => panic!(
            "live {stream} could not be read at {stage}: {:?}",
            error.kind()
        ),
        Err(panic_payload) => {
            drop(panic_payload);
            panic!("live {stream} reader failed at {stage}");
        }
    }
}

fn discard_reader(reader: thread::JoinHandle<io::Result<Vec<u8>>>) {
    let _ = reader.join();
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn require_exit(output: &Output, expected: i32, stage: &'static str) {
    require(
        output.status.code() == Some(expected),
        classified_exit_message(output, stage),
    );
}

fn classified_exit_message(output: &Output, stage: &'static str) -> &'static str {
    for (marker, message) in [
        (
            b"reason=proposal_rejected".as_slice(),
            "live child proposal was rejected",
        ),
        (
            b"reason=model_rejected".as_slice(),
            "live child model response was rejected",
        ),
        (
            b"reason=material_rejected".as_slice(),
            "live child material was rejected",
        ),
        (
            b"reason=admission_rejected".as_slice(),
            "live child admission was rejected",
        ),
        (
            b"reason=failed_work".as_slice(),
            "live child reported failed work",
        ),
        (
            b"reason=model_call_unknown".as_slice(),
            "live child model call became unknown",
        ),
        (
            b"reason=effect_outcome_unknown".as_slice(),
            "live child effect outcome became unknown",
        ),
        (
            b"reason=tick_budget_exhausted".as_slice(),
            "live child exhausted its tick budget",
        ),
        (
            b"reason=read_approval_required".as_slice(),
            "live child unexpectedly required read approval",
        ),
        (
            b"reason=remote_model_egress_consent_required".as_slice(),
            "live child unexpectedly required model egress consent",
        ),
        (
            b"XGENY_ERROR code=configuration_mismatch".as_slice(),
            "live child configuration did not match",
        ),
        (
            b"XGENY_ERROR code=run_integrity_failure".as_slice(),
            "live child Run integrity verification failed",
        ),
        (
            b"XGENY_ERROR code=internal_safety_failure".as_slice(),
            "live child internal safety check failed",
        ),
    ] {
        if contains_bytes(&output.stderr, marker) {
            return message;
        }
    }
    match stage {
        "first model turn" => "first model turn returned an unclassified exit status",
        "local read turn" => "local read turn returned an unclassified exit status",
        "second model turn" => "second model turn returned an unclassified exit status",
        "offline replay" => "offline replay returned an unclassified exit status",
        "workspace discovery" => "workspace discovery returned an unclassified exit status",
        "qwen coding loop" => "qwen coding loop returned an unclassified exit status",
        "workspace offline replay" => {
            "workspace offline replay returned an unclassified exit status"
        }
        "coding offline replay" => "coding offline replay returned an unclassified exit status",
        _ => "live child returned an unclassified exit status",
    }
}

fn extract_run_id(stderr: &[u8]) -> String {
    let text =
        std::str::from_utf8(stderr).unwrap_or_else(|_| panic!("live status output was not UTF-8"));
    let run_id = text
        .split_whitespace()
        .find_map(|field| field.strip_prefix("run_id="))
        .unwrap_or_else(|| panic!("live status output did not contain a Run ID"));
    require(
        run_id.len() == 36
            && run_id.starts_with("run-")
            && run_id[4..].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "live status output contained an invalid Run ID",
    );
    run_id.to_owned()
}

fn run_database(state_root: &Path, run_id: &str) -> PathBuf {
    state_root.join("runs").join(run_id).join("run.sqlite3")
}

fn require_live_confirmation(expected: &str) {
    let confirmation = required_env("XGENY_LIVE_CONFIRM");
    require(
        confirmation == expected,
        "live confirmation value was not exact",
    );
}

fn snapshot_known_hosts(fixture: &Path) -> PathBuf {
    let source_path = PathBuf::from(required_env("XGENY_LIVE_KNOWN_HOSTS_FILE"));
    let source = fs::File::open(source_path)
        .unwrap_or_else(|_| panic!("trusted live known_hosts file could not be opened"));
    let metadata = source
        .metadata()
        .unwrap_or_else(|_| panic!("trusted live known_hosts metadata could not be read"));
    require(
        metadata.is_file(),
        "trusted live known_hosts path must be a file",
    );
    require(
        metadata.len() <= MAX_KNOWN_HOSTS_BYTES,
        "trusted live known_hosts file exceeded the size limit",
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        require(
            metadata.permissions().mode() & 0o022 == 0,
            "trusted live known_hosts file must not be group- or other-writable",
        );
    }

    let mut bytes = Vec::new();
    source
        .take(MAX_KNOWN_HOSTS_BYTES + 1)
        .read_to_end(&mut bytes)
        .unwrap_or_else(|_| panic!("trusted live known_hosts file could not be read"));
    require(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_KNOWN_HOSTS_BYTES,
        "trusted live known_hosts snapshot was empty or oversized",
    );

    let snapshot_path = fixture.join("known_hosts");
    require_safe_ssh_option_path(&snapshot_path);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut snapshot = options
        .open(&snapshot_path)
        .unwrap_or_else(|_| panic!("trusted live known_hosts snapshot could not be created"));
    snapshot
        .write_all(&bytes)
        .unwrap_or_else(|_| panic!("trusted live known_hosts snapshot could not be written"));
    snapshot
        .sync_all()
        .unwrap_or_else(|_| panic!("trusted live known_hosts snapshot could not be synced"));
    drop(snapshot);
    snapshot_path
}

fn require_safe_ssh_option_path(path: &Path) {
    let text = path_text(path);
    require(
        text.starts_with('/')
            && text.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-')
            }),
        "live temporary directory is not safe for an SSH option path",
    );
}

fn required_env(name: &'static str) -> String {
    env::var(name).unwrap_or_else(|_| panic!("required live environment is missing: {name}"))
}

fn loopback_address(base_url: &str) -> SocketAddr {
    let authority = base_url
        .strip_prefix("http://")
        .and_then(|value| value.strip_suffix("/v1"))
        .unwrap_or_else(|| panic!("live base URL must be an exact loopback HTTP /v1 URL"));
    require(
        !authority.contains('/'),
        "live base URL must not contain another path",
    );
    let address: SocketAddr = authority
        .parse()
        .unwrap_or_else(|_| panic!("live base URL authority was invalid"));
    require(
        address.ip() == Ipv4Addr::LOCALHOST && address.port() != 0,
        "live base URL must use literal 127.0.0.1 and a nonzero port",
    );
    address
}

fn random_hex() -> String {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .unwrap_or_else(|_| panic!("live random fixture identity could not be generated"));
    let mut encoded = String::with_capacity(random.len() * 2);
    for byte in random {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn require_contains(haystack: &[u8], needle: &[u8], message: &'static str) {
    require(contains_bytes(haystack, needle), message);
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn require_output_absent(output: &Output, forbidden: &[u8]) {
    require(
        !contains_bytes(&output.stdout, forbidden) && !contains_bytes(&output.stderr, forbidden),
        "live process output retained a forbidden runtime value",
    );
}

fn require_run_directory_absent(directory: &Path, forbidden: &[u8]) {
    let entries = fs::read_dir(directory)
        .unwrap_or_else(|_| panic!("live Run directory could not be inspected"));
    for entry in entries {
        let entry = entry.unwrap_or_else(|_| panic!("live Run entry could not be inspected"));
        let file_type = entry
            .file_type()
            .unwrap_or_else(|_| panic!("live Run entry type could not be inspected"));
        if file_type.is_file() {
            let bytes = fs::read(entry.path())
                .unwrap_or_else(|_| panic!("live Run file could not be inspected"));
            require(
                !contains_bytes(&bytes, forbidden),
                "live Run state retained a forbidden runtime value",
            );
        }
    }
}

fn path_text(path: &Path) -> &str {
    path.to_str()
        .unwrap_or_else(|| panic!("live temporary path was not UTF-8"))
}

fn require(condition: bool, message: &'static str) {
    assert!(condition, "{message}");
}
