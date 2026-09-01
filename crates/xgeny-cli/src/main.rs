use std::env;
use std::io::{BufRead as _, ErrorKind, IsTerminal as _, Read as _, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgGroup, Args, Parser, Subcommand};
use url::Url;
use xgeny_cli::{
    DriverProgress, DriverProgressControl, LocalCommandResult, LocalProcessSession,
    LocalResumeRequest, LocalRunRequest, ModelCheckError, ModelCheckRequest, ModelCredentialStore,
    ModelProfile, ModelProfileError, ModelProfileStore, OsModelCredentialStore, PublicRunError,
    check_openai_compatibility, check_openai_model, list_openai_models, new_credential_reference,
    prepare_local_process_session, resume_local, resume_local_with_model_resolver,
    resume_local_with_model_resolver_and_progress,
    resume_local_with_process_session_and_model_resolver_progress,
    run_local_with_process_session_progress, run_local_with_started,
};
use xgeny_provider_openai::BearerCredential;
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
    name = "xgeny",
    version,
    about = "Local-first general-purpose agent CLI"
)]
struct Cli {
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
    after_long_help = "Resolution order: explicit options, XGENY_OPENAI_BASE_URL / XGENY_OPENAI_MODEL / XGENY_OPENAI_TOKENIZER environment, then the selected/active profile. HTTPS authentication uses --token-stdin, XGENY_OPENAI_API_KEY, then the profile secure store; no token value is accepted as a command argument."
)]
struct ModelCheckArgs {
    /// OpenAI-compatible API base URL ending in /v1.
    #[arg(long)]
    base_url: Option<String>,
    /// Exact served model identifier expected in GET /v1/models.
    #[arg(long)]
    model: Option<String>,
    /// Tokenizer/profile identifier validated for a later run; defaults to the model ID.
    #[arg(long)]
    tokenizer: Option<String>,
    /// Named profile; defaults to `XGENY_MODEL_PROFILE` and then the active profile.
    #[arg(long)]
    profile: Option<String>,
    /// Read one API token line from standard input; the value is never persisted.
    #[arg(long)]
    token_stdin: bool,
    /// Also send one strict JSON Schema Chat Completions compatibility probe.
    #[arg(long)]
    compatibility: bool,
}

#[derive(Debug, Args)]
#[command(
    after_long_help = "Interactive setup hides token input and stores it only in the platform secure store. In automation, --token-stdin or XGENY_OPENAI_API_KEY is ephemeral unless --store-token is explicitly supplied."
)]
struct ModelSetupArgs {
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
    after_long_help = "Resolution order: explicit options, XGENY_OPENAI_BASE_URL / XGENY_OPENAI_MODEL / XGENY_OPENAI_TOKENIZER environment, then the selected/active profile. HTTPS authentication uses --token-stdin, XGENY_OPENAI_API_KEY, then the profile secure store. Credentials are ignored for loopback HTTP and cannot be passed as a command-line value."
)]
struct RunArgs {
    /// Goal sent to the bounded planner.
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
    /// Named model profile; defaults to `XGENY_MODEL_PROFILE` and then the active profile.
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
}

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)] // Four independent CLI consent switches.
#[command(
    after_long_help = "For an incomplete Run, endpoint resolution is explicit --base-url, XGENY_OPENAI_BASE_URL, then the selected/active profile. HTTPS authentication uses --token-stdin, XGENY_OPENAI_API_KEY, then the matching profile secure store. Credentials are ignored for loopback HTTP."
)]
struct ResumeArgs {
    /// Durable Run identifier printed by `xgeny run`.
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

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        None => interactive_command(),
        Some(Command::Licenses) => print_licenses(),
        Some(Command::Protocol {
            command: ProtocolCommand::Check,
        }) => match xgeny_protocol::check_bundled_protocol() {
            Ok(report) => {
                println!("XGENy protocol v0.1: PASS");
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
                eprintln!("XGENy protocol v0.1: FAIL");
                eprintln!("  {error}");
                ExitCode::FAILURE
            }
        },
        Some(Command::Model { command }) => run_model_command(command),
        Some(Command::Run(args)) => run_command(args),
        Some(Command::Resume(args)) => resume_command(args),
    }
}

const REPL_MAX_TICKS: u32 = 32;

struct InteractiveHost {
    workspace: PathBuf,
    executable_specs: Vec<String>,
    executable_ids: Vec<String>,
    process_session: Option<LocalProcessSession>,
}

impl InteractiveHost {
    fn new() -> Result<Self, repl::ReplFailure> {
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
            .map(|profile| repl_model_view(&profile))
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
        let model = resolve_model(None, None, None, None, false)
            .map_err(|error| repl::ReplFailure::new(error.code()))?;
        let process_session = self.process_session()?;
        run_local_with_process_session_progress(
            LocalRunRequest {
                goal,
                workspace: self.workspace.clone(),
                base_url: model.base_url,
                planner_id: "xgeny.cli.openai".to_owned(),
                model: model.model,
                tokenizer: model.tokenizer,
                credential: model.credential,
                allow_files: Vec::new(),
                allow_dirs: vec![".".to_owned()],
                allow_executables: Vec::new(),
                allow_remote_model_egress: grants.model,
                allow_read: grants.read,
                allow_write: grants.write,
                allow_execute: grants.execute,
                max_ticks: REPL_MAX_TICKS,
            },
            &process_session,
            |run_id| eprintln!("XGENY_STARTED run_id={run_id}"),
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
                resolve_endpoint(None, None, false)
                    .map(|resolved| (resolved.base_url, resolved.credential))
                    .map_err(|error| {
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

fn interactive_command() -> ExitCode {
    let terminal = std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal();
    if terminal && let Err(error) = ensure_interactive_model() {
        return present_model_configuration_error(error);
    }

    let cancellation = repl::Cancellation::default();
    let signal_cancellation = cancellation.clone();
    if ctrlc::set_handler(move || signal_cancellation.request()).is_err() {
        eprintln!("XGENY_ERROR code={}", PublicRunError::Internal.code());
        return ExitCode::from(PublicRunError::Internal.exit_code());
    }
    let Ok(mut host) = InteractiveHost::new() else {
        eprintln!("XGENY_ERROR code={}", PublicRunError::Configuration.code());
        return ExitCode::from(PublicRunError::Configuration.exit_code());
    };
    let stdout = std::io::stdout();
    let mut input = repl::InterruptibleInput::stdin(cancellation.clone());
    let mut output = stdout.lock();
    match repl::run(&mut input, &mut output, &mut host, &cancellation) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(_) => {
            eprintln!("XGENY_ERROR code={}", PublicRunError::Internal.code());
            ExitCode::from(PublicRunError::Internal.exit_code())
        }
    }
}

fn ensure_interactive_model() -> Result<(), ModelCliError> {
    if select_profile(None)?.is_some() {
        return Ok(());
    }
    let (profile, stored) = try_model_setup(ModelSetupArgs {
        name: "default".to_owned(),
        base_url: None,
        model: None,
        tokenizer: None,
        token_stdin: false,
        store_token: false,
    })?;
    println!("XGENy model setup: PASS");
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
            allow_files: args.allow_files,
            allow_dirs: args.allow_dirs,
            allow_executables: args.allow_executables,
            allow_remote_model_egress: args.allow_remote_model_egress,
            allow_read: args.allow_read,
            allow_write: args.allow_write,
            allow_execute: args.allow_execute,
            max_ticks: args.max_ticks,
        },
        |run_id| eprintln!("XGENY_STARTED run_id={run_id}"),
    ))
}

fn resume_command(args: ResumeArgs) -> ExitCode {
    let ResumeArgs {
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
        resolve_endpoint(base_url, profile, token_stdin)
            .map(|resolved| (resolved.base_url, resolved.credential))
            .map_err(|error| {
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

const MAX_TOKEN_INPUT_BYTES: u64 = 16 * 1024;

struct ResolvedModel {
    base_url: String,
    model: String,
    tokenizer: String,
    credential: Option<BearerCredential>,
}

struct ResolvedEndpoint {
    base_url: String,
    credential: Option<BearerCredential>,
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
            | Self::CredentialRequiresHttps => 64,
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
            println!("XGENy model setup: PASS");
            println!("  profile: {}", profile.name());
            println!("  model: {}", profile.model());
            println!("  catalog: exact model advertised");
            println!("  chat completions: strict JSON compatible");
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
fn try_model_setup(args: ModelSetupArgs) -> Result<(ModelProfile, bool), ModelCliError> {
    let store = ModelProfileStore::discover()?;
    let mut profiles = store.load()?;
    let existing = profiles.get(&args.name).cloned();
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let base_url = args
        .base_url
        .or(read_environment("XGENY_OPENAI_BASE_URL")?)
        .or_else(|| {
            existing
                .as_ref()
                .map(|profile| profile.base_url().to_owned())
        })
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
    let secret = resolve_setup_secret(&base_url, args.token_stdin, existing.as_ref(), interactive)?;
    if args.store_token && secret.value.is_none() {
        return Err(ModelCliError::InvalidCredential);
    }
    let credential =
        credential_from_secret(&base_url, secret.value.as_deref().map(String::as_str))?;
    let requested_model = args
        .model
        .or(read_environment("XGENY_OPENAI_MODEL")?)
        .or_else(|| existing.as_ref().map(|profile| profile.model().to_owned()));
    let catalog_identity = requested_model
        .clone()
        .unwrap_or_else(|| "xgeny-catalog-discovery".to_owned());
    let models = list_openai_models(ModelCheckRequest {
        base_url: base_url.clone(),
        model: catalog_identity.clone(),
        tokenizer: catalog_identity,
        credential: credential.clone(),
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
        .or(read_environment("XGENY_OPENAI_TOKENIZER")?)
        .or_else(|| {
            existing.as_ref().and_then(|profile| {
                (profile.model() == model).then(|| profile.tokenizer().to_owned())
            })
        })
        .unwrap_or_else(|| model.clone());

    check_openai_compatibility(ModelCheckRequest {
        base_url: base_url.clone(),
        model: model.clone(),
        tokenizer: tokenizer.clone(),
        credential,
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
    let mut profile = ModelProfile::new(&args.name, base_url, model, tokenizer)?;
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
        credentials.put(
            &reference,
            secret
                .value
                .as_deref()
                .ok_or(ModelCliError::InvalidCredential)?,
        )?;
        profile.set_credential_reference(Some(reference.clone()))?;
        new_reference = Some(reference);
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
            println!("No model profiles configured. Run `xgeny model setup`.");
            return Ok(());
        }
        for profile in profiles.iter() {
            let marker = if profiles.active_name() == Some(profile.name()) {
                "*"
            } else {
                " "
            };
            println!(
                "{marker} {} model={} tokenizer={} authentication={}",
                profile.name(),
                profile.model(),
                profile.tokenizer(),
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
            println!("XGENy active model profile: {name}");
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
            println!("XGENy model credential removed: {name}");
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
        println!("XGENy model profile removed: {name}");
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
    ) {
        Ok(resolved) => resolved,
        Err(error) => return present_model_command_error("check", error),
    };
    let request = ModelCheckRequest {
        base_url: resolved.base_url.clone(),
        model: resolved.model.clone(),
        tokenizer: resolved.tokenizer.clone(),
        credential: resolved.credential.clone(),
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
        })
    {
        return present_model_check_error(error);
    }
    println!("XGENy model check: PASS");
    println!("  model catalog: exact model advertised");
    println!(
        "  chat completions: {}",
        if args.compatibility {
            "strict JSON compatible"
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
) -> Result<ResolvedModel, ModelCliError> {
    let profile = select_profile(profile_name)?;
    let base_url = base_url
        .or(read_environment("XGENY_OPENAI_BASE_URL")?)
        .or_else(|| {
            profile
                .as_ref()
                .map(|profile| profile.base_url().to_owned())
        })
        .ok_or(ModelCliError::MissingConfiguration)?;
    let model = model
        .or(read_environment("XGENY_OPENAI_MODEL")?)
        .or_else(|| profile.as_ref().map(|profile| profile.model().to_owned()))
        .ok_or(ModelCliError::MissingConfiguration)?;
    let tokenizer = tokenizer
        .or(read_environment("XGENY_OPENAI_TOKENIZER")?)
        .or_else(|| {
            profile
                .as_ref()
                .map(|profile| profile.tokenizer().to_owned())
        })
        .unwrap_or_else(|| model.clone());
    let credential = resolve_credential(&base_url, token_stdin, profile.as_ref())?;
    Ok(ResolvedModel {
        base_url,
        model,
        tokenizer,
        credential,
    })
}

fn resolve_endpoint(
    base_url: Option<String>,
    profile_name: Option<String>,
    token_stdin: bool,
) -> Result<ResolvedEndpoint, ModelCliError> {
    let profile = select_profile(profile_name)?;
    let base_url = base_url
        .or(read_environment("XGENY_OPENAI_BASE_URL")?)
        .or_else(|| {
            profile
                .as_ref()
                .map(|profile| profile.base_url().to_owned())
        })
        .ok_or(ModelCliError::MissingConfiguration)?;
    let credential = resolve_credential(&base_url, token_stdin, profile.as_ref())?;
    Ok(ResolvedEndpoint {
        base_url,
        credential,
    })
}

fn select_profile(name: Option<String>) -> Result<Option<ModelProfile>, ModelCliError> {
    let requested = name.or(read_environment("XGENY_MODEL_PROFILE")?);
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
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(ModelCliError::InvalidEnvironment),
    }
}

fn read_secret_environment() -> Result<Option<Zeroizing<String>>, ModelCliError> {
    read_environment("XGENY_OPENAI_API_KEY").map(|value| value.map(Zeroizing::new))
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
    eprintln!("XGENy model {command}: FAIL");
    eprintln!("  reason={}", error.code());
    ExitCode::from(error.exit_code())
}

fn present_model_configuration_error(error: ModelCliError) -> ExitCode {
    eprintln!("XGENY_ERROR code={}", error.code());
    ExitCode::from(error.exit_code())
}

fn present_model_check_error(error: ModelCheckError) -> ExitCode {
    eprintln!("XGENy model check: FAIL");
    eprintln!("  reason={}", error.code());
    ExitCode::from(error.exit_code())
}

fn print_licenses() -> ExitCode {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    for (title, contents) in [
        ("XGENy project license", PROJECT_LICENSE),
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
            eprintln!("XGENY_ERROR code={}", PublicRunError::Internal.code());
            return ExitCode::from(PublicRunError::Internal.exit_code());
        }
    }
    ExitCode::SUCCESS
}

fn present(result: Result<LocalCommandResult, PublicRunError>) -> ExitCode {
    match result {
        Ok(LocalCommandResult::Completed { run_id, summary }) => {
            eprintln!("XGENY_COMPLETED run_id={run_id}");
            if std::io::stdout().write_all(summary.as_bytes()).is_err() {
                eprintln!("XGENY_ERROR code={}", PublicRunError::Internal.code());
                return ExitCode::from(PublicRunError::Internal.exit_code());
            }
            ExitCode::SUCCESS
        }
        Ok(LocalCommandResult::Paused { run_id, reason }) => {
            if let Some(run_id) = run_id {
                eprintln!("XGENY_PAUSED run_id={run_id} reason={}", reason.code());
            } else {
                eprintln!("XGENY_PAUSED reason={}", reason.code());
            }
            ExitCode::from(10)
        }
        Ok(LocalCommandResult::Rejected { run_id, reason }) => {
            eprintln!("XGENY_REJECTED run_id={run_id} reason={}", reason.code());
            ExitCode::from(20)
        }
        Ok(LocalCommandResult::RecoveryRequired { run_id, reason }) => {
            eprintln!(
                "XGENY_RECOVERY_REQUIRED run_id={run_id} reason={}",
                reason.code()
            );
            ExitCode::from(30)
        }
        Err(error) => {
            eprintln!("XGENY_ERROR code={}", error.code());
            ExitCode::from(error.exit_code())
        }
    }
}
