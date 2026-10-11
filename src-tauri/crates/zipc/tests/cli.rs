//! End-to-end tests that run the built `zipc` binary.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn zipc<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_zipc"))
        .args(args)
        .output()
        .expect("failed to run zipc")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A small tree: `root/data/{a.txt, sub/b.txt}`.
fn make_tree(root: &Path) -> std::path::PathBuf {
    let data = root.join("data");
    fs::create_dir_all(data.join("sub")).unwrap();
    fs::write(data.join("a.txt"), "hello hello hello hello\n".repeat(50)).unwrap();
    fs::write(data.join("sub/b.txt"), b"world").unwrap();
    data
}

#[test]
fn no_arguments_is_usage_error() {
    let out = zipc::<_, &str>([]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn compress_without_output_is_usage_error() {
    let out = zipc(["compress", "somefile"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn level_out_of_range_is_usage_error() {
    let out = zipc(["compress", "a", "-o", "a.zip", "--level", "10"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn store_conflicts_with_level() {
    let out = zipc(["compress", "a", "-o", "a.zip", "--store", "--level", "3"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn missing_input_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope");
    let zip = dir.path().join("out.zip");
    let out = zipc([
        OsStr::new("compress"),
        missing.as_os_str(),
        OsStr::new("-o"),
        zip.as_os_str(),
    ]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("not found"));
    assert!(!zip.exists());
}

#[test]
fn refuses_to_overwrite_without_force() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_tree(dir.path());
    let zip = dir.path().join("out.zip");
    fs::write(&zip, b"precious").unwrap();

    let out = zipc([
        OsStr::new("compress"),
        data.as_os_str(),
        OsStr::new("-o"),
        zip.as_os_str(),
    ]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("--force"));
    assert_eq!(fs::read(&zip).unwrap(), b"precious");
}

#[test]
fn compresses_a_tree() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_tree(dir.path());
    let zip = dir.path().join("out.zip");

    let out = zipc([
        OsStr::new("compress"),
        data.as_os_str(),
        OsStr::new("-o"),
        zip.as_os_str(),
        OsStr::new("--quiet"),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let bytes = fs::read(&zip).unwrap();
    assert!(bytes.starts_with(b"PK"), "not a zip");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("2 files"), "stdout: {stdout}");
}

#[test]
fn force_overwrites_and_store_works() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_tree(dir.path());
    let zip = dir.path().join("out.zip");
    fs::write(&zip, b"old").unwrap();

    let out = zipc([
        OsStr::new("compress"),
        data.as_os_str(),
        OsStr::new("-o"),
        zip.as_os_str(),
        OsStr::new("--store"),
        OsStr::new("--force"),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(fs::read(&zip).unwrap().starts_with(b"PK"));
}
