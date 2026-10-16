//! `zipc` — command-line front end for `zipcore`.

mod compress;
mod diff;
mod extract;
mod json;
mod list;
mod progress;
mod testcmd;
mod units;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

/// Exit status when the user interrupts a run (128 + SIGINT).
const EXIT_CANCELLED: u8 = 130;
/// Exit status of `zipc diff` when the comparison could not be made at all
/// (archive unreadable, directory missing), told apart from 1 ("differs").
const EXIT_TROUBLE: u8 = 3;

const PRESETS_HELP: &str = "\
Presets (zipc compress -p PRESET); every archive opens with the built-in unzip on any OS:
  fast      quickest; somewhat larger files (Deflate level 1)
  balanced  good size at high speed; the default (Deflate level 6)
  best      smaller Deflate, trying several encoder settings per file; slower
  max       smallest (Zopfli, typically 3-8% below best); MUCH slower (often 50-100x best)

Example: zipc compress photos/ notes.txt -o out.zip -p best";

const TOP_HELP: &str = "\
Presets (zipc compress -p PRESET); every archive opens with the built-in unzip on any OS:
  fast      quickest; somewhat larger files (Deflate level 1)
  balanced  good size at high speed; the default (Deflate level 6)
  best      smaller Deflate, trying several encoder settings per file; slower
  max       smallest (Zopfli, typically 3-8% below best); MUCH slower (often 50-100x best)

Examples:
  zipc compress photos/ notes.txt -o out.zip -p best
  zipc list -l out.zip
  zipc extract out.zip -d restored/
  zipc test out.zip
  zipc diff out.zip photos/

Exit status: 0 success, 1 failure, 2 usage error, 130 interrupted (Ctrl-C).
zipc diff: 0 identical, 1 differences found, 3 could not compare.";

const LIST_HELP: &str = "\
Examples:
  zipc list out.zip            one name per line
  zipc list -l out.zip         table with sizes, ratio, method and local modified time
  zipc list --json out.zip     one JSON object per archive, one per line";

const EXTRACT_HELP: &str = "\
Without -d, extracts into a folder next to the archive named after it without
`.zip` (photos.zip -> photos/). If that folder already exists zipc stops rather
than merging into it; pass -d, --skip-existing or --overwrite. With -d DIR the
folder may exist, and an existing file stops the extraction (before anything is
written) unless --skip-existing or --overwrite is given. Never prompts.

Examples:
  zipc extract photos.zip
  zipc extract photos.zip -d ~/Pictures --skip-existing
  zipc extract photos.zip 2024/ notes.txt";

const DIFF_HELP: &str = "\
Each differing path is printed with a marker, then a summary line:
  M  contents differ          m  only mode/mtime differ (--mode, --mtime)
  T  file vs directory/link   -  only in the archive    +  only on disk
With --all, matching paths are listed too, marked =.

Exit status: 0 identical, 1 differences found, 2 usage error,
3 could not compare (archive unreadable, DIR missing), 130 interrupted.

Examples:
  zipc diff photos.zip photos/
  zipc diff out.zip project/ --full --exclude target/ --exclude '*.log'
  zipc diff out.zip restored/ --mode --mtime --json";

#[derive(Debug, Parser)]
#[command(
    name = "zipc",
    version,
    about = "Fast, high-ratio ZIP compressor",
    after_help = TOP_HELP
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
    /// Check that archives are intact, like `unzip -t`: every entry is
    /// decompressed and its CRC-32 and sizes compared with the headers.
    /// Exit status 0 if all are OK, 1 if any has a problem or can't be read.
    Test(testcmd::TestArgs),
    /// List the entries of archives without extracting anything.
    /// Exit status 1 if any archive can't be read.
    #[command(after_help = LIST_HELP)]
    List(list::ListArgs),
    /// Extract an archive, checking every file's CRC-32 as it is written.
    /// Nothing is written outside the destination folder.
    #[command(after_help = EXTRACT_HELP)]
    Extract(extract::ExtractArgs),
    /// Compare an archive with a directory, like `diff -r`: which paths
    /// match, differ, or exist on one side only. Writes nothing.
    #[command(after_help = DIFF_HELP)]
    Diff(diff::DiffArgs),
}

/// How a subcommand finished, mapped to the process exit status.
pub enum Outcome {
    Ok,
    /// Finished, but found problems (already reported); exit status 1.
    Failed,
    /// The work could not be done at all (already reported); exit status 3.
    Trouble,
    Cancelled,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Compress(args) => compress::run(args),
        Command::Test(args) => testcmd::run(args),
        Command::List(args) => list::run(args),
        Command::Extract(args) => extract::run(args),
        Command::Diff(args) => diff::run(args),
    };
    match result {
        Ok(Outcome::Ok) => ExitCode::SUCCESS,
        Ok(Outcome::Failed) => ExitCode::FAILURE,
        Ok(Outcome::Trouble) => ExitCode::from(EXIT_TROUBLE),
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
