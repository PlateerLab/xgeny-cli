use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use process_wrap::std::CommandWrap;
#[cfg(windows)]
use process_wrap::std::JobObject;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use xgeny_domain::InstanceFeatures;
use xgeny_runtime::{
    AdapterEvidenceDigest, AdapterExecutionObservation, AdapterExecutionUnknownReason,
    AdapterPrepareFailure, AdapterToolOutput, PreparedAdapterInvocation,
};

use crate::catalog::{parse_resource, validate_environment_pair};
use crate::{ProcessWorkspace, path::resolve_cwd};

pub const MIN_CAPTURE_BYTES: usize = 1_024;
pub const MAX_CAPTURE_BYTES: usize = 32 * 1_024;
pub const MAX_PROCESS_TIMEOUT_MS: u64 = 600_000;
const MIN_PROCESS_TIMEOUT_MS: u64 = 100;
const MAX_ARGS: usize = 128;
const MAX_ARG_BYTES: usize = 4_096;
const MAX_TOTAL_ARG_BYTES: usize = 32 * 1_024;
const MAX_MODEL_ENV_ENTRIES: usize = 64;
const MAX_MODEL_ENV_BYTES: usize = 32 * 1_024;
pub(crate) const MAX_RESULT_DURATION_MS: u64 = 660_000;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const CAPTURE_COMPLETION_GRACE: Duration = Duration::from_secs(1);

pub(crate) const fn supports_instance_features(features: &InstanceFeatures) -> bool {
    features.sync && !features.task && !features.cancellable && !features.idempotency_query
}

pub(crate) fn parse_prepared(
    arguments: &Value,
    workspace: &ProcessWorkspace,
) -> Result<Box<dyn PreparedAdapterInvocation>, AdapterPrepareFailure> {
    Ok(Box::new(parse_arguments(arguments, workspace)?))
}

pub(crate) fn accepts_normalized_material(arguments: &Value, workspace: &ProcessWorkspace) -> bool {
    parse_arguments(arguments, workspace).is_ok()
}

fn parse_arguments(
    arguments: &Value,
    workspace: &ProcessWorkspace,
) -> Result<PreparedProcess, AdapterPrepareFailure> {
    let object = arguments
        .as_object()
        .filter(|object| object.len() == 6)
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    let executable_resource = object
        .get("executable")
        .and_then(Value::as_str)
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    let executable_id = parse_resource(&workspace.workspace_id, executable_resource)
        .map_err(|_| AdapterPrepareFailure::InvalidMaterial)?;
    let executable = workspace
        .catalog
        .entry(executable_id)
        .ok_or(AdapterPrepareFailure::ResourceUnavailable)?
        .verified_path()
        .map_err(|_| AdapterPrepareFailure::ResourceUnavailable)?;
    let args = parse_args(object.get("args"))?;
    let cwd = object
        .get("cwd")
        .and_then(Value::as_str)
        .ok_or(AdapterPrepareFailure::InvalidMaterial)
        .and_then(|cwd| resolve_cwd(workspace, cwd))?;
    let mut environment = workspace.environment.values().clone();
    merge_model_environment(&mut environment, object.get("env"))?;
    let timeout_ms = object
        .get("timeoutMs")
        .and_then(Value::as_u64)
        .filter(|value| (MIN_PROCESS_TIMEOUT_MS..=MAX_PROCESS_TIMEOUT_MS).contains(value))
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    let max_output_bytes = object
        .get("maxOutputBytes")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| (MIN_CAPTURE_BYTES..=MAX_CAPTURE_BYTES).contains(value))
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    Ok(PreparedProcess {
        executable,
        args,
        cwd,
        environment,
        timeout: Duration::from_millis(timeout_ms),
        max_output_bytes,
    })
}

fn parse_args(value: Option<&Value>) -> Result<Vec<String>, AdapterPrepareFailure> {
    let args = value
        .and_then(Value::as_array)
        .filter(|args| args.len() <= MAX_ARGS)
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    let mut parsed = Vec::with_capacity(args.len());
    let mut total = 0_usize;
    for argument in args {
        let argument = argument
            .as_str()
            .filter(|argument| argument.len() <= MAX_ARG_BYTES && !argument.contains('\0'))
            .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
        total = total
            .checked_add(argument.len())
            .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
        if total > MAX_TOTAL_ARG_BYTES {
            return Err(AdapterPrepareFailure::InvalidMaterial);
        }
        parsed.push(argument.to_owned());
    }
    Ok(parsed)
}

fn merge_model_environment(
    environment: &mut BTreeMap<String, String>,
    value: Option<&Value>,
) -> Result<(), AdapterPrepareFailure> {
    let model = value
        .and_then(Value::as_object)
        .filter(|model| model.len() <= MAX_MODEL_ENV_ENTRIES)
        .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
    let mut total = 0_usize;
    for (key, value) in model {
        let value = value
            .as_str()
            .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
        validate_environment_pair(key, value)
            .map_err(|()| AdapterPrepareFailure::InvalidMaterial)?;
        total = total
            .checked_add(key.len())
            .and_then(|total| total.checked_add(value.len()))
            .ok_or(AdapterPrepareFailure::InvalidMaterial)?;
        if total > MAX_MODEL_ENV_BYTES
            || environment
                .keys()
                .any(|existing| existing.eq_ignore_ascii_case(key))
            || protected_environment_key(key)
        {
            return Err(AdapterPrepareFailure::InvalidMaterial);
        }
        environment.insert(key.clone(), value.to_owned());
    }
    Ok(())
}

fn protected_environment_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "PATH"
            | "PATHEXT"
            | "HOME"
            | "USERPROFILE"
            | "SYSTEMROOT"
            | "WINDIR"
            | "COMSPEC"
            | "TMP"
            | "TEMP"
            | "TMPDIR"
            | "CARGO_HOME"
            | "RUSTUP_HOME"
    ) || upper.starts_with("LD_")
        || upper.starts_with("DYLD_")
}

struct PreparedProcess {
    executable: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    environment: BTreeMap<String, String>,
    timeout: Duration,
    max_output_bytes: usize,
}

impl std::fmt::Debug for PreparedProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedProcess")
            .field("executable", &"<redacted>")
            .field("args", &format_args!("<{} redacted>", self.args.len()))
            .field("cwd", &"<redacted>")
            .field(
                "environment",
                &format_args!("<{} redacted>", self.environment.len()),
            )
            .field("timeout", &self.timeout)
            .field("max_output_bytes", &self.max_output_bytes)
            .finish()
    }
}

impl PreparedAdapterInvocation for PreparedProcess {
    fn execute(self: Box<Self>) -> AdapterExecutionObservation {
        match run_process(&self) {
            Ok(output) => {
                let digest = canonical_output_digest(&output)
                    .expect("the adapter constructs a finite JSON process output");
                AdapterExecutionObservation::SucceededWithOutput {
                    evidence_digest: AdapterEvidenceDigest::new(digest)
                        .expect("the internal SHA-256 digest is canonical"),
                    output: AdapterToolOutput::new(output),
                }
            }
            Err(reason) => AdapterExecutionObservation::Unknown { reason },
        }
    }
}

fn run_process(prepared: &PreparedProcess) -> Result<Value, AdapterExecutionUnknownReason> {
    let started = Instant::now();
    let mut command = Command::new(&prepared.executable);
    command
        .args(&prepared.args)
        .current_dir(&prepared.cwd)
        .env_clear()
        .envs(&prepared.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut wrapped = CommandWrap::from(command);
    #[cfg(unix)]
    wrapped.wrap(ProcessGroup::leader());
    #[cfg(windows)]
    wrapped.wrap(JobObject);

    let Ok(mut child) = wrapped.spawn() else {
        drop(wrapped);
        return build_output(
            "launch_failed",
            false,
            None,
            "",
            "",
            false,
            false,
            elapsed_ms(started),
        );
    };
    drop(wrapped);
    let Some(stdout) = child.stdout().take() else {
        let _ = child.kill();
        return Err(AdapterExecutionUnknownReason::AdapterTerminated);
    };
    let Some(stderr) = child.stderr().take() else {
        let _ = child.kill();
        return Err(AdapterExecutionUnknownReason::AdapterTerminated);
    };
    let stdout =
        start_capture(stdout, prepared.max_output_bytes, "xgeny-process-stdout").map_err(|()| {
            let _ = child.kill();
            AdapterExecutionUnknownReason::AdapterTerminated
        })?;
    let stderr =
        start_capture(stderr, prepared.max_output_bytes, "xgeny-process-stderr").map_err(|()| {
            let _ = child.kill();
            AdapterExecutionUnknownReason::AdapterTerminated
        })?;

    let termination = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Background descendants are not part of a one-shot process Capability. Terminate
                // the group/job even when the top-level process exited successfully.
                let _ = child.start_kill();
                let _ = child.wait();
                break Termination::Exited(status);
            }
            Ok(None) if started.elapsed() >= prepared.timeout => match child.kill() {
                Ok(()) => break Termination::TimedOut,
                Err(_) => match child.try_wait() {
                    Ok(Some(status)) => break Termination::Exited(status),
                    _ => return Err(AdapterExecutionUnknownReason::TransportOutcomeUnknown),
                },
            },
            Ok(None) => {
                let remaining = prepared.timeout.saturating_sub(started.elapsed());
                thread::sleep(POLL_INTERVAL.min(remaining));
            }
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait();
                return Err(AdapterExecutionUnknownReason::TransportOutcomeUnknown);
            }
        }
    };
    drop(child);

    let stdout = receive_capture(&stdout)?;
    let stderr = receive_capture(&stderr)?;
    let duration_ms = elapsed_ms(started);
    match termination {
        Termination::Exited(status) => build_output(
            "exited",
            status.success(),
            status.code(),
            &stdout.content,
            &stderr.content,
            stdout.truncated,
            stderr.truncated,
            duration_ms,
        ),
        Termination::TimedOut => build_output(
            "timed_out",
            false,
            None,
            &stdout.content,
            &stderr.content,
            stdout.truncated,
            stderr.truncated,
            duration_ms,
        ),
    }
}

enum Termination {
    Exited(ExitStatus),
    TimedOut,
}

struct CapturedStream {
    content: String,
    truncated: bool,
}

fn start_capture<R>(
    mut reader: R,
    maximum: usize,
    thread_name: &str,
) -> Result<Receiver<std::io::Result<CapturedStream>>, ()>
where
    R: std::io::Read + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name(thread_name.to_owned())
        .spawn(move || {
            let result = capture_reader(&mut reader, maximum);
            let _ = sender.send(result);
        })
        .map_err(|_| ())?;
    Ok(receiver)
}

fn capture_reader(
    reader: &mut impl std::io::Read,
    maximum: usize,
) -> std::io::Result<CapturedStream> {
    let mut retained = Vec::with_capacity(maximum);
    let mut truncated = false;
    let mut buffer = [0_u8; 8 * 1_024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let available = maximum.saturating_sub(retained.len());
        let keep = available.min(count);
        retained.extend_from_slice(&buffer[..keep]);
        truncated |= keep < count;
    }
    let mut content = String::from_utf8_lossy(&retained).into_owned();
    if content.len() > maximum {
        let mut end = maximum;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        content.truncate(end);
        truncated = true;
    }
    Ok(CapturedStream { content, truncated })
}

fn receive_capture(
    receiver: &Receiver<std::io::Result<CapturedStream>>,
) -> Result<CapturedStream, AdapterExecutionUnknownReason> {
    receiver
        .recv_timeout(CAPTURE_COMPLETION_GRACE)
        .map_err(|_| AdapterExecutionUnknownReason::ResponseUnverifiable)?
        .map_err(|_| AdapterExecutionUnknownReason::ResponseUnverifiable)
}

#[allow(clippy::too_many_arguments)]
fn build_output(
    outcome: &str,
    success: bool,
    exit_code: Option<i32>,
    stdout: &str,
    stderr: &str,
    stdout_truncated: bool,
    stderr_truncated: bool,
    duration_ms: u64,
) -> Result<Value, AdapterExecutionUnknownReason> {
    let output = json!({
        "outcome": outcome,
        "success": success,
        "exitCode": exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "stdoutTruncated": stdout_truncated,
        "stderrTruncated": stderr_truncated,
        "durationMs": duration_ms,
    });
    crate::verifier::inspect_output(&output, None)
        .map_err(|()| AdapterExecutionUnknownReason::ResponseUnverifiable)?;
    Ok(output)
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis())
        .unwrap_or(u64::MAX)
        .min(MAX_RESULT_DURATION_MS)
}

pub(crate) fn canonical_output_digest(value: &Value) -> Result<String, serde_json::Error> {
    let bytes = serde_jcs::to_vec(value)?;
    Ok(sha256_digest(&bytes))
}

pub(crate) fn sha256_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    format!("sha256:{encoded}")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;
    use std::sync::OnceLock;

    use serde_json::Map;
    use tempfile::{TempDir, tempdir};
    use xgeny_policy::ResourceResolver as _;

    use super::*;
    use crate::{
        ExecutableCatalog, PROCESS_EXECUTE_SCOPE, ProcessEnvironment, ProcessWorkspace,
        ProcessWorkspaceId,
    };

    static TEST_EXECUTABLE_CATALOG: OnceLock<ExecutableCatalog> = OnceLock::new();

    struct Fixture {
        workspace: ProcessWorkspace,
        directory: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempdir().expect("temporary workspace should exist");
            let catalog = TEST_EXECUTABLE_CATALOG
                .get_or_init(|| {
                    let executable =
                        std::env::current_exe().expect("test executable should resolve");
                    ExecutableCatalog::from_paths([("test-helper", executable)]).unwrap()
                })
                .clone();
            let workspace = ProcessWorkspace::open_ambient(
                directory.path(),
                ProcessWorkspaceId::new("fixture").unwrap(),
                catalog,
                ProcessEnvironment::empty(),
            )
            .unwrap();
            Self {
                workspace,
                directory,
            }
        }

        fn arguments(&self, mode: Option<&str>, maximum: usize, timeout_ms: u64) -> Value {
            let executable = self
                .workspace
                .resolver()
                .resolve(PROCESS_EXECUTE_SCOPE, "test-helper")
                .unwrap();
            let mut env = Map::new();
            if let Some(mode) = mode {
                env.insert(
                    "XGENY_PROCESS_TEST_MODE".to_owned(),
                    Value::String(mode.to_owned()),
                );
            }
            json!({
                "executable": executable,
                "args": ["execution::tests::process_child_helper", "--exact", "--nocapture", "--test-threads=1"],
                "cwd": ".",
                "env": env,
                "timeoutMs": timeout_ms,
                "maxOutputBytes": maximum,
            })
        }

        fn execute(&self, arguments: &Value) -> Value {
            let prepared = parse_arguments(arguments, &self.workspace).unwrap();
            run_process(&prepared).unwrap()
        }
    }

    #[test]
    fn shell_metacharacters_are_one_literal_argument() {
        let fixture = Fixture::new();
        let marker = fixture.directory.path().join("shell-must-not-create-this");
        let mut arguments = fixture.arguments(None, MIN_CAPTURE_BYTES, 5_000);
        let literal = if cfg!(windows) {
            format!("& type nul > {}", marker.display())
        } else {
            format!("; touch {}", marker.display())
        };
        arguments["args"] = json!([literal]);

        let output = fixture.execute(&arguments);
        assert_eq!(output["outcome"], "exited");
        assert!(!marker.exists());
    }

    #[cfg(unix)]
    #[test]
    fn catalogued_symlink_proxy_preserves_argv_zero_alias() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().expect("temporary workspace should exist");
        let alias = directory.path().join("cargo-proxy");
        symlink(std::env::current_exe().unwrap(), &alias).unwrap();
        let catalog = ExecutableCatalog::from_paths([("proxy", &alias)]).unwrap();
        let workspace = ProcessWorkspace::open_ambient(
            directory.path(),
            ProcessWorkspaceId::new("fixture").unwrap(),
            catalog,
            ProcessEnvironment::empty(),
        )
        .unwrap();
        let executable = workspace
            .resolver()
            .resolve(PROCESS_EXECUTE_SCOPE, "proxy")
            .unwrap();
        let arguments = json!({
            "executable": executable,
            "args": ["execution::tests::process_child_helper", "--exact", "--nocapture", "--test-threads=1"],
            "cwd": ".",
            "env": {"XGENY_PROCESS_TEST_MODE": "print-argv-zero"},
            "timeoutMs": 5_000,
            "maxOutputBytes": MIN_CAPTURE_BYTES,
        });

        let output = run_process(&parse_arguments(&arguments, &workspace).unwrap()).unwrap();
        assert_eq!(output["outcome"], "exited");
        assert!(
            output["stdout"]
                .as_str()
                .unwrap()
                .contains("argv-zero=cargo-proxy")
        );
    }

    #[test]
    fn nonzero_exit_and_bounded_streams_are_durable_result_data() {
        let fixture = Fixture::new();
        let exit = fixture.execute(&fixture.arguments(Some("exit-23"), MIN_CAPTURE_BYTES, 5_000));
        assert_eq!(exit["outcome"], "exited");
        assert_eq!(exit["success"], false);
        assert_eq!(exit["exitCode"], 23);

        let output =
            fixture.execute(&fixture.arguments(Some("large-output"), MIN_CAPTURE_BYTES, 5_000));
        assert_eq!(output["outcome"], "exited");
        assert_eq!(output["stdoutTruncated"], true);
        assert_eq!(output["stderrTruncated"], true);
        assert!(output["stdout"].as_str().unwrap().len() <= MIN_CAPTURE_BYTES);
        assert!(output["stderr"].as_str().unwrap().len() <= MIN_CAPTURE_BYTES);
    }

    #[test]
    fn invalid_utf8_replacement_never_expands_past_the_byte_limit() {
        let bytes = vec![0xff; MIN_CAPTURE_BYTES];
        let captured = capture_reader(&mut std::io::Cursor::new(bytes), MIN_CAPTURE_BYTES).unwrap();

        assert!(captured.content.len() <= MIN_CAPTURE_BYTES);
        assert!(captured.truncated);
        assert!(captured.content.is_char_boundary(captured.content.len()));
    }

    #[test]
    fn timeout_terminates_the_descendant_tree() {
        let fixture = Fixture::new();
        let marker = fixture.directory.path().join("escaped-grandchild-marker");
        let mut arguments = fixture.arguments(Some("tree-parent"), MIN_CAPTURE_BYTES, 200);
        arguments["env"]["XGENY_PROCESS_TEST_MARKER"] =
            Value::String(marker.to_string_lossy().into_owned());

        let output = fixture.execute(&arguments);
        assert_eq!(output["outcome"], "timed_out");
        assert_eq!(output["exitCode"], Value::Null);
        thread::sleep(Duration::from_millis(1_200));
        assert!(
            !marker.exists(),
            "a terminated grandchild must not finish later"
        );
    }

    #[test]
    fn normal_leader_exit_also_terminates_the_descendant_tree() {
        let fixture = Fixture::new();
        let marker = fixture
            .directory
            .path()
            .join("normal-exit-grandchild-marker");
        let mut arguments = fixture.arguments(Some("tree-exit-parent"), MIN_CAPTURE_BYTES, 5_000);
        arguments["env"]["XGENY_PROCESS_TEST_MARKER"] =
            Value::String(marker.to_string_lossy().into_owned());

        let output = fixture.execute(&arguments);
        assert_eq!(output["outcome"], "exited");
        assert_eq!(output["success"], true);
        thread::sleep(Duration::from_millis(1_200));
        assert!(
            !marker.exists(),
            "a background descendant must not outlive one-shot execution"
        );
    }

    #[test]
    fn relative_cwd_and_explicit_environment_reach_the_child() {
        let fixture = Fixture::new();
        let nested = fixture.directory.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("cwd-marker"), b"expected cwd").unwrap();
        let mut arguments = fixture.arguments(Some("print-context"), MAX_CAPTURE_BYTES, 5_000);
        arguments["cwd"] = Value::String("nested".to_owned());
        arguments["env"]["XGENY_PROCESS_TEST_VALUE"] = Value::String("EXPLICIT-VALUE".to_owned());

        let output = fixture.execute(&arguments);
        let stdout = output["stdout"].as_str().unwrap();
        assert_eq!(output["success"], true);
        assert!(stdout.contains("EXPLICIT-VALUE"));
        assert!(stdout.contains("cwd-marker=true"));
    }

    #[cfg(unix)]
    #[test]
    fn escaped_stdio_never_blocks_past_the_capture_grace() {
        let fixture = Fixture::new();
        let arguments = fixture.arguments(Some("stdio-escape-parent"), MIN_CAPTURE_BYTES, 5_000);
        let prepared = parse_arguments(&arguments, &fixture.workspace).unwrap();
        let started = Instant::now();

        assert_eq!(
            run_process(&prepared),
            Err(AdapterExecutionUnknownReason::ResponseUnverifiable)
        );
        assert!(started.elapsed() < Duration::from_millis(2_500));
    }

    #[test]
    fn prepared_debug_redacts_command_material_and_rejects_protected_env() {
        let fixture = Fixture::new();
        let mut arguments =
            fixture.arguments(Some("SECRET-MODE-SENTINEL"), MIN_CAPTURE_BYTES, 5_000);
        arguments["args"] = json!(["ARGUMENT-SENTINEL"]);
        let prepared = parse_arguments(&arguments, &fixture.workspace).unwrap();
        let debug = format!("{prepared:?}");
        assert!(!debug.contains("SECRET-MODE-SENTINEL"));
        assert!(!debug.contains("ARGUMENT-SENTINEL"));
        assert!(!debug.contains(&fixture.directory.path().to_string_lossy().into_owned()));

        arguments["env"]["PATH"] = Value::String("MODEL-PATH".to_owned());
        assert!(parse_arguments(&arguments, &fixture.workspace).is_err());
    }

    #[test]
    fn process_child_helper() {
        let Ok(mode) = std::env::var("XGENY_PROCESS_TEST_MODE") else {
            return;
        };
        match mode.as_str() {
            "exit-23" => std::process::exit(23),
            "large-output" => {
                println!("{}", "o".repeat(MIN_CAPTURE_BYTES * 4));
                eprintln!("{}", "e".repeat(MIN_CAPTURE_BYTES * 4));
            }
            "tree-parent" => {
                let mut grandchild = spawn_grandchild();
                thread::sleep(Duration::from_secs(10));
                let _ = grandchild.wait();
            }
            "tree-exit-parent" => {
                drop(spawn_grandchild());
            }
            #[cfg(unix)]
            "stdio-escape-parent" => {
                use std::os::unix::process::CommandExt as _;

                let executable = std::env::current_exe().expect("helper executable should resolve");
                let mut child = Command::new(executable);
                child
                    .args([
                        "execution::tests::process_child_helper",
                        "--exact",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env("XGENY_PROCESS_TEST_MODE", "stdio-escape-child")
                    .process_group(0);
                drop(child.spawn().expect("escaped child should spawn"));
            }
            "stdio-escape-child" => {
                thread::sleep(Duration::from_secs(3));
            }
            "tree-child" => {
                thread::sleep(Duration::from_millis(800));
                let marker =
                    std::env::var("XGENY_PROCESS_TEST_MARKER").expect("marker should be inherited");
                fs::write(marker, b"escaped").expect("marker should write if not terminated");
            }
            "print-context" => {
                println!(
                    "cwd-marker={} env={}",
                    std::path::Path::new("cwd-marker").is_file(),
                    std::env::var("XGENY_PROCESS_TEST_VALUE").unwrap()
                );
            }
            "print-argv-zero" => {
                let argv_zero = std::env::args_os()
                    .next()
                    .and_then(|path| {
                        std::path::PathBuf::from(path)
                            .file_name()
                            .map(std::ffi::OsStr::to_os_string)
                    })
                    .expect("argv zero should have a file name");
                println!("argv-zero={}", argv_zero.to_string_lossy());
            }
            other => panic!("unexpected helper mode: {other}"),
        }
    }

    fn spawn_grandchild() -> std::process::Child {
        let executable = std::env::current_exe().expect("helper executable should resolve");
        let mut child = Command::new(executable);
        child
            .args([
                "execution::tests::process_child_helper",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("XGENY_PROCESS_TEST_MODE", "tree-child");
        child.spawn().expect("grandchild should spawn")
    }
}
