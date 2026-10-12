//! `compress_to_path`: input/output validation and atomic temp-file + rename.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use crate::writer;
use crate::{walk, CompressOptions, Error, Progress, Result, Summary};

fn io_err(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Directory the output goes in (`.` for a bare file name) and its file name.
fn split_output(out_path: &Path) -> Result<(PathBuf, std::ffi::OsString)> {
    let name = out_path.file_name().ok_or_else(|| {
        io_err(
            out_path,
            io::Error::new(io::ErrorKind::InvalidInput, "output path has no file name"),
        )
    })?;
    let dir = match out_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    Ok((dir, name.to_os_string()))
}

/// Refuse when the (canonical) output location lies inside an input directory.
/// The output may not exist yet, so its parent is canonicalized instead.
fn check_output_location(inputs: &[PathBuf], out_dir: &Path, out_path: &Path) -> Result<()> {
    let out_dir_canon = out_dir.canonicalize().map_err(|e| io_err(out_dir, e))?;
    for input in inputs {
        let canon = match input.canonicalize() {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::NotFound(input.clone()))
            }
            Err(e) => return Err(io_err(input, e)),
        };
        if canon.is_dir() && out_dir_canon.starts_with(&canon) {
            return Err(Error::OutputInsideInput(out_path.to_path_buf()));
        }
    }
    Ok(())
}

pub(crate) fn temp_name(final_name: &std::ffi::OsStr) -> std::ffi::OsString {
    let mut s = std::ffi::OsString::from(".");
    s.push(final_name);
    s.push(format!(".partial-{}", std::process::id()));
    s
}

pub(crate) fn compress_to_path(
    inputs: &[PathBuf],
    out_path: &Path,
    opts: &CompressOptions,
    progress: &mut dyn FnMut(&Progress),
    cancel: &AtomicBool,
) -> Result<Summary> {
    let started = Instant::now();
    let (out_dir, name) = split_output(out_path)?;
    check_output_location(inputs, &out_dir, out_path)?;
    let entries = walk::scan(inputs, opts, cancel)?;

    let tmp = out_dir.join(temp_name(&name));
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)
        .map_err(|e| io_err(&tmp, e))?;

    let result = (|| -> Result<Summary> {
        let written = writer::write_archive(&entries, file, opts, progress, cancel, &tmp)?;
        let file: File = written.inner;
        file.sync_all().map_err(|e| io_err(&tmp, e))?;
        drop(file);
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        fs::rename(&tmp, out_path).map_err(|e| io_err(out_path, e))?;
        Ok(written.summary)
    })();

    match result {
        Ok(mut summary) => {
            sync_dir(&out_dir);
            summary.elapsed = started.elapsed();
            Ok(summary)
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Best-effort fsync of the directory so the rename itself is durable.
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_name_is_hidden_sibling() {
        let t = temp_name(std::ffi::OsStr::new("out.zip"));
        let t = t.to_string_lossy();
        assert!(t.starts_with(".out.zip.partial-"));
    }

    #[test]
    fn split_output_handles_bare_names() {
        let (d, n) = split_output(Path::new("a.zip")).unwrap();
        assert_eq!(d, PathBuf::from("."));
        assert_eq!(n, "a.zip");
        let (d, n) = split_output(Path::new("/x/y/a.zip")).unwrap();
        assert_eq!(d, PathBuf::from("/x/y"));
        assert_eq!(n, "a.zip");
    }

    #[test]
    fn output_inside_input_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("in");
        fs::create_dir_all(input.join("sub")).unwrap();
        let out = input.join("sub").join("new.zip");
        let err = check_output_location(std::slice::from_ref(&input), &input.join("sub"), &out)
            .unwrap_err();
        assert!(matches!(err, Error::OutputInsideInput(_)));
        // A sibling is fine, even with a shared name prefix.
        fs::create_dir_all(root.path().join("in2")).unwrap();
        check_output_location(
            &[input],
            &root.path().join("in2"),
            &root.path().join("in2/a.zip"),
        )
        .unwrap();
    }
}
