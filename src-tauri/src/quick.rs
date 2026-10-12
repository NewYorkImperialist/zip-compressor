//! Quick mode: `zip-compressor --quick PATH...`, what the right-click menu
//! runs. A small window compresses the paths with the default preset into a
//! new archive next to them (Finder-style names: `Name.zip` for one item,
//! `Archive.zip` for several, `… 2.zip` and so on if taken) and closes.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The paths after `--quick`, if the app was started in quick mode.
pub fn from_args(args: impl IntoIterator<Item = OsString>) -> Option<Vec<PathBuf>> {
    let mut args = args.into_iter();
    args.by_ref().find(|a| a == "--quick")?;
    Some(args.map(PathBuf::from).collect())
}

/// The archive to create for `inputs` (at least one): next to the first
/// input, named after it (or `Archive` for several), not overwriting anything.
pub fn output_for(inputs: &[PathBuf]) -> PathBuf {
    let first = trim_trailing_separators(&inputs[0]);
    let dir = first
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let base = if inputs.len() == 1 {
        first.file_name().map_or_else(
            || "Archive".to_owned(),
            |n| n.to_string_lossy().into_owned(),
        )
    } else {
        "Archive".to_owned()
    };
    unique_in(&dir, &base)
}

/// `dir/base.zip`, or `dir/base 2.zip`, `dir/base 3.zip`, … if taken.
fn unique_in(dir: &Path, base: &str) -> PathBuf {
    let first = dir.join(format!("{base}.zip"));
    if !exists(&first) {
        return first;
    }
    (2u32..)
        .map(|n| dir.join(format!("{base} {n}.zip")))
        .find(|p| !exists(p))
        .expect("some name is free")
}

fn exists(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
}

fn trim_trailing_separators(p: &Path) -> PathBuf {
    // `Path::file_name` already ignores a trailing separator; `components`
    // normalizes the rest (`a/b/` -> `a/b`).
    p.components().collect()
}

/// Windows runs a context-menu verb once per selected item (as separate
/// processes, all at about the same time). Gather them into one job: the
/// first process to take the lock collects the others' paths until no more
/// arrive for a moment and returns them all; the others hand their paths
/// over and return `None` (exit without a window).
///
/// Robust to a collector that dies (its lock is released by the OS: a
/// waiting process whose paths were not taken becomes the collector) and to
/// stragglers after collection ends (they start their own job).
#[cfg(any(windows, test))]
pub fn gather(paths: Vec<PathBuf>, spool: &Path) -> Option<Vec<PathBuf>> {
    use std::fs::{self, File};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const QUIET: Duration = Duration::from_millis(400);
    const MAX_WAIT: Duration = Duration::from_secs(5);

    if fs::create_dir_all(spool).is_err() {
        return Some(paths);
    }
    let Ok(lock) = File::create(spool.join("collector.lock")) else {
        return Some(paths);
    };

    // A follower: drop our paths off and wait until they are taken.
    let mut handed: Option<PathBuf> = None;
    let start = Instant::now();
    while lock.try_lock().is_err() {
        if handed.is_none() {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let name = format!("{}-{nanos}", std::process::id());
            let tmp = spool.join(format!("{name}.tmp"));
            let body = join_paths(&paths);
            let done = spool.join(format!("{name}.paths"));
            if fs::write(&tmp, body).is_ok() && fs::rename(&tmp, &done).is_ok() {
                handed = Some(done);
            } else {
                return Some(paths);
            }
        }
        if handed.as_ref().is_some_and(|f| !f.exists()) {
            return None; // collected
        }
        if start.elapsed() > MAX_WAIT {
            break; // give up waiting; make our own archive
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    // We are the collector (or gave up waiting). Take back our own drop-off
    // if it is still there, then collect the others' until it goes quiet.
    let mut all = paths;
    if let Some(f) = handed {
        if fs::remove_file(&f).is_err() {
            // Collected by someone else after all.
            if start.elapsed() <= MAX_WAIT {
                return None;
            }
        }
    }
    let mut last_arrival = Instant::now();
    loop {
        let mut found: Vec<PathBuf> = fs::read_dir(spool)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "paths"))
            .collect();
        found.sort();
        for f in found {
            if let Ok(body) = fs::read_to_string(&f) {
                if fs::remove_file(&f).is_ok() {
                    all.extend(split_paths(&body));
                    last_arrival = Instant::now();
                }
            }
        }
        if last_arrival.elapsed() >= QUIET {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    drop(lock);
    let mut seen = std::collections::HashSet::new();
    all.retain(|p| seen.insert(p.clone()));
    Some(all)
}

#[cfg(any(windows, test))]
fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.to_string_lossy())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(any(windows, test))]
fn split_paths(body: &str) -> impl Iterator<Item = PathBuf> + '_ {
    body.lines().filter(|l| !l.is_empty()).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn args() {
        let a = |v: &[&str]| from_args(v.iter().map(OsString::from));
        assert_eq!(a(&[]), None);
        assert_eq!(a(&["--other"]), None);
        assert_eq!(a(&["--quick"]), Some(vec![]));
        assert_eq!(
            a(&["--quick", "/a b/c", "d"]),
            Some(vec![PathBuf::from("/a b/c"), PathBuf::from("d")])
        );
    }

    #[test]
    fn output_names() {
        let td = tempfile::tempdir().unwrap();
        let d = td.path();
        fs::create_dir(d.join("Photos")).unwrap();
        fs::write(d.join("notes.txt"), "x").unwrap();
        let one = |p: &str| output_for(&[d.join(p)]);
        assert_eq!(one("Photos"), d.join("Photos.zip"));
        assert_eq!(one("Photos/"), d.join("Photos.zip"));
        assert_eq!(one("notes.txt"), d.join("notes.txt.zip"));
        let two = [d.join("Photos"), d.join("notes.txt")];
        assert_eq!(output_for(&two), d.join("Archive.zip"));
        fs::write(d.join("Archive.zip"), "").unwrap();
        fs::write(d.join("Archive 2.zip"), "").unwrap();
        assert_eq!(output_for(&two), d.join("Archive 3.zip"));
        assert_eq!(
            output_for(&[PathBuf::from("rel")]),
            PathBuf::from(".").join("rel.zip")
        );
    }

    #[test]
    fn gather_collects_concurrent_invocations_once() {
        let td = tempfile::tempdir().unwrap();
        let spool = td.path().join("spool");
        let results: Vec<Option<Vec<PathBuf>>> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..6)
                .map(|i| {
                    let spool = spool.clone();
                    s.spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(i * 30));
                        gather(vec![PathBuf::from(format!("/sel/{i}"))], &spool)
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let jobs: Vec<_> = results.into_iter().flatten().collect();
        assert_eq!(jobs.len(), 1, "one collector: {jobs:?}");
        let mut got = jobs[0].clone();
        got.sort();
        let want: Vec<PathBuf> = (0..6).map(|i| PathBuf::from(format!("/sel/{i}"))).collect();
        assert_eq!(got, want);
        // Nothing left behind but the lock file.
        let left: Vec<_> = fs::read_dir(&spool)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from("collector.lock")]);
    }

    #[test]
    fn gather_alone_returns_own_paths() {
        let td = tempfile::tempdir().unwrap();
        let p = vec![PathBuf::from("/only")];
        assert_eq!(gather(p.clone(), td.path()), Some(p));
    }
}
