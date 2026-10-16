//! `zipc test`: check archives like `unzip -t`.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Result;
use clap::Args;
use zipcore::{Error, TestReport};

use crate::json::{self, Object, Value};
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

    /// Print one JSON object per archive, one per line.
    #[arg(long)]
    json: bool,
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
                if args.json {
                    println!("{}", report_json(&name, &report));
                } else {
                    print_report(&name, &report);
                }
                if !report.is_ok() {
                    failed += 1;
                }
            }
            Err(Error::Cancelled) => return Ok(Outcome::Cancelled),
            Err(e) => {
                failed += 1;
                if args.json {
                    println!("{}", error_json(&name, &e));
                } else {
                    println!("FAILED  {name}");
                }
                match e {
                    // Already names the file.
                    Error::Io { .. } => eprintln!("zipc: error: {e}"),
                    e => eprintln!("zipc: error: {name}: {e}"),
                }
            }
        }
    }
    if args.archives.len() > 1 && !args.json {
        let total = args.archives.len();
        println!("{} of {total} archives OK", total - failed);
    }
    Ok(if failed == 0 {
        Outcome::Ok
    } else {
        Outcome::Failed
    })
}

fn report_json(name: &str, r: &TestReport) -> String {
    let problems = json::array(r.problems.iter().map(|p| {
        Object::new()
            .field("entry", p.entry.as_deref())
            .field("message", p.message.as_str())
            .finish()
    }));
    Object::new()
        .field("archive", name)
        .field("ok", r.is_ok())
        .field("entries", r.entries)
        .field("bytes_checked", r.bytes_checked)
        .field("problems", Value::Raw(&problems))
        .field("elapsed_seconds", Value::F64(r.elapsed.as_secs_f64()))
        .finish()
}

fn error_json(name: &str, e: &Error) -> String {
    Object::new()
        .field("archive", name)
        .field("ok", false)
        .field("error", e.to_string().as_str())
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use zipcore::Problem;

    #[test]
    fn json_report() {
        let r = TestReport {
            entries: 2,
            bytes_checked: 10,
            problems: vec![
                Problem {
                    entry: Some("a\"b".into()),
                    message: "bad CRC-32".into(),
                },
                Problem {
                    entry: None,
                    message: "prepended data".into(),
                },
            ],
            elapsed: Duration::from_millis(500),
        };
        assert_eq!(
            report_json("x.zip", &r),
            r#"{"archive":"x.zip","ok":false,"entries":2,"bytes_checked":10,"problems":[{"entry":"a\"b","message":"bad CRC-32"},{"entry":null,"message":"prepended data"}],"elapsed_seconds":0.5}"#
        );
        assert_eq!(
            error_json("y.zip", &Error::InvalidArchive("no end record".into())),
            r#"{"archive":"y.zip","ok":false,"error":"not a valid ZIP archive: no end record"}"#
        );
    }
}
