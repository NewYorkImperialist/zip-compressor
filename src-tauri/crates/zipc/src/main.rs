//! `zipc` — command-line front end for `zipcore`.

mod compress;
mod units;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Exit status when the user interrupts a run (128 + SIGINT).
const EXIT_CANCELLED: u8 = 130;

#[derive(Debug, Parser)]
#[command(name = "zipc", version, about = "Fast, high-ratio ZIP compressor")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a ZIP archive from files and directories.
    Compress(compress::CompressArgs),
    // Later phases: Extract, Test, Bench.
}

/// How a subcommand finished, mapped to the process exit status.
pub enum Outcome {
    Ok,
    Cancelled,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Compress(args) => compress::run(args),
    };
    match result {
        Ok(Outcome::Ok) => ExitCode::SUCCESS,
        Ok(Outcome::Cancelled) => {
            eprintln!("cancelled");
            ExitCode::from(EXIT_CANCELLED)
        }
        Err(err) => {
            eprintln!("zipc: error: {err:#}");
            ExitCode::FAILURE
        }
    }
}
