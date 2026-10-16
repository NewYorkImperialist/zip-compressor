//! `zipc extract`.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Args;
use zipcore::{Error, ExtractOptions, ExtractSummary, Overwrite};

use crate::json::{Object, Value};
use crate::progress::{install_cancel_handler, ProgressLine};
use crate::units::{format_bytes, format_duration};
use crate::Outcome;

#[derive(Debug, Args)]
pub struct ExtractArgs {
    /// The ZIP archive to extract.
    #[arg(value_name = "ARCHIVE")]
    archive: PathBuf,

    /// Only extract these archive paths (a directory path takes everything
    /// under it), e.g. `docs/` or `docs/readme.txt`.
    #[arg(value_name = "PATHS")]
    paths: Vec<String>,

    /// Folder to extract into (created if missing). Default: a folder next to
    /// the archive named after it without `.zip`, which must not already exist
    /// unless --overwrite or --skip-existing is given.
    #[arg(short, long, value_name = "DIR")]
    dest: Option<PathBuf>,

    /// Replace files that already exist at the destination.
    #[arg(long, conflicts_with = "skip_existing")]
    overwrite: bool,

    /// Leave files that already exist alone and extract the rest.
    #[arg(long)]
    skip_existing: bool,

    /// Do not restore Unix permission bits from the archive.
    #[arg(long)]
    no_permissions: bool,

    /// Do not restore modification times (files get the current time).
    #[arg(long)]
    no_mtime: bool,

    /// Skip symbolic-link entries instead of creating links.
    #[arg(long)]
    no_symlinks: bool,

    /// Worker threads; 0 uses one per CPU core.
    #[arg(short = 'j', long, default_value_t = 0, value_name = "N")]
    threads: usize,

    /// Do not show progress.
    #[arg(short, long)]
    quiet: bool,

    /// Print the summary as one JSON object.
    #[arg(long)]
    json: bool,
}

impl ExtractArgs {
    fn overwrite(&self) -> Overwrite {
        if self.overwrite {
            Overwrite::Always
        } else if self.skip_existing {
            Overwrite::Skip
        } else {
            Overwrite::Never
        }
    }

    fn options(&self) -> ExtractOptions {
        ExtractOptions {
            overwrite: self.overwrite(),
            restore_permissions: !self.no_permissions,
            restore_mtime: !self.no_mtime,
            symlinks: !self.no_symlinks,
            only: self.paths.clone(),
            threads: self.threads,
        }
    }
}

/// The default destination: the archive's name without `.zip` (any case),
/// next to it, as Archive Utility and Explorer do. An archive with some other
/// extension loses that extension; one with none gets `-extracted` appended
/// so the folder never collides with the archive itself.
pub fn default_dest(archive: &Path) -> PathBuf {
    let parent = archive
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = archive.file_name().unwrap_or(archive.as_os_str());
    let has_ext = archive.extension().is_some();
    let stem = match archive.file_stem() {
        Some(stem) if has_ext && !stem.is_empty() => stem.to_os_string(),
        _ => {
            let mut s = name.to_os_string();
            s.push("-extracted");
            s
        }
    };
    parent.join(stem)
}

pub fn run(args: ExtractArgs) -> Result<Outcome> {
    let meta = std::fs::metadata(&args.archive)
        .with_context(|| format!("archive not found: {}", args.archive.display()))?;
    if !meta.is_file() {
        bail!("{} is not a file", args.archive.display());
    }
    let dest = match &args.dest {
        Some(d) => d.clone(),
        None => {
            let d = default_dest(&args.archive);
            // Never merge into an existing folder by accident.
            if args.overwrite() == Overwrite::Never && std::fs::symlink_metadata(&d).is_ok() {
                bail!(
                    "{} already exists; choose another folder with -d DIR, or pass \
                     --skip-existing or --overwrite to extract into it",
                    d.display()
                );
            }
            d
        }
    };

    let cancel = install_cancel_handler()?;
    let show_progress = !args.quiet && std::io::stderr().is_terminal();
    let mut bar = ProgressLine::new(show_progress, "files");
    let result = zipcore::extract_archive(
        &args.archive,
        &dest,
        &args.options(),
        &mut |p| bar.update(p),
        &cancel,
    );
    bar.finish();

    match result {
        Ok(summary) => {
            if args.json {
                println!("{}", summary_json(&args.archive, &dest, &summary));
            } else {
                println!("{}", summary_line(&dest, &summary));
            }
            Ok(Outcome::Ok)
        }
        Err(Error::Cancelled) => Ok(Outcome::Cancelled),
        // Already name the file; the destination is what matters.
        Err(e @ (Error::AlreadyExists(_) | Error::Io { .. })) => {
            Err(e).with_context(|| format!("extracting into {}", dest.display()))
        }
        Err(e) => Err(e).with_context(|| format!("extracting {}", args.archive.display())),
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// e.g. `3 files, 1 dir, 0 symlinks -> out/, 1.2 MiB in 15 ms`.
fn summary_line(dest: &Path, s: &ExtractSummary) -> String {
    let skipped = if s.skipped > 0 {
        format!(", {} skipped", s.skipped)
    } else {
        String::new()
    };
    format!(
        "{}, {}, {}{skipped} -> {}, {} in {}",
        plural(s.files, "file", "files"),
        plural(s.dirs, "dir", "dirs"),
        plural(s.symlinks, "symlink", "symlinks"),
        dest.display(),
        format_bytes(s.bytes_out),
        format_duration(s.elapsed)
    )
}

fn summary_json(archive: &Path, dest: &Path, s: &ExtractSummary) -> String {
    Object::new()
        .field("archive", archive.to_string_lossy().as_ref())
        .field("dest", dest.to_string_lossy().as_ref())
        .field("files", s.files)
        .field("dirs", s.dirs)
        .field("symlinks", s.symlinks)
        .field("skipped", s.skipped)
        .field("bytes_out", s.bytes_out)
        .field("elapsed_seconds", Value::F64(s.elapsed.as_secs_f64()))
        .finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn default_destination() {
        let d = |s: &str| default_dest(Path::new(s));
        assert_eq!(d("photos.zip"), Path::new("./photos"));
        assert_eq!(d("dir/Photos.ZIP"), Path::new("dir/Photos"));
        assert_eq!(d("/a/b/x.tar.zip"), Path::new("/a/b/x.tar"));
        assert_eq!(d("/a/data.bin"), Path::new("/a/data"));
        assert_eq!(d("/a/archive"), Path::new("/a/archive-extracted"));
        assert_eq!(d("/a/.zip"), Path::new("/a/.zip-extracted"));
    }

    #[test]
    fn summary_formats() {
        let mut s = ExtractSummary {
            files: 1,
            dirs: 2,
            symlinks: 0,
            skipped: 0,
            bytes_out: 2048,
            elapsed: Duration::from_millis(15),
        };
        assert_eq!(
            summary_line(Path::new("out"), &s),
            "1 file, 2 dirs, 0 symlinks -> out, 2.0 KiB in 15 ms"
        );
        s.skipped = 3;
        assert!(summary_line(Path::new("out"), &s).contains("0 symlinks, 3 skipped -> out"));
        assert_eq!(
            summary_json(Path::new("a\".zip"), Path::new("out"), &s),
            r#"{"archive":"a\".zip","dest":"out","files":1,"dirs":2,"symlinks":0,"skipped":3,"bytes_out":2048,"elapsed_seconds":0.015}"#
        );
    }
}
