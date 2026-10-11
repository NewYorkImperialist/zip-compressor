//! Progress reporting, cancellation, `compress_to_path` and error paths.

mod common;

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use zipcore::{Error, Method, Progress};

const DEFLATE: Method = Method::Deflate { level: 6 };

fn many_files(parent: &Path, n: usize) -> PathBuf {
    let root = parent.join("many");
    for i in 0..n {
        write_file(
            &root.join(format!("d{}/f{i:03}.txt", i % 5)),
            &compressible_bytes(2000 + i),
        );
    }
    root
}

fn to_path(
    inputs: &[PathBuf],
    out: &Path,
    progress: &mut dyn FnMut(&Progress),
    cancel: &AtomicBool,
) -> zipcore::Result<zipcore::Summary> {
    zipcore::compress_to_path(inputs, out, &opts(DEFLATE), progress, cancel)
}

#[test]
fn progress_is_monotonic_and_completes() {
    let td = tempfile::tempdir().unwrap();
    let root = mixed_tree(td.path());
    let extra = many_files(td.path(), 20);
    let mut events: Vec<Progress> = Vec::new();
    let mut cb = |p: &Progress| events.push(p.clone());
    let cancel = AtomicBool::new(false);
    let mut cur = Cursor::new(Vec::new());
    let summary =
        zipcore::compress(&[root, extra], &mut cur, &opts(DEFLATE), &mut cb, &cancel).unwrap();
    let _entries = read_entries(&cur.into_inner());

    assert!(!events.is_empty(), "progress callback never invoked");
    let first = &events[0];
    for w in events.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        assert!(
            b.files_done >= a.files_done,
            "files_done went backwards: {a:?} -> {b:?}"
        );
        assert!(
            b.bytes_done >= a.bytes_done,
            "bytes_done went backwards: {a:?} -> {b:?}"
        );
    }
    for p in &events {
        assert_eq!(
            p.files_total, first.files_total,
            "files_total changed: {p:?}"
        );
        assert_eq!(
            p.bytes_total, first.bytes_total,
            "bytes_total changed: {p:?}"
        );
        assert!(p.files_done <= p.files_total, "{p:?}");
        assert!(p.bytes_done <= p.bytes_total, "{p:?}");
    }
    let last = events.last().unwrap();
    assert_eq!(
        last.files_done, last.files_total,
        "final files_done: {last:?}"
    );
    assert_eq!(
        last.bytes_done, last.bytes_total,
        "final bytes_done: {last:?}"
    );
    assert_eq!(
        last.bytes_total, summary.bytes_in,
        "bytes_total vs Summary.bytes_in"
    );
    assert!(
        last.files_total >= summary.files,
        "files_total {} < Summary.files {}",
        last.files_total,
        summary.files
    );
}

#[test]
fn pre_cancelled_returns_cancelled() {
    let td = tempfile::tempdir().unwrap();
    let root = mixed_tree(td.path());
    let cancel = AtomicBool::new(true);
    let mut cb = |_: &Progress| {};
    let mut cur = Cursor::new(Vec::new());
    let err = zipcore::compress(
        std::slice::from_ref(&root),
        &mut cur,
        &opts(DEFLATE),
        &mut cb,
        &cancel,
    )
    .unwrap_err();
    assert!(matches!(err, Error::Cancelled), "got {err:?}");

    let out_dir = td.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    let out = out_dir.join("a.zip");
    let err = to_path(&[root], &out, &mut cb, &cancel).unwrap_err();
    assert!(matches!(err, Error::Cancelled), "got {err:?}");
    assert!(
        list_dir(&out_dir).is_empty(),
        "leftovers: {:?}",
        list_dir(&out_dir)
    );
}

#[test]
fn cancel_from_progress_callback() {
    let td = tempfile::tempdir().unwrap();
    let root = many_files(td.path(), 60);
    let cancel = AtomicBool::new(false);
    let mut calls_after_cancel = 0usize;
    let mut cancelled_at: Option<u64> = None;
    let mut cb = |p: &Progress| {
        if cancelled_at.is_some() {
            calls_after_cancel += 1;
        } else if p.files_done >= 2 {
            cancelled_at = Some(p.files_done);
            cancel.store(true, Ordering::SeqCst);
        }
    };
    let mut cur = Cursor::new(Vec::new());
    let err = zipcore::compress(&[root], &mut cur, &opts(DEFLATE), &mut cb, &cancel).unwrap_err();
    assert!(matches!(err, Error::Cancelled), "got {err:?}");
    assert!(cancelled_at.is_some(), "never got to files_done >= 2");
    // "stops promptly": it must not keep processing dozens of further files.
    assert!(
        calls_after_cancel <= 3,
        "{calls_after_cancel} progress calls after cancel"
    );
}

#[test]
fn cancel_mid_run_with_compress_to_path_leaves_nothing() {
    let td = tempfile::tempdir().unwrap();
    let root = many_files(td.path(), 60);
    let out_dir = td.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    let out = out_dir.join("archive.zip");
    let cancel = AtomicBool::new(false);
    let mut cb = |p: &Progress| {
        if p.files_done >= 3 {
            cancel.store(true, Ordering::SeqCst);
        }
    };
    let err = to_path(&[root], &out, &mut cb, &cancel).unwrap_err();
    assert!(matches!(err, Error::Cancelled), "got {err:?}");
    assert!(!out.exists());
    assert!(
        list_dir(&out_dir).is_empty(),
        "leftovers: {:?}",
        list_dir(&out_dir)
    );
}

#[test]
fn cancel_preserves_existing_output_file() {
    let td = tempfile::tempdir().unwrap();
    let root = many_files(td.path(), 30);
    let out_dir = td.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    let out = out_dir.join("archive.zip");
    fs::write(&out, b"previous archive").unwrap();
    let cancel = AtomicBool::new(false);
    let mut cb = |p: &Progress| {
        if p.files_done >= 3 {
            cancel.store(true, Ordering::SeqCst);
        }
    };
    let err = to_path(&[root], &out, &mut cb, &cancel).unwrap_err();
    assert!(matches!(err, Error::Cancelled), "got {err:?}");
    assert_eq!(fs::read(&out).unwrap(), b"previous archive");
    assert_eq!(list_dir(&out_dir), vec!["archive.zip"]);
}

#[test]
fn compress_to_path_success() {
    let td = tempfile::tempdir().unwrap();
    let root = mixed_tree(td.path());
    let out_dir = td.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    let out = out_dir.join("result.zip");
    // An existing file at the destination is replaced.
    fs::write(&out, b"stale").unwrap();
    let cancel = AtomicBool::new(false);
    let mut cb = |_: &Progress| {};
    let summary = to_path(std::slice::from_ref(&root), &out, &mut cb, &cancel).unwrap();
    let bytes = fs::read(&out).unwrap();
    assert_eq!(
        summary.bytes_out,
        bytes.len() as u64,
        "Summary.bytes_out vs file size"
    );
    let entries = read_entries(&bytes);
    assert_nodes_eq(&archive_nodes(&entries), &expected_entries(&root));
    assert_eq!(
        list_dir(&out_dir),
        vec!["result.zip"],
        "temp files left behind"
    );

    // Same tree through compress() gives the same archive.
    let (mem, _) = compress_ok(&[root], &opts(DEFLATE));
    assert_eq!(names(&read_entries(&mem)), names(&entries));
}

#[test]
fn compress_to_path_missing_input() {
    let td = tempfile::tempdir().unwrap();
    let out_dir = td.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    let missing = td.path().join("nope");
    let cancel = AtomicBool::new(false);
    let mut cb = |_: &Progress| {};
    let err = to_path(
        std::slice::from_ref(&missing),
        &out_dir.join("x.zip"),
        &mut cb,
        &cancel,
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::NotFound(ref p) if p == &missing),
        "got {err:?}"
    );
    assert!(
        list_dir(&out_dir).is_empty(),
        "leftovers: {:?}",
        list_dir(&out_dir)
    );
}

fn assert_inside_refused(inputs: &[PathBuf], out: &Path, watch: &Path) {
    let before = snapshot(watch);
    let cancel = AtomicBool::new(false);
    let mut cb = |_: &Progress| {};
    let err = to_path(inputs, out, &mut cb, &cancel).unwrap_err();
    assert!(
        matches!(err, Error::OutputInsideInput(_)),
        "out={out:?}: got {err:?}"
    );
    assert!(!out.exists(), "{out:?} was created");
    assert_eq!(
        snapshot(watch),
        before,
        "input tree modified by refused run"
    );
}

#[test]
fn output_inside_input_is_refused() {
    let td = tempfile::tempdir().unwrap();
    let root = mixed_tree(td.path());
    assert_inside_refused(std::slice::from_ref(&root), &root.join("self.zip"), &root);
    assert_inside_refused(
        std::slice::from_ref(&root),
        &root.join("deep/x/self.zip"),
        &root,
    );

    // Non-canonical spelling of a path inside the input.
    let sneaky = root.join("deep").join("..").join("sneaky.zip");
    assert_inside_refused(std::slice::from_ref(&root), &sneaky, &root);

    // Second of several inputs.
    let other = td.path().join("other.txt");
    write_file(&other, b"o");
    assert_inside_refused(&[other, root.clone()], &root.join("bin/self.zip"), &root);
}

#[cfg(unix)]
#[test]
fn output_inside_input_via_symlinked_path_is_refused() {
    let td = tempfile::tempdir().unwrap();
    let root = mixed_tree(td.path());
    let canon_root = fs::canonicalize(&root).unwrap();
    // Reach the input through an alias symlink.
    let alias = td.path().join("alias");
    std::os::unix::fs::symlink(&canon_root, &alias).unwrap();
    assert_inside_refused(std::slice::from_ref(&root), &alias.join("x.zip"), &root);
    assert_inside_refused(
        std::slice::from_ref(&alias),
        &canon_root.join("y.zip"),
        &root,
    );
}

#[test]
fn output_next_to_input_with_shared_prefix_is_allowed() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("data");
    write_file(&root.join("f.txt"), b"f");
    let cancel = AtomicBool::new(false);
    let mut cb = |_: &Progress| {};
    // `data2.zip` and `data-out/` share a string prefix with `data` but are not inside it.
    let out1 = td.path().join("data2.zip");
    to_path(std::slice::from_ref(&root), &out1, &mut cb, &cancel)
        .expect("data2.zip is not inside data/");
    let sibling = td.path().join("data-out");
    fs::create_dir(&sibling).unwrap();
    let out2 = sibling.join("a.zip");
    to_path(std::slice::from_ref(&root), &out2, &mut cb, &cancel)
        .expect("data-out/ is not inside data/");
    assert!(out1.is_file() && out2.is_file());
}

#[cfg(unix)]
#[test]
fn unreadable_file_errors_and_leaves_nothing() {
    use std::os::unix::fs::PermissionsExt;
    let td = tempfile::tempdir().unwrap();
    let root = many_files(td.path(), 10);
    let secret = root.join("d0/zz_secret.txt");
    write_file(&secret, b"secret");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&secret).is_ok() {
        eprintln!("NOTE: skipping unreadable_file_errors_and_leaves_nothing: running as root");
        return;
    }
    let out_dir = td.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    let cancel = AtomicBool::new(false);
    let mut cb = |_: &Progress| {};
    let res = to_path(
        std::slice::from_ref(&root),
        &out_dir.join("x.zip"),
        &mut cb,
        &cancel,
    );
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).unwrap();
    let err = res.expect_err("an unreadable file must fail the run");
    assert!(matches!(err, Error::Io { .. }), "got {err:?}");
    assert!(
        list_dir(&out_dir).is_empty(),
        "leftovers: {:?}",
        list_dir(&out_dir)
    );
}

#[test]
fn progress_current_is_an_archive_path() {
    let td = tempfile::tempdir().unwrap();
    let root = mixed_tree(td.path());
    let mut events: Vec<Progress> = Vec::new();
    let mut cb = |p: &Progress| events.push(p.clone());
    let cancel = AtomicBool::new(false);
    let mut cur = Cursor::new(Vec::new());
    zipcore::compress(
        std::slice::from_ref(&root),
        &mut cur,
        &opts(DEFLATE),
        &mut cb,
        &cancel,
    )
    .unwrap();
    let entry_names: std::collections::HashSet<String> = names(&read_entries(&cur.into_inner()))
        .into_iter()
        .collect();
    let bad: Vec<&str> = events
        .iter()
        .map(|p| p.current.as_str())
        .filter(|c| !c.is_empty() && !entry_names.contains(*c))
        .collect();
    assert!(
        bad.is_empty(),
        "Progress.current values that are not archive entry names: {bad:?}\nentries: {entry_names:?}"
    );
}
