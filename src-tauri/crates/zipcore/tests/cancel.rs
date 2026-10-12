//! Phase 2: cancellation and worker failures with a thread pool. Every run is
//! under a watchdog so a deadlocked pipeline fails instead of hanging.

mod common;

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::*;
use zipcore::{Error, Method, Progress};

const WATCHDOG: u64 = 10;

fn many_files(root: &Path, n: usize) {
    for i in 0..n {
        write_file(
            &root.join(format!("d{}/f{i:04}.txt", i % 9)),
            &compressible_bytes(3000 + i),
        );
    }
}

fn periodic(len: usize, period: usize, seed: u64) -> Vec<u8> {
    random_bytes(period, seed)
        .into_iter()
        .cycle()
        .take(len)
        .collect()
}

enum Sink {
    Mem,
    Path(PathBuf),
}

/// Run with a cancel trigger; returns (result, progress calls after cancel,
/// time from cancel to return).
fn run_cancel(
    inputs: Vec<PathBuf>,
    sink: Sink,
    method: Method,
    trigger: impl Fn(&Progress) -> bool + Send + 'static,
) -> (zipcore::Result<zipcore::Summary>, usize, Option<Duration>) {
    with_timeout(WATCHDOG, "cancelled run", move || {
        let cancel = AtomicBool::new(false);
        let mut after = 0usize;
        let mut at: Option<Instant> = None;
        let mut cb = |p: &Progress| {
            if at.is_some() {
                after += 1;
            } else if trigger(p) {
                at = Some(Instant::now());
                cancel.store(true, Ordering::SeqCst);
            }
        };
        let opts = opts_t(method, 8);
        let res = match sink {
            Sink::Mem => {
                let mut cur = Cursor::new(Vec::new());
                zipcore::compress(&inputs, &mut cur, &opts, &mut cb, &cancel)
            }
            Sink::Path(p) => zipcore::compress_to_path(&inputs, &p, &opts, &mut cb, &cancel),
        };
        let lag = at.map(|t| t.elapsed());
        (res, after, lag)
    })
}

#[test]
fn cancel_mid_run_with_8_threads() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("many");
    many_files(&root, 600);
    let (res, after, lag) = run_cancel(
        vec![root.clone()],
        Sink::Mem,
        Method::Deflate { level: 6 },
        |p| p.files_done >= 2,
    );
    let err = res.expect_err("cancelled run returned Ok");
    assert!(matches!(err, Error::Cancelled), "got {err:?}");
    assert!(lag.is_some(), "never reached files_done >= 2");
    assert!(after <= 3, "{after} progress calls after cancel");
    assert!(
        lag.unwrap() < Duration::from_secs(3),
        "took {lag:?} to stop"
    );

    let out_dir = td.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    let (res, after, _) = run_cancel(
        vec![root],
        Sink::Path(out_dir.join("a.zip")),
        Method::Deflate { level: 6 },
        |p| p.files_done >= 5,
    );
    let err = res.expect_err("cancelled run returned Ok");
    assert!(matches!(err, Error::Cancelled), "got {err:?}");
    assert!(after <= 3, "{after} progress calls after cancel");
    assert!(
        list_dir(&out_dir).is_empty(),
        "leftovers: {:?}",
        list_dir(&out_dir)
    );
}

#[test]
fn cancel_while_chunked_file_in_flight() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("bigcancel");
    write_file(&root.join("a.txt"), b"first");
    let big_len = 64 * MIB;
    write_file(&root.join("b_big.bin"), &periodic(big_len, 10_007, 1));
    many_files(&root.join("c_after"), 50);
    let big_name = "bigcancel/b_big.bin";
    for to_path in [false, true] {
        let out_dir = td.path().join(format!("out{to_path}"));
        fs::create_dir(&out_dir).unwrap();
        let sink = if to_path {
            Sink::Path(out_dir.join("big.zip"))
        } else {
            Sink::Mem
        };
        let (res, _after, lag) = run_cancel(
            vec![root.clone()],
            sink,
            Method::Deflate { level: 9 },
            move |p| p.current == big_name && p.bytes_done > 0 && p.bytes_done < p.bytes_total / 2,
        );
        assert!(
            lag.is_some(),
            "to_path={to_path}: no progress report while the 64 MiB file was in flight (result {:?})",
            res.as_ref().map(|_| ())
        );
        let err = res.expect_err("cancelled run returned Ok");
        assert!(
            matches!(err, Error::Cancelled),
            "to_path={to_path}: got {err:?}"
        );
        assert!(
            lag.unwrap() < Duration::from_secs(4),
            "to_path={to_path}: took {lag:?} after cancel to stop"
        );
        assert!(
            list_dir(&out_dir).is_empty(),
            "leftovers: {:?}",
            list_dir(&out_dir)
        );
    }
}

/// Cancel flipped by another thread, not from the callback.
#[test]
fn cancel_from_another_thread() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("ext");
    write_file(&root.join("big.bin"), &periodic(48 * MIB, 5_003, 2));
    many_files(&root.join("more"), 100);
    let out_dir = td.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    let out = out_dir.join("x.zip");
    let res = with_timeout(WATCHDOG, "externally cancelled run", move || {
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            c2.store(true, Ordering::SeqCst);
        });
        let r = zipcore::compress_to_path(
            &[root],
            &out,
            &opts_t(Method::Deflate { level: 9 }, 8),
            &mut |_| {},
            &cancel,
        );
        t.join().unwrap();
        r
    });
    let err = res.expect_err("run finished before a 300 ms cancel? input too small");
    assert!(matches!(err, Error::Cancelled), "got {err:?}");
    assert!(
        list_dir(&out_dir).is_empty(),
        "leftovers: {:?}",
        list_dir(&out_dir)
    );
}

#[cfg(unix)]
mod unreadable {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o644));
        }
    }

    fn lock(p: &Path) -> Option<Restore> {
        fs::set_permissions(p, fs::Permissions::from_mode(0o000)).unwrap();
        let g = Restore(p.to_path_buf());
        if fs::read(p).is_ok() {
            eprintln!("NOTE: skipping unreadable-file test: running as root");
            return None;
        }
        Some(g)
    }

    fn check(root: PathBuf, secret: &Path, rel: &str, td: &Path) {
        for to_path in [false, true] {
            let out_dir = td.join(format!("out-{to_path}-{}", rel.replace('/', "_")));
            fs::create_dir(&out_dir).unwrap();
            let out = out_dir.join("x.zip");
            let inputs = vec![root.clone()];
            let res = with_timeout(WATCHDOG, "run with unreadable file", move || {
                let cancel = AtomicBool::new(false);
                let opts = opts_t(Method::Deflate { level: 6 }, 8);
                if to_path {
                    zipcore::compress_to_path(&inputs, &out, &opts, &mut |_| {}, &cancel)
                } else {
                    let mut cur = Cursor::new(Vec::new());
                    zipcore::compress(&inputs, &mut cur, &opts, &mut |_| {}, &cancel)
                }
            });
            let err = res.expect_err("an unreadable file must fail the run");
            match &err {
                Error::Io { path, .. } => assert!(
                    path.ends_with(rel) || path == secret,
                    "to_path={to_path}: Io error names {path:?}, expected {secret:?}"
                ),
                other => panic!("to_path={to_path}: expected Error::Io, got {other:?}"),
            }
            assert!(
                list_dir(&out_dir).is_empty(),
                "leftovers: {:?}",
                list_dir(&out_dir)
            );
        }
    }

    #[test]
    fn unreadable_file_among_500_with_8_threads() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("many");
        many_files(&root, 500);
        let rel = "d4/f0250_secret.txt";
        let secret = root.join(rel);
        write_file(&secret, &compressible_bytes(5000));
        let Some(_g) = lock(&secret) else { return };
        check(root, &secret, &format!("many/{rel}"), td.path());
    }

    /// The failing file comes right after a large chunked file, so the
    /// pipeline is full of chunk work when the error surfaces.
    #[test]
    fn unreadable_file_after_large_chunked_file() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("pipe");
        write_file(&root.join("a_big.txt"), &periodic(24 * MIB, 3_001, 3));
        let secret = root.join("b_locked.txt");
        write_file(&secret, b"locked");
        many_files(&root.join("c_more"), 100);
        let Some(_g) = lock(&secret) else { return };
        check(root, &secret, "pipe/b_locked.txt", td.path());
    }

    /// An unreadable large (would-be chunked) file.
    #[test]
    fn unreadable_large_file() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("lg");
        many_files(&root.join("a"), 50);
        let secret = root.join("b_huge.bin");
        write_file(&secret, &compressible_bytes(6 * MIB));
        many_files(&root.join("c"), 50);
        let Some(_g) = lock(&secret) else { return };
        check(root, &secret, "lg/b_huge.bin", td.path());
    }
}
