//! `zipc` — command-line front end for `zipcore`.

mod compress;
mod units;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Exit status when the user interrupts a run (128 + SIGINT).
const EXIT_CANCELLED: u8 = 130;

const PRESETS_HELP: &str = "\
Presets (zipc compress -p PRESET); every archive opens with the built-in unzip on any OS:
  fast      quickest; somewhat larger files (Deflate level 1)
  balanced  good size at high speed; the default (Deflate level 6)
  best      smaller Deflate, trying several encoder settings per file; slower
  max       smallest (Zopfli, typically 3-8% below best); MUCH slower (often 50-100x best)

Example: zipc compress photos/ notes.txt -o out.zip -p best";

#[derive(Debug, Parser)]
#[command(
    name = "zipc",
    version,
    about = "Fast, high-ratio ZIP compressor",
    after_help = PRESETS_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a ZIP archive from files and directories.
    #[command(after_help = PRESETS_HELP)]
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
