//! `zipc diff`: compare an archive with a directory, like `diff -r`.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use zipcore::{DiffEntry, DiffKind, DiffOptions, DiffReport, Error};

use crate::json::{self, Object, Value};
use crate::progress::{install_cancel_handler, ProgressLine};
use crate::Outcome;

#[derive(Debug, Args)]
pub struct DiffArgs {
    /// The ZIP archive.
    #[arg(value_name = "ARCHIVE")]
    archive: PathBuf,

    /// The directory to compare it with.
    #[arg(value_name = "DIR")]
    dir: PathBuf,

    /// Decompress every file and compare bytes, instead of comparing sizes
    /// and the disk file's CRC-32 with the archive's.
    #[arg(long)]
    full: bool,

    /// Also compare Unix permission bits (when the archive has them).
    #[arg(long)]
    mode: bool,

    /// Also compare modification times (within 2 seconds unless the archive
    /// stores exact times, as zipc's archives do).
    #[arg(long)]
    mtime: bool,

    /// Archive folder that corresponds to DIR. By default, DIR's own name
    /// when every entry is under it (what `zipc compress DIR` makes),
    /// otherwise the archive root. `--prefix ""` forces the root.
    #[arg(long, value_name = "P")]
    prefix: Option<String>,

    /// Ignore paths matching this gitignore-style pattern on both sides
    /// (`*.log`, `build/`, `/target`). Repeatable.
    #[arg(short = 'x', long, value_name = "PATTERN")]
    exclude: Vec<String>,

    /// Compare `.DS_Store`, `__MACOSX/`, `._*`, `Thumbs.db` and `desktop.ini`
    /// too (ignored by default).
    #[arg(long)]
    keep_junk: bool,

    /// Also list paths that are the same.
    #[arg(short, long)]
    all: bool,

    /// Worker threads; 0 uses one per CPU core.
    #[arg(short = 'j', long, default_value_t = 0, value_name = "N")]
    threads: usize,

    /// Do not show progress.
    #[arg(short, long)]
    quiet: bool,

    /// Print the result as one JSON object, listing every path.
    #[arg(long)]
    json: bool,
}

pub fn run(args: DiffArgs) -> Result<Outcome> {
    // Any failure here means "could not compare", never "differs" (1).
    run_inner(args).or_else(|e| {
        eprintln!("zipc: error: {e:#}");
        Ok(Outcome::Trouble)
    })
}

fn run_inner(args: DiffArgs) -> Result<Outcome> {
    let cancel = install_cancel_handler()?;
    let opts = DiffOptions {
        prefix: args.prefix.clone(),
        full: args.full,
        compare_mode: args.mode,
        compare_mtime: args.mtime,
        exclude_junk: !args.keep_junk,
        exclude: args.exclude.clone(),
        threads: args.threads,
    };
    let show_progress = !args.quiet && std::io::stderr().is_terminal();
    let mut bar = ProgressLine::new(show_progress, "paths");
    let result = zipcore::diff_archive(
        &args.archive,
        &args.dir,
        &opts,
        &mut |p| bar.update(p),
        &cancel,
    );
    bar.finish();
    let report = match result {
        Ok(r) => r,
        Err(Error::Cancelled) => return Ok(Outcome::Cancelled),
        Err(e) => {
            if args.json {
                println!("{}", error_json(&args.archive, &args.dir, &e));
            }
            match e {
                // Already names the file.
                Error::Io { .. } | Error::NotFound(_) => eprintln!("zipc: error: {e}"),
                e => eprintln!("zipc: error: {}: {e}", args.archive.display()),
            }
            return Ok(Outcome::Trouble);
        }
    };
    let written = if args.json {
        writeln!(
            std::io::stdout().lock(),
            "{}",
            report_json(&args.archive, &args.dir, &report)
        )
    } else {
        write_human(&mut std::io::stdout().lock(), &report, args.all)
    };
    match written {
        // `zipc diff a.zip dir | head`: the reader went away; not an error.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        r => r?,
    }
    Ok(if report.is_identical() {
        Outcome::Ok
    } else {
        Outcome::Failed
    })
}

/// The one-character marker for a kind of difference.
fn marker(k: DiffKind) -> char {
    match k {
        DiffKind::Same => '=',
        DiffKind::Different => 'M',
        DiffKind::MetadataDiffers => 'm',
        DiffKind::TypeMismatch => 'T',
        DiffKind::OnlyInArchive => '-',
        DiffKind::OnlyOnDisk => '+',
    }
}

/// Every kind, in summary order.
const KINDS: [DiffKind; 6] = [
    DiffKind::Same,
    DiffKind::Different,
    DiffKind::MetadataDiffers,
    DiffKind::TypeMismatch,
    DiffKind::OnlyInArchive,
    DiffKind::OnlyOnDisk,
];

/// The kind's name in JSON and (with spaces) in the summary line.
fn kind_name(k: DiffKind) -> &'static str {
    match k {
        DiffKind::Same => "same",
        DiffKind::Different => "different",
        DiffKind::MetadataDiffers => "metadata_differs",
        DiffKind::TypeMismatch => "type_mismatch",
        DiffKind::OnlyInArchive => "only_in_archive",
        DiffKind::OnlyOnDisk => "only_on_disk",
    }
}

fn count(r: &DiffReport, k: DiffKind) -> u64 {
    r.entries.iter().filter(|e| e.kind == k).count() as u64
}

fn line(e: &DiffEntry) -> String {
    match &e.detail {
        Some(d) => format!("{} {}  ({d})", marker(e.kind), e.path),
        None => format!("{} {}", marker(e.kind), e.path),
    }
}

/// The summary: "3 same, 1 different — differences found". Kinds other than
/// "same" appear only when present.
fn summary(r: &DiffReport) -> String {
    let parts: Vec<String> = KINDS
        .iter()
        .map(|&k| (k, count(r, k)))
        .filter(|&(k, n)| k == DiffKind::Same || n > 0)
        .map(|(k, n)| format!("{n} {}", kind_name(k).replace('_', " ")))
        .collect();
    let verdict = if r.is_identical() {
        "identical"
    } else {
        "differences found"
    };
    format!("{} — {verdict}", parts.join(", "))
}

fn write_human(out: &mut impl Write, r: &DiffReport, all: bool) -> std::io::Result<()> {
    for e in &r.entries {
        if all || e.kind != DiffKind::Same {
            writeln!(out, "{}", line(e))?;
        }
    }
    writeln!(out, "{}", summary(r))?;
    out.flush()
}

fn report_json(archive: &Path, dir: &Path, r: &DiffReport) -> String {
    let mut counts = Object::new();
    for k in KINDS {
        counts = counts.field(kind_name(k), count(r, k));
    }
    let counts = counts.finish();
    let entries = json::array(r.entries.iter().map(|e| {
        Object::new()
            .field("path", e.path.as_str())
            .field("kind", kind_name(e.kind))
            .field("detail", e.detail.as_deref())
            .finish()
    }));
    Object::new()
        .field("archive", archive.to_string_lossy().as_ref())
        .field("dir", dir.to_string_lossy().as_ref())
        .field("prefix", r.prefix.as_str())
        .field("identical", r.is_identical())
        .field("counts", Value::Raw(&counts))
        .field("entries", Value::Raw(&entries))
        .field("elapsed_seconds", Value::F64(r.elapsed.as_secs_f64()))
        .finish()
}

fn error_json(archive: &Path, dir: &Path, e: &Error) -> String {
    Object::new()
        .field("archive", archive.to_string_lossy().as_ref())
        .field("dir", dir.to_string_lossy().as_ref())
        .field("error", e.to_string().as_str())
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn report(kinds: &[(&str, DiffKind, Option<&str>)]) -> DiffReport {
        DiffReport {
            entries: kinds
                .iter()
                .map(|&(path, kind, detail)| DiffEntry {
                    path: path.into(),
                    kind,
                    detail: detail.map(String::from),
                })
                .collect(),
            prefix: "data/".into(),
            elapsed: Duration::from_millis(250),
        }
    }

    #[test]
    fn human_output() {
        let r = report(&[
            ("a.txt", DiffKind::Same, None),
            ("b.txt", DiffKind::Different, Some("size 1 vs 2")),
            ("c/", DiffKind::OnlyOnDisk, None),
        ]);
        let mut out = Vec::new();
        write_human(&mut out, &r, false).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "M b.txt  (size 1 vs 2)\n+ c/\n1 same, 1 different, 1 only on disk — differences found\n"
        );
        let mut out = Vec::new();
        write_human(&mut out, &r, true).unwrap();
        assert!(String::from_utf8(out).unwrap().starts_with("= a.txt\n"));

        let same = report(&[("a.txt", DiffKind::Same, None)]);
        assert_eq!(summary(&same), "1 same — identical");
        assert_eq!(summary(&report(&[])), "0 same — identical");
    }

    #[test]
    fn json_output() {
        let r = report(&[("m", DiffKind::MetadataDiffers, Some("mode 0644 vs 0600"))]);
        assert_eq!(
            report_json(Path::new("a.zip"), Path::new("d"), &r),
            r#"{"archive":"a.zip","dir":"d","prefix":"data/","identical":false,"counts":{"same":0,"different":0,"metadata_differs":1,"type_mismatch":0,"only_in_archive":0,"only_on_disk":0},"entries":[{"path":"m","kind":"metadata_differs","detail":"mode 0644 vs 0600"}],"elapsed_seconds":0.25}"#
        );
    }
}
