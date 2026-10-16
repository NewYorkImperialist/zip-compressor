//! `zipc list`: show what an archive contains without extracting it.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Args;
use zipcore::{EntryInfo, EntryKind, Error, Listing};

use crate::json::{self, Object, Value};
use crate::units::{format_bytes, format_local, format_mode, format_rfc3339_utc, ratio_percent};
use crate::Outcome;

#[derive(Debug, Args)]
pub struct ListArgs {
    /// ZIP archives to list.
    #[arg(required = true, value_name = "ARCHIVE")]
    archives: Vec<PathBuf>,

    /// Show a table: size, compressed size, ratio, method, modified time
    /// (local), name; then a total line.
    #[arg(short, long, conflicts_with = "json")]
    long: bool,

    /// Print one JSON object per archive, one per line (a single archive gives
    /// a single JSON document).
    #[arg(long)]
    json: bool,
}

pub fn run(args: ListArgs) -> Result<Outcome> {
    let mut failed = false;
    match list_all(&args, &mut failed) {
        // `zipc list big.zip | head`: the reader went away; not an error.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
        r => r?,
    }
    Ok(if failed { Outcome::Failed } else { Outcome::Ok })
}

fn list_all(args: &ListArgs, failed: &mut bool) -> std::io::Result<()> {
    let many = args.archives.len() > 1;
    let mut out = std::io::stdout().lock();
    for (i, path) in args.archives.iter().enumerate() {
        let listing = match zipcore::list_archive(path) {
            Ok(l) => l,
            Err(e) => {
                *failed = true;
                report_error(path, &e);
                continue;
            }
        };
        if args.json {
            writeln!(out, "{}", to_json(path, &listing))?;
            continue;
        }
        for p in &listing.problems {
            eprintln!("zipc: warning: {}: {p}", path.display());
        }
        // Like `ls a b`: a heading per archive only when there are several.
        if many {
            if i > 0 {
                writeln!(out)?;
            }
            writeln!(out, "{}:", path.display())?;
        }
        if args.long {
            write_long(&mut out, &listing)?;
        } else {
            for e in &listing.entries {
                writeln!(out, "{}", e.name)?;
            }
        }
    }
    out.flush()
}

fn report_error(path: &Path, e: &Error) {
    match e {
        // Already names the file.
        Error::Io { .. } => eprintln!("zipc: error: {e}"),
        e => eprintln!("zipc: error: {}: {e}", path.display()),
    }
}

/// The method column: names for the two methods everyone uses, else the number.
fn method_name(m: u16) -> String {
    match m {
        0 => "Stored".into(),
        8 => "Deflate".into(),
        n => n.to_string(),
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn kind_name(k: EntryKind) -> &'static str {
    match k {
        EntryKind::File => "file",
        EntryKind::Dir => "dir",
        EntryKind::Symlink => "symlink",
    }
}

fn ratio(uncompressed: u64, compressed: u64) -> String {
    ratio_percent(uncompressed, compressed).map_or_else(|| "-".into(), |r| format!("{r:.1}%"))
}

fn write_long(out: &mut impl Write, l: &Listing) -> std::io::Result<()> {
    writeln!(
        out,
        "{:>10}  {:>10}  {:>6}  {:<8}  {:<19}  Name",
        "Size", "Compressed", "Ratio", "Method", "Modified"
    )?;
    let (mut size, mut packed) = (0u64, 0u64);
    let (mut files, mut dirs, mut links) = (0u64, 0u64, 0u64);
    let mut any_encrypted = false;
    for e in &l.entries {
        size = size.saturating_add(e.uncompressed);
        packed = packed.saturating_add(e.compressed);
        match e.kind {
            EntryKind::File => files += 1,
            EntryKind::Dir => dirs += 1,
            EntryKind::Symlink => links += 1,
        }
        any_encrypted |= e.encrypted;
        writeln!(out, "{}", long_row(e))?;
    }
    writeln!(
        out,
        "{:>10}  {:>10}  {:>6}  {}, {}, {}",
        format_bytes(size),
        format_bytes(packed),
        ratio(size, packed),
        plural(files, "file", "files"),
        plural(dirs, "dir", "dirs"),
        plural(links, "symlink", "symlinks"),
    )?;
    if any_encrypted {
        writeln!(out, "* encrypted")?;
    }
    Ok(())
}

fn long_row(e: &EntryInfo) -> String {
    let ratio = if e.kind == EntryKind::Dir {
        "-".into()
    } else {
        ratio(e.uncompressed, e.compressed)
    };
    let method = format!(
        "{}{}",
        method_name(e.method),
        if e.encrypted { "*" } else { "" }
    );
    let modified = e
        .modified
        .map_or_else(|| "-".into(), |t| format_local(zipcore::local_time(t)));
    format!(
        "{:>10}  {:>10}  {:>6}  {:<8}  {:<19}  {}",
        format_bytes(e.uncompressed),
        format_bytes(e.compressed),
        ratio,
        method,
        modified,
        e.name
    )
}

fn to_json(path: &Path, l: &Listing) -> String {
    let entries = json::array(l.entries.iter().map(entry_json));
    let problems = json::array(l.problems.iter().map(|p| {
        let mut s = String::new();
        json::write_str(&mut s, p);
        s
    }));
    Object::new()
        .field("archive", path.to_string_lossy().as_ref())
        .field("archive_bytes", l.archive_bytes)
        .field("entries", Value::Raw(&entries))
        .field("problems", Value::Raw(&problems))
        .finish()
}

fn entry_json(e: &EntryInfo) -> String {
    let modified = e.modified.map(format_rfc3339_utc);
    let mode = e.unix_mode.map(format_mode);
    Object::new()
        .field("name", e.name.as_str())
        .field("kind", kind_name(e.kind))
        .field("method", u64::from(e.method))
        .field("compressed", e.compressed)
        .field("uncompressed", e.uncompressed)
        .field("crc32", u64::from(e.crc32))
        .field("modified", modified.as_deref())
        .field("unix_mode", mode.as_deref())
        .field("encrypted", e.encrypted)
        .finish()
}
