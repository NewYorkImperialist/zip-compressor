//! `zipc test`: check archives like `unzip -t`.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use zipcore::{Error, TestReport};

use crate::progress::{install_cancel_handler, ProgressLine};
use crate::units::format_bytes;
use crate::Outcome;

#[derive(Debug, Args)]
pub struct TestArgs {
    /// ZIP archives to check.
    #[arg(required = true, value_name = "ARCHIVE")]
    archives: Vec<PathBuf>,

    /// Do not show progress.
    #[arg(short, long)]
    quiet: bool,
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn print_report(name: &str, r: &TestReport) {
    let entries = plural(r.entries, "entry", "entries");
    if r.is_ok() {
        println!(
            "OK  {name}  ({entries}, {} checked)",
            format_bytes(r.bytes_checked)
        );
        return;
    }
    let n = plural(r.problems.len() as u64, "problem", "problems");
    println!("FAILED  {name}  ({entries}, {n})");
    for p in &r.problems {
        println!(
            "  {}: {}",
            p.entry.as_deref().unwrap_or("(archive)"),
            p.message
        );
    }
}

pub fn run(args: TestArgs) -> Result<Outcome> {
    let cancel = install_cancel_handler()?;
    let show_progress = !args.quiet && std::io::stderr().is_terminal();
    let mut failed = 0usize;
    for path in &args.archives {
        let name = path.display().to_string();
        let mut bar = ProgressLine::new(show_progress, "entries");
        let result = zipcore::test_archive(path, &mut |p| bar.update(p), &cancel);
        bar.finish();
        match result {
            Ok(report) => {
                print_report(&name, &report);
                if !report.is_ok() {
                    failed += 1;
                }
            }
            Err(Error::Cancelled) => return Ok(Outcome::Cancelled),
            Err(e) => {
                failed += 1;
                println!("FAILED  {name}");
                match e {
                    // Already names the file.
                    Error::Io { .. } => eprintln!("zipc: error: {e}"),
                    e => eprintln!("zipc: error: {name}: {e}"),
                }
            }
        }
    }
    if args.archives.len() > 1 {
        let total = args.archives.len();
        println!("{} of {total} archives OK", total - failed);
    }
    Ok(if failed == 0 {
        Outcome::Ok
    } else {
        Outcome::Failed
    })
}
