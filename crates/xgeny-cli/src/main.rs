use std::process::ExitCode;

use clap::{Parser, Subcommand};

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
}

#[derive(Debug, Subcommand)]
enum ProtocolCommand {
    /// Run offline schema, fixture, round-trip, and digest checks.
    Check,
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
    }
}
