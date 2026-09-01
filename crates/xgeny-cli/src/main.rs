use std::io::{ErrorKind, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{ArgGroup, Args, Parser, Subcommand};
use xgeny_cli::{
    LocalCommandResult, LocalResumeRequest, LocalRunRequest, ModelCheckError, ModelCheckRequest,
    PublicRunError, check_openai_model, resume_local, run_local_with_started,
};

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
    command: Command,
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
    /// Check access to an OpenAI-compatible model catalog without creating Run state.
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
    /// Check catalog access and exact model advertisement with one GET.
    Check(ModelCheckArgs),
}

#[derive(Debug, Args)]
#[command(
    after_long_help = "HTTPS authentication: set XGENY_OPENAI_API_KEY. There is no --api-key option; inject the value through a secret manager/current process environment and do not type the literal into shell history."
)]
struct ModelCheckArgs {
    /// OpenAI-compatible API base URL ending in /v1.
    #[arg(long, env = "XGENY_OPENAI_BASE_URL", hide_env_values = true)]
    base_url: String,
    /// Exact served model identifier expected in GET /v1/models.
    #[arg(long, env = "XGENY_OPENAI_MODEL", hide_env_values = true)]
    model: String,
    /// Tokenizer/profile identifier validated for a later run; defaults to the model ID.
    #[arg(long, env = "XGENY_OPENAI_TOKENIZER", hide_env_values = true)]
    tokenizer: Option<String>,
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
    after_long_help = "HTTPS authentication: set XGENY_OPENAI_API_KEY. Credentials are ignored for loopback HTTP and cannot be passed as a command-line argument."
)]
struct RunArgs {
    /// Goal sent to the bounded planner.
    goal: String,
    /// Workspace root opened as the local filesystem capability.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    /// OpenAI-compatible API base URL ending in /v1.
    #[arg(long, env = "XGENY_OPENAI_BASE_URL", hide_env_values = true)]
    base_url: String,
    /// Served model identifier.
    #[arg(long, env = "XGENY_OPENAI_MODEL", hide_env_values = true)]
    model: String,
    /// Tokenizer/profile identifier committed for restart validation; defaults to the model ID.
    #[arg(long, env = "XGENY_OPENAI_TOKENIZER", hide_env_values = true)]
    tokenizer: Option<String>,
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
    after_long_help = "HTTPS authentication for an incomplete Run: set XGENY_OPENAI_API_KEY. Credentials are ignored for loopback HTTP."
)]
struct ResumeArgs {
    /// Durable Run identifier printed by `xgeny run`.
    run_id: String,
    /// Original physical workspace root; unnecessary for completed replay.
    #[arg(long, default_value = ".")]
    workspace: Option<PathBuf>,
    /// Current OpenAI-compatible base URL; unnecessary for completed replay.
    #[arg(long, env = "XGENY_OPENAI_BASE_URL", hide_env_values = true)]
    base_url: Option<String>,
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
        Command::Licenses => print_licenses(),
        Command::Protocol {
            command: ProtocolCommand::Check,
        } => match xgeny_protocol::check_bundled_protocol() {
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
        Command::Model {
            command: ModelCommand::Check(args),
        } => {
            let tokenizer = args.tokenizer.unwrap_or_else(|| args.model.clone());
            match check_openai_model(ModelCheckRequest {
                base_url: args.base_url,
                model: args.model,
                tokenizer,
            }) {
                Ok(()) => {
                    println!("XGENy model check: PASS");
                    println!("  model catalog: exact model advertised");
                    println!("  inference requests: 0");
                    ExitCode::SUCCESS
                }
                Err(error) => present_model_check_error(error),
            }
        }
        Command::Run(args) => {
            let tokenizer = args.tokenizer.unwrap_or_else(|| args.model.clone());
            present(run_local_with_started(
                LocalRunRequest {
                    goal: args.goal,
                    workspace: args.workspace,
                    base_url: args.base_url,
                    planner_id: args.planner_id,
                    model: args.model,
                    tokenizer,
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
        Command::Resume(args) => present(resume_local(LocalResumeRequest {
            run_id: args.run_id,
            workspace: args.workspace,
            base_url: args.base_url,
            allow_files: args.allow_files,
            allow_dirs: args.allow_dirs,
            allow_executables: args.allow_executables,
            allow_remote_model_egress: args.allow_remote_model_egress,
            allow_read: args.allow_read,
            allow_write: args.allow_write,
            allow_execute: args.allow_execute,
            max_ticks: args.max_ticks,
        })),
    }
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
