use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use xgeny_cli::{
    LocalCommandResult, LocalResumeRequest, LocalRunRequest, PublicRunError, resume_local,
    run_local_with_started,
};

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
    /// Inspect and validate bundled protocol contracts.
    Protocol {
        #[command(subcommand)]
        command: ProtocolCommand,
    },
    /// Start a bounded local Run using one OpenAI-compatible model and read-text capability.
    Run(RunArgs),
    /// Continue an existing Run, or replay its durable completion without model access.
    Resume(ResumeArgs),
}

#[derive(Debug, Subcommand)]
enum ProtocolCommand {
    /// Run offline schema, fixture, round-trip, and digest checks.
    Check,
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Goal sent to the bounded planner.
    goal: String,
    /// Workspace root opened as the local filesystem capability.
    #[arg(long)]
    workspace: PathBuf,
    /// OpenAI-compatible API base URL ending in /v1.
    #[arg(long)]
    base_url: String,
    /// Served model identifier.
    #[arg(long)]
    model: String,
    /// Tokenizer/profile identifier committed for restart validation.
    #[arg(long)]
    tokenizer: String,
    /// Stable non-secret planner identity.
    #[arg(long, default_value = "xgeny.cli.openai")]
    planner_id: String,
    /// Relative workspace file the model may select; repeat for more files.
    #[arg(long = "allow-file", required = true)]
    allow_files: Vec<String>,
    /// Explicitly allow goal/context/tool output transfer to the remote model boundary.
    #[arg(long)]
    allow_remote_model_egress: bool,
    /// Explicitly approve one exact read selected from --allow-file.
    #[arg(long)]
    allow_read: bool,
    /// Bound work performed by this process invocation.
    #[arg(long, default_value_t = 32)]
    max_ticks: u32,
}

#[derive(Debug, Args)]
struct ResumeArgs {
    /// Durable Run identifier printed by `xgeny run`.
    run_id: String,
    /// Original physical workspace root; unnecessary for completed replay.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Current OpenAI-compatible base URL; unnecessary for completed replay.
    #[arg(long)]
    base_url: Option<String>,
    /// Same allow-file catalog supplied to the original Run.
    #[arg(long = "allow-file")]
    allow_files: Vec<String>,
    /// Allow a new remote model request to this invocation's supplied --base-url.
    #[arg(long)]
    allow_remote_model_egress: bool,
    /// Explicitly approve one exact read selected from --allow-file.
    #[arg(long)]
    allow_read: bool,
    /// Bound work performed by this process invocation.
    #[arg(long, default_value_t = 32)]
    max_ticks: u32,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
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
        Command::Run(args) => present(run_local_with_started(
            LocalRunRequest {
                goal: args.goal,
                workspace: args.workspace,
                base_url: args.base_url,
                planner_id: args.planner_id,
                model: args.model,
                tokenizer: args.tokenizer,
                allow_files: args.allow_files,
                allow_remote_model_egress: args.allow_remote_model_egress,
                allow_read: args.allow_read,
                max_ticks: args.max_ticks,
            },
            |run_id| eprintln!("XGENY_STARTED run_id={run_id}"),
        )),
        Command::Resume(args) => present(resume_local(LocalResumeRequest {
            run_id: args.run_id,
            workspace: args.workspace,
            base_url: args.base_url,
            allow_files: args.allow_files,
            allow_remote_model_egress: args.allow_remote_model_egress,
            allow_read: args.allow_read,
            max_ticks: args.max_ticks,
        })),
    }
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
