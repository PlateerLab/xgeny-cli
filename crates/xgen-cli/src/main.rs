use std::cell::RefCell;
use std::env;
use std::io::{BufRead as _, ErrorKind, IsTerminal as _, Read as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{ArgGroup, Args, Parser, Subcommand};
use url::Url;
use xgen_cli::{
    DriverProgress, DriverProgressControl, InferenceLimits, LocalCommandResult,
    LocalProcessSession, LocalResumeRequest, LocalRunRequest, ModelCheckError, ModelCheckRequest,
    ModelCredentialStore, ModelProfile, ModelProfileError, ModelProfileStore,
    OsModelCredentialStore, PublicRunError, RequestOptions, ResolvedModelEndpoint,
    check_openai_compatibility, check_openai_model, discard_local_model_call,
    discard_local_model_call_at_head, inspect_local_model_call, list_openai_models,
    new_credential_reference, prepare_local_process_session, resume_local,
    resume_local_with_model_resolver, resume_local_with_model_resolver_and_progress,
    resume_local_with_process_session_and_model_resolver_progress,
    run_interactive_with_process_session_progress, run_local_with_started,
};
use xgen_provider_openai::{BearerCredential, ResponseFormat, ThinkingMode};
use zeroize::Zeroizing;

mod repl;

const PROJECT_LICENSE: &str = include_str!("../../../LICENSE");
const CARGO_DEPENDENCY_NOTICES: &str = include_str!("../../../THIRD_PARTY_LICENSES.txt");
const RUST_LIBRARY_NOTICES: &str =
    include_str!(concat!(env!("OUT_DIR"), "/RUST_COPYRIGHT_LIBRARY.html"));
const MUSL_RUNTIME_NOTICES: &str = include_str!("../licenses/musl-1.2.5-COPYRIGHT");
const LLVM_LIBUNWIND_NOTICES: &str = include_str!("../licenses/llvm-libunwind-52ed14f-LICENSE.TXT");

#[derive(Debug, Parser)]
#[command(
    name = "xgen",
    version,
    about = "Local-first general-purpose agent CLI"
)]
struct Cli {
    /// Show durable progress events instead of the interactive Thinking display.
    #[arg(long)]
    debug: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print licenses and notices embedded in this binary.
    Licenses,
    /// Inspect and validate bundled protocol contracts.
    Protocol {
        #[command(subcommand)]
        command: ProtocolCommand,
    },
    /// Configure and verify OpenAI-compatible model profiles without creating Run state.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Start a bounded local Run with confined filesystem and optional shell-free process capabilities.
    Run(RunArgs),
    /// Continue an existing Run, or replay its durable completion without model access.
    Resume(ResumeArgs),
    /// Inspect or explicitly discard an unresolved model call offline; never resumes the Run.
    Recover(RecoverArgs),
}

#[derive(Debug, Args)]
#[command(
    after_long_help = "Without --discard-model-call this command only inspects verified journal state. Discard stops accepting the exact call's response; it does NOT prove non-delivery or refund consumed budget. Neither form calls a model or tool. Continue separately with resume and the original workspace, catalogs, profile, permissions and remaining budget. Output is a bounded JSON report, not a completion result."
)]
struct RecoverArgs {
    /// Durable Run identifier printed by `xgen run`.
    run_id: String,
    /// Explicitly discard this exact active call ID obtained from a prior inspection.
    #[arg(long, value_name = "CALL_ID")]
    discard_model_call: Option<String>,
    /// Require the inspected journal head to still match under the Run lease.
    #[arg(long, value_name = "SHA256_HEAD", requires = "discard_model_call")]
    expected_journal_head: Option<String>,
}

#[derive(Debug, Subcommand)]
enum ProtocolCommand {
    /// Run offline schema, fixture, round-trip, and digest checks.
    Check,
}

#[derive(Debug, Subcommand)]
enum ModelCommand {
    /// Configure, verify, and activate one OpenAI-compatible model profile.
    Setup(ModelSetupArgs),
    /// List non-secret model profiles.
    List,
    /// Select an existing active profile.
    Use(ModelNameArgs),
    /// Check catalog access and exact model advertisement with one GET.
    Check(ModelCheckArgs),
    /// Delete a profile's secure credential while retaining non-secret settings.
    Logout(ModelOptionalNameArgs),
    /// Delete one model profile and its secure credential.
    Remove(ModelNameArgs),
}

#[derive(Debug, Args)]
#[command(
    after_long_help = "Resolution order: explicit options, XGEN_OPENAI_BASE_URL / XGEN_OPENAI_MODEL / XGEN_OPENAI_TOKENIZER environment, then the selected/active profile. Planner inference limits follow XGEN_OPENAI_INFERENCE_TIMEOUT / XGEN_OPENAI_MAX_OUTPUT_TOKENS, then the profile (default 300s / 1024 tokens). HTTPS authentication uses --token-stdin, XGEN_OPENAI_API_KEY, then the profile secure store; no token value is accepted as a command argument."
)]
struct ModelCheckArgs {
    #[command(flatten)]
    request_options: RequestOptionArgs,
    /// OpenAI-compatible API base URL ending in /v1.
    #[arg(long)]
    base_url: Option<String>,
    /// Exact served model identifier expected in GET /v1/models.
    #[arg(long)]
    model: Option<String>,
    /// Tokenizer/profile identifier validated for a later run; defaults to the model ID.
    #[arg(long)]
    tokenizer: Option<String>,
    /// Named profile; defaults to `XGEN_MODEL_PROFILE` and then the active profile.
    #[arg(long)]
    profile: Option<String>,
    /// Read one API token line from standard input; the value is never persisted.
    #[arg(long)]
    token_stdin: bool,
    /// Also send one host-validated Chat Completions probe using the selected response format.
    #[arg(long)]
    compatibility: bool,
}

#[derive(Debug, Args)]
#[command(
    after_long_help = "Interactive setup hides token input and stores it only in the platform secure store. In automation, --token-stdin or XGEN_OPENAI_API_KEY is ephemeral unless --store-token is explicitly supplied."
)]
struct ModelSetupArgs {
    #[command(flatten)]
    request_options: RequestOptionArgs,
    /// Provider preset; omitted interactive setup offers a selection.
    #[arg(long, value_parser = ["deepseek", "openai", "custom"])]
    provider: Option<String>,
    /// Profile name to create or replace.
    #[arg(long, default_value = "default")]
    name: String,
    /// OpenAI-compatible API base URL ending in /v1.
    #[arg(long)]
    base_url: Option<String>,
    /// Exact served model identifier; interactive setup lists and prompts when omitted.
    #[arg(long)]
    model: Option<String>,
    /// Tokenizer/profile identifier; defaults to the selected model ID.
    #[arg(long)]
    tokenizer: Option<String>,
    /// Read one API token line from standard input.
    #[arg(long)]
    token_stdin: bool,
    /// Persist the supplied stdin/environment token in the platform secure store.
    #[arg(long)]
    store_token: bool,
    /// Planner inference wall-clock budget in seconds (1..=3600); defaults to the profile value or 300.
    #[arg(long, value_name = "SECONDS")]
    inference_timeout: Option<u64>,
    /// Planner output token budget (1..=65536); defaults to the profile value or 1024.
    #[arg(long, value_name = "TOKENS")]
    max_output_tokens: Option<u32>,
}

#[derive(Debug, Args)]
struct ModelNameArgs {
    /// Exact model profile name.
    name: String,
}

#[derive(Debug, Args)]
struct ModelOptionalNameArgs {
    /// Exact model profile name; defaults to the active profile.
    name: Option<String>,
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)] // Four independent CLI consent switches.
#[command(
    group(
        ArgGroup::new("read_scope")
            .required(true)
            .multiple(true)
            .args(["allow_files", "allow_dirs"])
    ),
    after_long_help = "Resolution order: explicit options, XGEN_OPENAI_BASE_URL / XGEN_OPENAI_MODEL / XGEN_OPENAI_TOKENIZER environment, then the selected/active profile. Planner inference limits follow XGEN_OPENAI_INFERENCE_TIMEOUT / XGEN_OPENAI_MAX_OUTPUT_TOKENS, then the profile (default 300s / 1024 tokens). HTTPS authentication uses --token-stdin, XGEN_OPENAI_API_KEY, then the profile secure store. Credentials are ignored for loopback HTTP and cannot be passed as a command-line value."
)]
struct RunArgs {
    #[command(flatten)]
    request_options: RequestOptionArgs,
    /// Goal sent to the bounded planner.
    #[arg(help = format!("Goal sent to the bounded planner. XGEN_MAX_GOAL_BYTES={} XGEN_OPENAI_ARTIFACT_SCHEMA=atomic-json-schema-v1 (optional per-invocation JSON Schema; also required unchanged on resume)", xgen_cli::MAX_GOAL_BYTES))]
    goal: String,
    /// Workspace root opened as the local filesystem capability.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// OpenAI-compatible API base URL ending in /v1.
    #[arg(long)]
    base_url: Option<String>,
    /// Served model identifier.
    #[arg(long)]
    model: Option<String>,
    /// Tokenizer/profile identifier committed for restart validation; defaults to the model ID.
    #[arg(long)]
    tokenizer: Option<String>,
    /// Named model profile; defaults to `XGEN_MODEL_PROFILE` and then the active profile.
    #[arg(long)]
    profile: Option<String>,
    /// Read one API token line from standard input for this invocation only.
    #[arg(long)]
    token_stdin: bool,
    /// Stable non-secret planner identity.
    #[arg(long, default_value = "xgeny.cli.openai")]
    planner_id: String,
    /// Exact relative workspace file the model may read; repeat for more files.
    #[arg(long = "allow-file")]
    allow_files: Vec<String>,
    /// Relative workspace directory the model may inspect recursively; use '.' for the root.
    #[arg(long = "allow-dir")]
    allow_dirs: Vec<String>,
    /// Catalog one executable as `LOGICAL_ID=ABSOLUTE_PATH`; repeat for more executables.
    #[arg(long = "allow-executable", value_name = "ID=ABSOLUTE_PATH")]
    allow_executables: Vec<String>,
    /// Explicitly allow goal/context/tool output transfer to the remote model boundary.
    #[arg(long)]
    allow_remote_model_egress: bool,
    /// Approve each one-shot read-only action selected within the declared file/directory scope.
    #[arg(long)]
    allow_read: bool,
    /// Approve each one-shot atomic file write selected within an allow-dir scope.
    #[arg(long)]
    allow_write: bool,
    /// Approve each one-shot process execution selected from the executable catalog.
    #[arg(long)]
    allow_execute: bool,
    /// Bound work performed by this process invocation.
    #[arg(long, default_value_t = 32)]
    max_ticks: u32,
    /// Agent loop budget recorded in the Run manifest for workspace discovery runs.
    #[arg(
        long,
        value_name = "TURNS",
        value_parser = clap::value_parser!(u32).range(1..=i64::from(xgen_cli::MAX_HOST_MODEL_TURNS)),
        help = format!(
            "Agent loop budget recorded in the Run manifest (1..={}); model calls, planned steps and tool calls scale 2:1:1. Resume keeps the recorded budget. XGEN_RUN_BUDGET=manifest-model-turns-v1",
            xgen_cli::MAX_HOST_MODEL_TURNS
        )
    )]
    max_model_turns: Option<u32>,
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)] // Four independent CLI consent switches.
#[command(
    after_long_help = "For an incomplete Run, endpoint resolution is explicit --base-url, XGEN_OPENAI_BASE_URL, then the selected/active profile. HTTPS authentication uses --token-stdin, XGEN_OPENAI_API_KEY, then the matching profile secure store. Credentials are ignored for loopback HTTP."
)]
struct ResumeArgs {
    #[command(flatten)]
    request_options: RequestOptionArgs,
    /// Durable Run identifier printed by `xgen run`.
    run_id: String,
    /// Original physical workspace root; unnecessary for completed replay.
    #[arg(long, default_value = ".")]
    workspace: Option<PathBuf>,
    /// Current OpenAI-compatible base URL; unnecessary for completed replay.
    #[arg(long)]
    base_url: Option<String>,
    /// Named model profile used for endpoint and secure credential resolution.
    #[arg(long)]
    profile: Option<String>,
    /// Read one API token line from standard input for this invocation only.
    #[arg(long)]
    token_stdin: bool,
    /// Same exact allow-file entries supplied to the original Run.
    #[arg(long = "allow-file")]
    allow_files: Vec<String>,
    /// Same allow-dir catalog supplied to the original Run.
    #[arg(long = "allow-dir")]
    allow_dirs: Vec<String>,
    /// Same executable catalog supplied to the original Run.
    #[arg(long = "allow-executable", value_name = "ID=ABSOLUTE_PATH")]
    allow_executables: Vec<String>,
    /// Allow a new remote model request to this invocation's supplied --base-url.
    #[arg(long)]
    allow_remote_model_egress: bool,
    /// Approve each one-shot read-only action selected within the declared file/directory scope.
    #[arg(long)]
    allow_read: bool,
    /// Approve each one-shot atomic file write selected within an allow-dir scope.
    #[arg(long)]
    allow_write: bool,
    /// Approve each one-shot process execution selected from the executable catalog.
    #[arg(long)]
    allow_execute: bool,
    /// Bound work performed by this process invocation.
    #[arg(long, default_value_t = 32)]
    max_ticks: u32,
}

#[derive(Debug, Args, Default)]
struct RequestOptionArgs {
    /// Structured output transport; `json_object` is validated by `XGEN`, not server-enforced schema.
    #[arg(long, value_parser = ["json_schema", "json_object", "json_schema_atomic_json"])]
    response_format: Option<String>,
    /// Explicit provider thinking setting; default omits the provider-specific setting.
    #[arg(long, value_parser = ["default", "disabled", "enabled", "chat_template_disabled"])]
    thinking: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        None => interactive_command(cli.debug),
        Some(Command::Licenses) => print_licenses(),
        Some(Command::Protocol {
            command: ProtocolCommand::Check,
        }) => match xgen_protocol::check_bundled_protocol() {
            Ok(report) => {
                println!("XGEN protocol v0.1: PASS");
                println!("  schemas: {}", report.schema_count);
                println!(
                    "  fixtures: {} ({} valid, {} invalid)",
                    report.fixture_count, report.valid_fixture_count, report.invalid_fixture_count
                );
                println!("  semantic checks: {}", report.semantic_check_count);
                println!("  reference resolution: bundled/offline");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("XGEN protocol v0.1: FAIL");
                eprintln!("  {error}");
                ExitCode::FAILURE
            }
        },
        Some(Command::Model { command }) => run_model_command(command),
        Some(Command::Run(args)) => run_command(args),
        Some(Command::Resume(args)) => resume_command(args),
        Some(Command::Recover(args)) => recover_command(&args),
    }
}

const REPL_MAX_TICKS: u32 = 32;

struct InteractiveHost {
    detailed_progress: bool,
    workspace: PathBuf,
    executable_specs: Vec<String>,
    executable_ids: Vec<String>,
    process_session: Option<LocalProcessSession>,
}

impl InteractiveHost {
    fn new(detailed_progress: bool) -> Result<Self, repl::ReplFailure> {
        let workspace = env::current_dir()
            .map_err(|_| repl::ReplFailure::new(PublicRunError::Configuration.code()))?;
        let mut executable_specs = Vec::new();
        let mut executable_ids = Vec::new();
        for (id, path) in repl::discover_developer_executables() {
            let Some(path) = path
                .to_str()
                .filter(|path| !path.chars().any(char::is_control))
            else {
                continue;
            };
            executable_specs.push(format!("{id}={path}"));
            executable_ids.push(id);
        }
        Ok(Self {
            detailed_progress,
            workspace,
            executable_specs,
            executable_ids,
            process_session: None,
        })
    }

    fn selected_model_view() -> Result<repl::ModelView, repl::ReplFailure> {
        select_profile(None)
            .map_err(|error| repl::ReplFailure::new(error.code()))?
            .map(|profile| repl_model_view(&profile))
            .ok_or_else(|| repl::ReplFailure::new("model_configuration_missing"))
    }

    fn process_session(&mut self) -> Result<LocalProcessSession, repl::ReplFailure> {
        if self.process_session.is_none() {
            self.process_session = Some(
                prepare_local_process_session(&self.workspace, &self.executable_specs)
                    .map_err(|error| repl::ReplFailure::new(error.code()))?,
            );
        }
        self.process_session
            .clone()
            .ok_or_else(|| repl::ReplFailure::new(PublicRunError::Internal.code()))
    }
}

impl repl::ReplHost for InteractiveHost {
    fn model(&mut self) -> Result<repl::ModelView, repl::ReplFailure> {
        Self::selected_model_view()
    }

    fn use_model(&mut self, name: &str) -> Result<repl::ModelView, repl::ReplFailure> {
        try_model_use(name)
            .and_then(|profile| {
                if std::io::stdin().is_terminal() {
                    ensure_interactive_credential(&profile)?;
                }
                Ok(repl_model_view(&profile))
            })
            .map_err(|error| repl::ReplFailure::new(error.code()))
    }

    fn executable_ids(&self) -> &[String] {
        &self.executable_ids
    }

    fn start(
        &mut self,
        goal: String,
        grants: repl::InvocationGrants,
        progress: &mut dyn FnMut(DriverProgress) -> DriverProgressControl,
    ) -> Result<LocalCommandResult, repl::ReplFailure> {
        let model = resolve_model(None, None, None, None, false, RequestOptionArgs::default())
            .map_err(|error| repl::ReplFailure::new(error.code()))?;
        let process_session = self.process_session()?;
        run_interactive_with_process_session_progress(
            LocalRunRequest {
                goal,
                workspace: self.workspace.clone(),
                base_url: model.base_url,
                planner_id: "xgeny.cli.openai".to_owned(),
                model: model.model,
                tokenizer: model.tokenizer,
                credential: model.credential,
                inference_limits: model.inference_limits,
                request_options: model.request_options,
                allow_files: Vec::new(),
                allow_dirs: vec![".".to_owned()],
                allow_executables: Vec::new(),
                allow_remote_model_egress: grants.model,
                allow_read: grants.read,
                allow_write: grants.write,
                allow_execute: grants.execute,
                max_ticks: REPL_MAX_TICKS,
                max_model_turns: None,
            },
            &process_session,
            |run_id| {
                if self.detailed_progress {
                    eprintln!("XGEN_STARTED run_id={run_id}");
                }
            },
            progress,
        )
        .map_err(|error| repl::ReplFailure::new(error.code()))
    }

    fn resume(
        &mut self,
        run_id: &str,
        grants: repl::InvocationGrants,
        progress: &mut dyn FnMut(DriverProgress) -> DriverProgressControl,
    ) -> Result<LocalCommandResult, repl::ReplFailure> {
        let process_session = self.process_session.clone();
        let request = LocalResumeRequest {
            run_id: run_id.to_owned(),
            workspace: Some(self.workspace.clone()),
            base_url: None,
            credential: None,
            inference_limits: InferenceLimits::default(),
            request_options: RequestOptions::default(),
            allow_files: Vec::new(),
            allow_dirs: vec![".".to_owned()],
            allow_executables: if process_session.is_some() {
                Vec::new()
            } else {
                self.executable_specs.clone()
            },
            allow_remote_model_egress: grants.model,
            allow_read: grants.read,
            allow_write: grants.write,
            allow_execute: grants.execute,
            max_ticks: REPL_MAX_TICKS,
        };
        let mut resolution_error = None;
        let result = {
            let mut resolve = || {
                if !grants.model {
                    return Err(PublicRunError::Configuration);
                }
                resolve_endpoint(None, None, false, RequestOptionArgs::default()).map_err(|error| {
                    resolution_error = Some(error);
                    PublicRunError::Configuration
                })
            };
            if let Some(process_session) = process_session.as_ref() {
                resume_local_with_process_session_and_model_resolver_progress(
                    request,
                    process_session,
                    &mut resolve,
                    progress,
                )
            } else {
                resume_local_with_model_resolver_and_progress(request, &mut resolve, progress)
            }
        };
        if let Some(error) = resolution_error {
            Err(repl::ReplFailure::new(error.code()))
        } else {
            result.map_err(|error| repl::ReplFailure::new(error.code()))
        }
    }
}

fn interactive_command(debug: bool) -> ExitCode {
    let terminal = std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal();
    if terminal && let Err(error) = ensure_interactive_model() {
        return present_model_configuration_error(error);
    }

    let cancellation = repl::Cancellation::default();
    let signal_cancellation = cancellation.clone();
    if ctrlc::set_handler(move || signal_cancellation.request()).is_err() {
        eprintln!("XGEN_ERROR code={}", PublicRunError::Internal.code());
        return ExitCode::from(PublicRunError::Internal.exit_code());
    }
    let Ok(mut host) = InteractiveHost::new(!terminal || debug) else {
        eprintln!("XGEN_ERROR code={}", PublicRunError::Configuration.code());
        return ExitCode::from(PublicRunError::Configuration.exit_code());
    };
    let mut input = repl::InterruptibleInput::stdin(cancellation.clone());
    let mut output = std::io::stdout();
    match repl::run_with_display(
        &mut input,
        &mut output,
        &mut host,
        &cancellation,
        terminal && !debug,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(_) => {
            eprintln!("XGEN_ERROR code={}", PublicRunError::Internal.code());
            ExitCode::from(PublicRunError::Internal.exit_code())
        }
    }
}

struct ProviderPreset {
    base_url: &'static str,
    response_format: &'static str,
    thinking: &'static str,
}

fn provider_preset(provider: &str) -> Option<ProviderPreset> {
    match provider {
        "deepseek" => Some(ProviderPreset {
            base_url: "https://api.deepseek.com/v1",
            response_format: "json_object",
            thinking: "disabled",
        }),
        "openai" => Some(ProviderPreset {
            base_url: "https://api.openai.com/v1",
            response_format: "json_schema",
            thinking: "default",
        }),
        _ => None,
    }
}

fn prompt_provider() -> Result<String, ModelCliError> {
    eprintln!(
        "Choose a provider:\n  1) DeepSeek\n  2) OpenAI\n  3) Other OpenAI-compatible endpoint"
    );
    match prompt_line("Provider number: ")?.as_str() {
        "1" => Ok("deepseek".to_owned()),
        "2" => Ok("openai".to_owned()),
        "3" => Ok("custom".to_owned()),
        _ => Err(ModelCliError::InputUnavailable),
    }
}

thread_local! {
    // Never persisted or placed in the environment inherited by tool processes.
    static SESSION_CREDENTIAL: RefCell<Option<(String, Zeroizing<String>)>> = const { RefCell::new(None) };
}

fn remember_session_credential(base_url: &str, value: &str) {
    SESSION_CREDENTIAL.with(|slot| {
        *slot.borrow_mut() = Some((base_url.to_owned(), Zeroizing::new(value.to_owned())));
    });
}

fn session_credential(base_url: &str) -> Option<Zeroizing<String>> {
    SESSION_CREDENTIAL.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|(endpoint, _)| endpoint == base_url)
            .map(|(_, value)| value.clone())
    })
}

fn store_setup_credential(
    credentials: &impl ModelCredentialStore,
    reference: &str,
    base_url: &str,
    value: &str,
    allow_session: bool,
) -> Result<bool, ModelCliError> {
    match credentials.put(reference, value) {
        Ok(()) => Ok(true),
        Err(ModelProfileError::CredentialStoreUnavailable) if allow_session => {
            remember_session_credential(base_url, value);
            eprintln!(
                "Secure credential store unavailable. The key is held only for this process; next launch will ask again."
            );
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

fn ensure_interactive_credential(profile: &ModelProfile) -> Result<(), ModelCliError> {
    let endpoint =
        read_environment("XGEN_OPENAI_BASE_URL")?.unwrap_or_else(|| profile.base_url().to_owned());
    let url = Url::parse(&endpoint).map_err(|_| ModelCliError::MissingConfiguration)?;
    if url.scheme() != "https"
        || read_secret_environment()?.is_some()
        || session_credential(&endpoint).is_some()
    {
        return Ok(());
    }
    if endpoint == profile.base_url()
        && let Some(reference) = profile.credential_reference()
    {
        match OsModelCredentialStore.get(reference) {
            Ok(_) => return Ok(()),
            Err(
                ModelProfileError::CredentialNotFound
                | ModelProfileError::CredentialStoreUnavailable,
            ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    eprintln!(
        "No saved key is available. Enter a key for this session; it will not be written to disk."
    );
    let value = Zeroizing::new(
        rpassword::prompt_password("API key (hidden; leave empty for no authentication): ")
            .map_err(|_| ModelCliError::InputUnavailable)?,
    );
    if value.is_empty() {
        return Ok(());
    }
    BearerCredential::new(&value).map_err(|_| ModelCliError::InvalidCredential)?;
    remember_session_credential(&endpoint, &value);
    Ok(())
}

fn ensure_interactive_model() -> Result<(), ModelCliError> {
    if let Some(profile) = select_profile(None)? {
        ensure_interactive_credential(&profile)?;
        return Ok(());
    }
    let (profile, stored) = try_model_setup(ModelSetupArgs {
        request_options: RequestOptionArgs::default(),
        provider: None,
        name: "default".to_owned(),
        base_url: None,
        model: None,
        tokenizer: None,
        token_stdin: false,
        store_token: false,
        inference_timeout: None,
        max_output_tokens: None,
    })?;
    println!("XGEN model setup: PASS");
    println!("  profile: {}", profile.name());
    println!("  model: {}", profile.model());
    println!(
        "  authentication: {}",
        if stored {
            "secure_store"
        } else {
            "external_or_none"
        }
    );
    Ok(())
}

fn repl_model_view(profile: &ModelProfile) -> repl::ModelView {
    repl::ModelView {
        profile: profile.name().to_owned(),
        model: profile.model().to_owned(),
        authentication: if profile.has_stored_credential() {
            "secure_store"
        } else {
            "external_or_none"
        },
    }
}

fn run_model_command(command: ModelCommand) -> ExitCode {
    match command {
        ModelCommand::Setup(args) => model_setup(args),
        ModelCommand::List => model_list(),
        ModelCommand::Use(args) => model_use(&args.name),
        ModelCommand::Check(args) => model_check(args),
        ModelCommand::Logout(args) => model_logout(args.name.as_deref()),
        ModelCommand::Remove(args) => model_remove(&args.name),
    }
}

fn run_command(args: RunArgs) -> ExitCode {
    let resolved = match resolve_model(
        args.base_url,
        args.model,
        args.tokenizer,
        args.profile,
        args.token_stdin,
        args.request_options,
    ) {
        Ok(resolved) => resolved,
        Err(error) => return present_model_configuration_error(error),
    };
    present(run_local_with_started(
        LocalRunRequest {
            goal: args.goal,
            workspace: args.workspace,
            base_url: resolved.base_url,
            planner_id: args.planner_id,
            model: resolved.model,
            tokenizer: resolved.tokenizer,
            credential: resolved.credential,
            inference_limits: resolved.inference_limits,
            request_options: resolved.request_options,
            allow_files: args.allow_files,
            allow_dirs: args.allow_dirs,
            allow_executables: args.allow_executables,
            allow_remote_model_egress: args.allow_remote_model_egress,
            allow_read: args.allow_read,
            allow_write: args.allow_write,
            allow_execute: args.allow_execute,
            max_ticks: args.max_ticks,
            max_model_turns: args.max_model_turns,
        },
        |run_id| eprintln!("XGEN_STARTED run_id={run_id}"),
    ))
}

fn resume_command(args: ResumeArgs) -> ExitCode {
    let ResumeArgs {
        request_options,
        run_id,
        workspace,
        base_url,
        profile,
        token_stdin,
        allow_files,
        allow_dirs,
        allow_executables,
        allow_remote_model_egress,
        allow_read,
        allow_write,
        allow_execute,
        max_ticks,
    } = args;
    let request = LocalResumeRequest {
        run_id,
        workspace,
        base_url: None,
        credential: None,
        inference_limits: InferenceLimits::default(),
        request_options: RequestOptions::default(),
        allow_files,
        allow_dirs,
        allow_executables,
        allow_remote_model_egress,
        allow_read,
        allow_write,
        allow_execute,
        max_ticks,
    };
    if !allow_remote_model_egress {
        return present(resume_local(request));
    }

    let mut resolution_error = None;
    let result = resume_local_with_model_resolver(request, || {
        resolve_endpoint(base_url, profile, token_stdin, request_options).map_err(|error| {
            resolution_error = Some(error);
            PublicRunError::Configuration
        })
    });
    if let Some(error) = resolution_error {
        present_model_configuration_error(error)
    } else {
        present(result)
    }
}

fn recover_command(args: &RecoverArgs) -> ExitCode {
    let result = match &args.discard_model_call {
        Some(call_id) => match &args.expected_journal_head {
            Some(head) => discard_local_model_call_at_head(&args.run_id, call_id, head),
            None => discard_local_model_call(&args.run_id, call_id),
        },
        None => inspect_local_model_call(&args.run_id),
    };
    match result {
        Ok(report) => {
            // A failed stdout write can follow a committed discard. Inspect before retrying.
            let mut stdout = std::io::stdout().lock();
            if serde_json::to_writer(&mut stdout, &report).is_err()
                || stdout.write_all(b"\n").is_err()
                || stdout.flush().is_err()
            {
                return present(Err(PublicRunError::Internal));
            }
            ExitCode::SUCCESS
        }
        Err(error) => present(Err(error)),
    }
}

const MAX_TOKEN_INPUT_BYTES: u64 = 16 * 1024;

struct ResolvedModel {
    base_url: String,
    model: String,
    tokenizer: String,
    credential: Option<BearerCredential>,
    inference_limits: InferenceLimits,
    request_options: RequestOptions,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupSecretSource {
    None,
    StandardInput,
    Environment,
    SecureStore,
    Interactive,
}

struct SetupSecret {
    value: Option<Zeroizing<String>>,
    source: SetupSecretSource,
}

#[derive(Debug, Clone, Copy)]
enum ModelCliError {
    Profile(ModelProfileError),
    Check(ModelCheckError),
    MissingConfiguration,
    InvalidEnvironment,
    InputUnavailable,
    InvalidCredential,
    CredentialRequiresHttps,
    InvalidInferenceLimits,
    InvalidRequestOptions,
}

impl ModelCliError {
    const fn code(self) -> &'static str {
        match self {
            Self::Profile(error) => error.code(),
            Self::Check(error) => error.code(),
            Self::MissingConfiguration => "model_configuration_missing",
            Self::InvalidEnvironment => "environment_invalid",
            Self::InputUnavailable => "input_unavailable",
            Self::InvalidCredential => "api_key_invalid",
            Self::CredentialRequiresHttps => "api_key_requires_https",
            Self::InvalidInferenceLimits => "inference_limits_invalid",
            Self::InvalidRequestOptions => "request_options_invalid",
        }
    }

    const fn exit_code(self) -> u8 {
        match self {
            Self::Profile(
                ModelProfileError::CredentialNotFound
                | ModelProfileError::CredentialStoreUnavailable,
            ) => 77,
            Self::Profile(
                ModelProfileError::ProfileStoreUnavailable
                | ModelProfileError::ProfileCommitUnknown,
            ) => 69,
            Self::Profile(_)
            | Self::MissingConfiguration
            | Self::InvalidEnvironment
            | Self::InputUnavailable
            | Self::InvalidCredential
            | Self::CredentialRequiresHttps
            | Self::InvalidInferenceLimits
            | Self::InvalidRequestOptions => 64,
            Self::Check(error) => error.exit_code(),
        }
    }
}

impl From<ModelProfileError> for ModelCliError {
    fn from(error: ModelProfileError) -> Self {
        Self::Profile(error)
    }
}

impl From<ModelCheckError> for ModelCliError {
    fn from(error: ModelCheckError) -> Self {
        Self::Check(error)
    }
}

fn model_setup(args: ModelSetupArgs) -> ExitCode {
    match try_model_setup(args) {
        Ok((profile, stored)) => {
            println!("XGEN model setup: PASS");
            println!("  profile: {}", profile.name());
            println!("  model: {}", profile.model());
            println!("  catalog: exact model advertised");
            println!(
                "  chat completions: {}",
                compatibility_label(profile.request_options())
            );
            println!(
                "  thinking: {}",
                thinking_label(profile.request_options().thinking)
            );
            println!(
                "  inference limits: timeout={}s max_output_tokens={}",
                profile.inference_limits().timeout().as_secs(),
                profile.inference_limits().max_output_tokens()
            );
            println!(
                "  authentication: {}",
                if stored {
                    "secure_store"
                } else {
                    "external_or_none"
                }
            );
            ExitCode::SUCCESS
        }
        Err(error) => present_model_command_error("setup", error),
    }
}

#[allow(clippy::too_many_lines)]
fn try_model_setup(mut args: ModelSetupArgs) -> Result<(ModelProfile, bool), ModelCliError> {
    let store = ModelProfileStore::discover()?;
    let mut profiles = store.load()?;
    let existing = profiles.get(&args.name).cloned();
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let configured_url = args
        .base_url
        .take()
        .or(read_environment("XGEN_OPENAI_BASE_URL")?)
        .or_else(|| {
            existing
                .as_ref()
                .map(|profile| profile.base_url().to_owned())
        });
    let provider = match args.provider.take() {
        Some(provider) => Some(provider),
        None if interactive && configured_url.is_none() => Some(prompt_provider()?),
        None => None,
    };
    let preset = provider.as_deref().and_then(provider_preset);
    let base_url = configured_url
        .or_else(|| preset.as_ref().map(|preset| preset.base_url.to_owned()))
        .map_or_else(
            || {
                if interactive {
                    prompt_line("OpenAI-compatible base URL (ending in /v1): ")
                } else {
                    Err(ModelCliError::MissingConfiguration)
                }
            },
            Ok,
        )?;
    // Presets supply defaults only; explicit options, environment and existing profiles win.
    if existing.is_none()
        && let Some(preset) = preset
    {
        if args.request_options.response_format.is_none()
            && read_environment("XGEN_OPENAI_RESPONSE_FORMAT")?.is_none()
        {
            args.request_options.response_format = Some(preset.response_format.to_owned());
        }
        if args.request_options.thinking.is_none()
            && read_environment("XGEN_OPENAI_THINKING")?.is_none()
        {
            args.request_options.thinking = Some(preset.thinking.to_owned());
        }
    }
    let secret = resolve_setup_secret(&base_url, args.token_stdin, existing.as_ref(), interactive)?;
    if args.store_token && secret.value.is_none() {
        return Err(ModelCliError::InvalidCredential);
    }
    let credential =
        credential_from_secret(&base_url, secret.value.as_deref().map(String::as_str))?;
    let requested_model = args
        .model
        .or(read_environment("XGEN_OPENAI_MODEL")?)
        .or_else(|| existing.as_ref().map(|profile| profile.model().to_owned()));
    let catalog_identity = requested_model
        .clone()
        .unwrap_or_else(|| "xgen-catalog-discovery".to_owned());
    let models = list_openai_models(ModelCheckRequest {
        base_url: base_url.clone(),
        model: catalog_identity.clone(),
        tokenizer: catalog_identity,
        credential: credential.clone(),
        inference_limits: InferenceLimits::default(),
        request_options: RequestOptions::default(),
    })?;
    let model = match requested_model {
        Some(model) if models.iter().any(|candidate| candidate == &model) => model,
        Some(_) => return Err(ModelCliError::Check(ModelCheckError::ModelNotAdvertised)),
        None if interactive => prompt_model(&models)?,
        None if models.len() == 1 => models[0].clone(),
        None => return Err(ModelCliError::MissingConfiguration),
    };
    let tokenizer = args
        .tokenizer
        .or(read_environment("XGEN_OPENAI_TOKENIZER")?)
        .or_else(|| {
            existing.as_ref().and_then(|profile| {
                (profile.model() == model).then(|| profile.tokenizer().to_owned())
            })
        })
        .unwrap_or_else(|| model.clone());

    let inference_limits = resolve_inference_limits(
        args.inference_timeout,
        args.max_output_tokens,
        existing.as_ref(),
    )?;
    let request_options = resolve_request_options(args.request_options, existing.as_ref())?;
    check_openai_compatibility(ModelCheckRequest {
        base_url: base_url.clone(),
        model: model.clone(),
        tokenizer: tokenizer.clone(),
        credential,
        inference_limits,
        request_options,
    })?;

    let _lock = store.try_lock()?;
    if store.load()? != profiles {
        return Err(ModelProfileError::ConcurrentModification.into());
    }

    let old_reference = existing
        .as_ref()
        .and_then(ModelProfile::credential_reference)
        .map(str::to_owned);
    let credentials = OsModelCredentialStore;
    let mut profile = ModelProfile::new(&args.name, base_url.clone(), model, tokenizer)?;
    profile.set_inference_limits(inference_limits)?;
    profile.set_request_options(request_options)?;
    let retain_existing = secret.source == SetupSecretSource::SecureStore;
    let should_store = args.store_token || secret.source == SetupSecretSource::Interactive;
    let mut new_reference = None;

    if retain_existing {
        profile.set_credential_reference(old_reference.clone())?;
    } else if should_store {
        if let Some(reference) = old_reference.as_deref() {
            credentials.delete(reference)?;
        }
        let reference = new_credential_reference()?;
        let value = secret
            .value
            .as_deref()
            .ok_or(ModelCliError::InvalidCredential)?;
        if store_setup_credential(
            &credentials,
            &reference,
            &base_url,
            value,
            interactive && !args.store_token && secret.source == SetupSecretSource::Interactive,
        )? {
            profile.set_credential_reference(Some(reference.clone()))?;
            new_reference = Some(reference);
        }
    } else if let Some(reference) = old_reference.as_deref() {
        credentials.delete(reference)?;
    }

    profiles.upsert(profile.clone())?;
    profiles.set_active(profile.name())?;
    if let Err(error) = store.save(&mut profiles) {
        if let Some(reference) = new_reference.as_deref() {
            let _ = credentials.delete(reference);
        }
        return Err(error.into());
    }
    Ok((profile.clone(), profile.has_stored_credential()))
}

fn model_list() -> ExitCode {
    let result = (|| -> Result<(), ModelCliError> {
        let profiles = ModelProfileStore::discover()?.load()?;
        if profiles.iter().next().is_none() {
            println!("No model profiles configured. Run `xgen model setup`.");
            return Ok(());
        }
        for profile in profiles.iter() {
            let marker = if profiles.active_name() == Some(profile.name()) {
                "*"
            } else {
                " "
            };
            println!(
                "{marker} {} model={} tokenizer={} timeout={}s max_output_tokens={} response_format={} thinking={} authentication={}",
                profile.name(),
                profile.model(),
                profile.tokenizer(),
                profile.inference_limits().timeout().as_secs(),
                profile.inference_limits().max_output_tokens(),
                response_format_label(profile.request_options().response_format),
                thinking_label(profile.request_options().thinking),
                if profile.has_stored_credential() {
                    "secure_store"
                } else {
                    "external_or_none"
                }
            );
        }
        Ok(())
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => present_model_command_error("list", error),
    }
}

fn model_use(name: &str) -> ExitCode {
    match try_model_use(name) {
        Ok(_) => {
            println!("XGEN active model profile: {name}");
            ExitCode::SUCCESS
        }
        Err(error) => present_model_command_error("use", error),
    }
}

fn try_model_use(name: &str) -> Result<ModelProfile, ModelCliError> {
    let store = ModelProfileStore::discover()?;
    let _lock = store.try_lock()?;
    let mut profiles = store.load()?;
    profiles.set_active(name)?;
    let selected = profiles
        .active()
        .cloned()
        .ok_or(ModelProfileError::ProfileNotFound)?;
    store.save(&mut profiles)?;
    Ok(selected)
}

fn model_logout(name: Option<&str>) -> ExitCode {
    let result = (|| -> Result<String, ModelCliError> {
        let store = ModelProfileStore::discover()?;
        let _lock = store.try_lock()?;
        let mut profiles = store.load()?;
        let selected = name
            .map(str::to_owned)
            .or_else(|| profiles.active_name().map(str::to_owned))
            .ok_or(ModelCliError::MissingConfiguration)?;
        let reference = profiles
            .get(&selected)
            .ok_or(ModelProfileError::ProfileNotFound)?
            .credential_reference()
            .map(str::to_owned);
        if let Some(reference) = reference.as_deref() {
            OsModelCredentialStore.delete(reference)?;
        }
        profiles.clear_credential(&selected)?;
        store.save(&mut profiles)?;
        Ok(selected)
    })();
    match result {
        Ok(name) => {
            println!("XGEN model credential removed: {name}");
            ExitCode::SUCCESS
        }
        Err(error) => present_model_command_error("logout", error),
    }
}

fn model_remove(name: &str) -> ExitCode {
    let result = (|| -> Result<(), ModelCliError> {
        let store = ModelProfileStore::discover()?;
        let _lock = store.try_lock()?;
        let mut profiles = store.load()?;
        let reference = profiles
            .get(name)
            .ok_or(ModelProfileError::ProfileNotFound)?
            .credential_reference()
            .map(str::to_owned);
        if let Some(reference) = reference.as_deref() {
            OsModelCredentialStore.delete(reference)?;
        }
        profiles.remove(name)?;
        store.save(&mut profiles)?;
        println!("XGEN model profile removed: {name}");
        Ok(())
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => present_model_command_error("remove", error),
    }
}

fn model_check(args: ModelCheckArgs) -> ExitCode {
    let resolved = match resolve_model(
        args.base_url,
        args.model,
        args.tokenizer,
        args.profile,
        args.token_stdin,
        args.request_options,
    ) {
        Ok(resolved) => resolved,
        Err(error) => return present_model_command_error("check", error),
    };
    let request = ModelCheckRequest {
        base_url: resolved.base_url.clone(),
        model: resolved.model.clone(),
        tokenizer: resolved.tokenizer.clone(),
        credential: resolved.credential.clone(),
        inference_limits: resolved.inference_limits,
        request_options: resolved.request_options,
    };
    if let Err(error) = check_openai_model(request) {
        return present_model_check_error(error);
    }
    if args.compatibility
        && let Err(error) = check_openai_compatibility(ModelCheckRequest {
            base_url: resolved.base_url,
            model: resolved.model,
            tokenizer: resolved.tokenizer,
            credential: resolved.credential,
            inference_limits: resolved.inference_limits,
            request_options: resolved.request_options,
        })
    {
        return present_model_check_error(error);
    }
    println!("XGEN model check: PASS");
    println!("  model catalog: exact model advertised");
    println!(
        "  chat completions: {}",
        if args.compatibility {
            compatibility_label(resolved.request_options)
        } else {
            "not requested"
        }
    );
    println!("  inference requests: {}", usize::from(args.compatibility));
    ExitCode::SUCCESS
}

fn resolve_model(
    base_url: Option<String>,
    model: Option<String>,
    tokenizer: Option<String>,
    profile_name: Option<String>,
    token_stdin: bool,
    request_options: RequestOptionArgs,
) -> Result<ResolvedModel, ModelCliError> {
    let profile = select_profile(profile_name)?;
    let base_url = base_url
        .or(read_environment("XGEN_OPENAI_BASE_URL")?)
        .or_else(|| {
            profile
                .as_ref()
                .map(|profile| profile.base_url().to_owned())
        })
        .ok_or(ModelCliError::MissingConfiguration)?;
    let model = model
        .or(read_environment("XGEN_OPENAI_MODEL")?)
        .or_else(|| profile.as_ref().map(|profile| profile.model().to_owned()))
        .ok_or(ModelCliError::MissingConfiguration)?;
    let tokenizer = tokenizer
        .or(read_environment("XGEN_OPENAI_TOKENIZER")?)
        .or_else(|| {
            profile
                .as_ref()
                .map(|profile| profile.tokenizer().to_owned())
        })
        .unwrap_or_else(|| model.clone());
    let credential = resolve_credential(&base_url, token_stdin, profile.as_ref())?;
    let inference_limits = resolve_inference_limits(None, None, profile.as_ref())?;
    let request_options = resolve_request_options(request_options, profile.as_ref())?;
    Ok(ResolvedModel {
        base_url,
        model,
        tokenizer,
        credential,
        inference_limits,
        request_options,
    })
}

/// Resolve planner limits: explicit option, `XGEN_OPENAI_INFERENCE_TIMEOUT` /
/// `XGEN_OPENAI_MAX_OUTPUT_TOKENS`, the profile, then the ADR-0035 defaults.
fn resolve_inference_limits(
    timeout_seconds: Option<u64>,
    max_output_tokens: Option<u32>,
    profile: Option<&ModelProfile>,
) -> Result<InferenceLimits, ModelCliError> {
    let base = profile
        .map(ModelProfile::inference_limits)
        .unwrap_or_default();
    let timeout_seconds = match timeout_seconds {
        Some(value) => value,
        None => match read_environment("XGEN_OPENAI_INFERENCE_TIMEOUT")? {
            Some(value) => value
                .trim()
                .parse()
                .map_err(|_| ModelCliError::InvalidInferenceLimits)?,
            None => base.timeout().as_secs(),
        },
    };
    let max_output_tokens = match max_output_tokens {
        Some(value) => value,
        None => match read_environment("XGEN_OPENAI_MAX_OUTPUT_TOKENS")? {
            Some(value) => value
                .trim()
                .parse()
                .map_err(|_| ModelCliError::InvalidInferenceLimits)?,
            None => base.max_output_tokens(),
        },
    };
    InferenceLimits::new(Duration::from_secs(timeout_seconds), max_output_tokens)
        .map_err(|_| ModelCliError::InvalidInferenceLimits)
}

fn resolve_endpoint(
    base_url: Option<String>,
    profile_name: Option<String>,
    token_stdin: bool,
    request_options: RequestOptionArgs,
) -> Result<ResolvedModelEndpoint, ModelCliError> {
    let profile = select_profile(profile_name)?;
    let base_url = base_url
        .or(read_environment("XGEN_OPENAI_BASE_URL")?)
        .or_else(|| {
            profile
                .as_ref()
                .map(|profile| profile.base_url().to_owned())
        })
        .ok_or(ModelCliError::MissingConfiguration)?;
    let credential = resolve_credential(&base_url, token_stdin, profile.as_ref())?;
    Ok(ResolvedModelEndpoint {
        base_url,
        credential,
        inference_limits: resolve_inference_limits(None, None, profile.as_ref())?,
        request_options: resolve_request_options(request_options, profile.as_ref())?,
    })
}

fn resolve_request_options(
    explicit: RequestOptionArgs,
    profile: Option<&ModelProfile>,
) -> Result<RequestOptions, ModelCliError> {
    let base = profile
        .map(ModelProfile::request_options)
        .unwrap_or_default();
    let response_format = match explicit
        .response_format
        .or(read_environment("XGEN_OPENAI_RESPONSE_FORMAT")?)
        .as_deref()
    {
        None => base.response_format,
        Some("json_schema") => ResponseFormat::JsonSchema,
        Some("json_object") => ResponseFormat::JsonObject,
        Some("json_schema_atomic_json") => ResponseFormat::JsonSchemaAtomicJson,
        Some(_) => return Err(ModelCliError::InvalidRequestOptions),
    };
    let thinking = match explicit
        .thinking
        .or(read_environment("XGEN_OPENAI_THINKING")?)
        .as_deref()
    {
        None => base.thinking,
        Some("default") => ThinkingMode::Default,
        Some("disabled") => ThinkingMode::Disabled,
        Some("enabled") => ThinkingMode::Enabled,
        Some("chat_template_disabled") => ThinkingMode::ChatTemplateDisabled,
        Some(_) => return Err(ModelCliError::InvalidRequestOptions),
    };
    Ok(RequestOptions {
        response_format,
        thinking,
    })
}

const fn response_format_label(format: ResponseFormat) -> &'static str {
    match format {
        ResponseFormat::JsonSchema => "json_schema",
        ResponseFormat::JsonObject => "json_object",
        ResponseFormat::JsonSchemaAtomicJson => "json_schema_atomic_json",
    }
}

const fn thinking_label(thinking: ThinkingMode) -> &'static str {
    match thinking {
        ThinkingMode::Default => "default",
        ThinkingMode::Disabled => "disabled",
        ThinkingMode::Enabled => "enabled",
        ThinkingMode::ChatTemplateDisabled => "chat_template_disabled",
    }
}

const fn compatibility_label(options: RequestOptions) -> &'static str {
    match options.response_format {
        ResponseFormat::JsonSchema => "strict JSON compatible",
        ResponseFormat::JsonSchemaAtomicJson => {
            "atomic JSON wire compatible (native write still verified)"
        }
        ResponseFormat::JsonObject => {
            "JSON object compatible (host-validated; no server schema guarantee)"
        }
    }
}

fn select_profile(name: Option<String>) -> Result<Option<ModelProfile>, ModelCliError> {
    let requested = name.or(read_environment("XGEN_MODEL_PROFILE")?);
    let store = match ModelProfileStore::discover() {
        Ok(store) => store,
        Err(ModelProfileError::ProfileStoreUnavailable) if requested.is_none() => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let profiles = store.load()?;
    match requested {
        Some(name) => profiles
            .get(&name)
            .cloned()
            .map(Some)
            .ok_or(ModelProfileError::ProfileNotFound.into()),
        None => Ok(profiles.active().cloned()),
    }
}

fn resolve_credential(
    base_url: &str,
    token_stdin: bool,
    profile: Option<&ModelProfile>,
) -> Result<Option<BearerCredential>, ModelCliError> {
    let url = Url::parse(base_url).map_err(|_| ModelCliError::MissingConfiguration)?;
    if url.scheme() != "https" {
        if token_stdin {
            return Err(ModelCliError::CredentialRequiresHttps);
        }
        return Ok(None);
    }
    let token = if token_stdin {
        read_token_stdin()?
    } else if let Some(token) = read_secret_environment()? {
        Some(token)
    } else if let Some(value) = session_credential(base_url) {
        Some(value)
    } else if let Some(profile) = profile.filter(|profile| profile.base_url() == base_url) {
        profile
            .credential_reference()
            .map(|reference| OsModelCredentialStore.get(reference))
            .transpose()?
    } else {
        None
    };
    credential_from_secret(base_url, token.as_deref().map(String::as_str))
}

fn resolve_setup_secret(
    base_url: &str,
    token_stdin: bool,
    existing: Option<&ModelProfile>,
    interactive: bool,
) -> Result<SetupSecret, ModelCliError> {
    let url = Url::parse(base_url).map_err(|_| ModelCliError::MissingConfiguration)?;
    if url.scheme() != "https" {
        if token_stdin {
            return Err(ModelCliError::CredentialRequiresHttps);
        }
        return Ok(SetupSecret {
            value: None,
            source: SetupSecretSource::None,
        });
    }
    if token_stdin {
        return Ok(SetupSecret {
            value: read_token_stdin()?,
            source: SetupSecretSource::StandardInput,
        });
    }
    if let Some(value) = read_secret_environment()? {
        return Ok(SetupSecret {
            value: Some(value),
            source: SetupSecretSource::Environment,
        });
    }
    if let Some(reference) = existing
        .filter(|profile| profile.base_url() == base_url)
        .and_then(ModelProfile::credential_reference)
    {
        match OsModelCredentialStore.get(reference) {
            Ok(value) => {
                return Ok(SetupSecret {
                    value: Some(value),
                    source: SetupSecretSource::SecureStore,
                });
            }
            Err(ModelProfileError::CredentialNotFound) if interactive => {}
            Err(error) => return Err(error.into()),
        }
    }
    if interactive {
        let value = Zeroizing::new(
            rpassword::prompt_password("API key (hidden; leave empty for no authentication): ")
                .map_err(|_| ModelCliError::InputUnavailable)?,
        );
        if value.is_empty() {
            return Ok(SetupSecret {
                value: None,
                source: SetupSecretSource::None,
            });
        }
        return Ok(SetupSecret {
            value: Some(value),
            source: SetupSecretSource::Interactive,
        });
    }
    Ok(SetupSecret {
        value: None,
        source: SetupSecretSource::None,
    })
}

fn credential_from_secret(
    base_url: &str,
    secret: Option<&str>,
) -> Result<Option<BearerCredential>, ModelCliError> {
    let url = Url::parse(base_url).map_err(|_| ModelCliError::MissingConfiguration)?;
    if url.scheme() != "https" {
        return if secret.is_some() {
            Err(ModelCliError::CredentialRequiresHttps)
        } else {
            Ok(None)
        };
    }
    secret
        .map(BearerCredential::new)
        .transpose()
        .map_err(|_| ModelCliError::InvalidCredential)
}

fn read_environment(name: &str) -> Result<Option<String>, ModelCliError> {
    xgen_cli::compatible_environment(name)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| ModelCliError::InvalidEnvironment)
        })
        .transpose()
}

fn read_secret_environment() -> Result<Option<Zeroizing<String>>, ModelCliError> {
    read_environment("XGEN_OPENAI_API_KEY").map(|value| value.map(Zeroizing::new))
}

fn read_token_stdin() -> Result<Option<Zeroizing<String>>, ModelCliError> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock().take(MAX_TOKEN_INPUT_BYTES + 2);
    let mut value = String::new();
    let read = reader
        .read_line(&mut value)
        .map_err(|_| ModelCliError::InputUnavailable)?;
    if read == 0 {
        return Err(ModelCliError::InvalidCredential);
    }
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    if value.is_empty() || u64::try_from(value.len()).unwrap_or(u64::MAX) > MAX_TOKEN_INPUT_BYTES {
        return Err(ModelCliError::InvalidCredential);
    }
    Ok(Some(Zeroizing::new(value)))
}

fn prompt_line(prompt: &str) -> Result<String, ModelCliError> {
    eprint!("{prompt}");
    std::io::stderr()
        .flush()
        .map_err(|_| ModelCliError::InputUnavailable)?;
    let mut value = String::new();
    std::io::stdin()
        .read_line(&mut value)
        .map_err(|_| ModelCliError::InputUnavailable)?;
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(ModelCliError::InputUnavailable);
    }
    Ok(value)
}

fn prompt_model(models: &[String]) -> Result<String, ModelCliError> {
    eprintln!("Available models:");
    for (index, model) in models.iter().enumerate() {
        eprintln!("  {}) {model}", index + 1);
    }
    let selected = prompt_line("Select model number: ")?
        .parse::<usize>()
        .ok()
        .filter(|index| (1..=models.len()).contains(index))
        .ok_or(ModelCliError::InputUnavailable)?;
    Ok(models[selected - 1].clone())
}

fn present_model_command_error(command: &str, error: ModelCliError) -> ExitCode {
    eprintln!("XGEN model {command}: FAIL");
    eprintln!("  reason={}", error.code());
    ExitCode::from(error.exit_code())
}

fn present_model_configuration_error(error: ModelCliError) -> ExitCode {
    eprintln!("XGEN_ERROR code={}", error.code());
    ExitCode::from(error.exit_code())
}

fn present_model_check_error(error: ModelCheckError) -> ExitCode {
    eprintln!("XGEN model check: FAIL");
    eprintln!("  reason={}", error.code());
    ExitCode::from(error.exit_code())
}

fn print_licenses() -> ExitCode {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    for (title, contents) in [
        ("XGEN project license", PROJECT_LICENSE),
        ("Cargo dependency notices", CARGO_DEPENDENCY_NOTICES),
        ("Rust standard library notices", RUST_LIBRARY_NOTICES),
        ("musl C runtime notices", MUSL_RUNTIME_NOTICES),
        ("LLVM libunwind notices", LLVM_LIBUNWIND_NOTICES),
    ] {
        let result = writeln!(output, "===== {title} =====")
            .and_then(|()| output.write_all(contents.as_bytes()))
            .and_then(|()| writeln!(output));
        if let Err(error) = result {
            if error.kind() == ErrorKind::BrokenPipe {
                return ExitCode::SUCCESS;
            }
            eprintln!("XGEN_ERROR code={}", PublicRunError::Internal.code());
            return ExitCode::from(PublicRunError::Internal.exit_code());
        }
    }
    ExitCode::SUCCESS
}

fn present(result: Result<LocalCommandResult, PublicRunError>) -> ExitCode {
    match result {
        Ok(LocalCommandResult::Responded { run_id, summary }) => {
            eprintln!("XGEN_RESPONDED run_id={run_id}");
            if std::io::stdout().write_all(summary.as_bytes()).is_err() {
                eprintln!("XGEN_ERROR code={}", PublicRunError::Internal.code());
                return ExitCode::from(PublicRunError::Internal.exit_code());
            }
            ExitCode::SUCCESS
        }
        Ok(LocalCommandResult::Completed { run_id, summary }) => {
            eprintln!("XGEN_COMPLETED run_id={run_id}");
            if std::io::stdout().write_all(summary.as_bytes()).is_err() {
                eprintln!("XGEN_ERROR code={}", PublicRunError::Internal.code());
                return ExitCode::from(PublicRunError::Internal.exit_code());
            }
            ExitCode::SUCCESS
        }
        Ok(LocalCommandResult::Paused { run_id, reason }) => {
            if let Some(run_id) = run_id {
                eprintln!("XGEN_PAUSED run_id={run_id} reason={}", reason.code());
            } else {
                eprintln!("XGEN_PAUSED reason={}", reason.code());
            }
            ExitCode::from(10)
        }
        Ok(LocalCommandResult::Rejected { run_id, reason }) => {
            eprintln!("XGEN_REJECTED run_id={run_id} reason={}", reason.code());
            if let Some(diagnostic) = reason.invocation_diagnostic() {
                eprintln!(
                    "XGEN_INVOCATION_DIAGNOSTIC run_id={run_id} version=1 category={} field={}",
                    diagnostic.category(),
                    diagnostic.field()
                );
            }
            ExitCode::from(20)
        }
        Ok(LocalCommandResult::RecoveryRequired { run_id, reason }) => {
            eprintln!(
                "XGEN_RECOVERY_REQUIRED run_id={run_id} reason={}",
                reason.code()
            );
            ExitCode::from(30)
        }
        Err(error) => {
            eprintln!("XGEN_ERROR code={}", error.code());
            ExitCode::from(error.exit_code())
        }
    }
}

#[cfg(test)]
mod onboarding_tests {
    use super::*;

    struct UnavailableCredentials(ModelProfileError);

    impl ModelCredentialStore for UnavailableCredentials {
        fn get(&self, _: &str) -> Result<Zeroizing<String>, ModelProfileError> {
            Err(self.0)
        }
        fn put(&self, _: &str, _: &str) -> Result<(), ModelProfileError> {
            Err(self.0)
        }
        fn delete(&self, _: &str) -> Result<(), ModelProfileError> {
            Err(self.0)
        }
    }

    #[test]
    fn unavailable_store_falls_back_only_for_interactive_ephemeral_keys() {
        let endpoint = "https://session.example/v1";
        let credentials = UnavailableCredentials(ModelProfileError::CredentialStoreUnavailable);
        assert!(
            !store_setup_credential(&credentials, "unused", endpoint, "fixture-key", true).unwrap()
        );
        assert!(session_credential(endpoint).is_some());
        assert!(session_credential("https://other.example/v1").is_none());
        assert!(session_credential("https://session.example/v1/").is_none());
        assert!(session_credential("http://127.0.0.1/v1").is_none());
        assert!(
            store_setup_credential(&credentials, "unused", endpoint, "fixture-key", false).is_err()
        );
        let credentials = UnavailableCredentials(ModelProfileError::InvalidProfile);
        assert!(
            store_setup_credential(&credentials, "unused", endpoint, "fixture-key", true).is_err()
        );
        remember_session_credential("https://second.example/v1", "second-fixture-key");
        assert!(session_credential(endpoint).is_none());
        assert!(session_credential("https://second.example/v1").is_some());
        SESSION_CREDENTIAL.with(|slot| *slot.borrow_mut() = None);
    }
}
