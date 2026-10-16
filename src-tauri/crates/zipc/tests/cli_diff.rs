//! End-to-end tests for `zipc diff` and `zipc test --json`.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
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

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// `root/data/{a.txt, sub/b.txt}` and `root/data.zip` made from it.
fn setup(root: &Path) -> (PathBuf, PathBuf) {
    let data = root.join("data");
    fs::create_dir_all(data.join("sub")).unwrap();
    fs::write(data.join("a.txt"), "hello hello hello hello\n".repeat(50)).unwrap();
    fs::write(data.join("sub/b.txt"), b"world").unwrap();
    let zip = root.join("data.zip");
    let out = zipc([
        OsStr::new("compress"),
        data.as_os_str(),
        OsStr::new("-o"),
        zip.as_os_str(),
        OsStr::new("-q"),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    (data, zip)
}

fn diff(zip: &Path, dir: &Path, extra: &[&str]) -> Output {
    let mut args = vec![OsStr::new("diff"), zip.as_os_str(), dir.as_os_str()];
    args.extend(extra.iter().map(OsStr::new));
    zipc(args)
}

#[test]
fn identical_exits_0() {
    let td = tempfile::tempdir().unwrap();
    let (data, zip) = setup(td.path());
    let out = diff(&zip, &data, &[]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "3 same — identical\n");

    let out = diff(&zip, &data, &["--all", "--full", "-j", "2"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "= a.txt\n= sub/\n= sub/b.txt\n3 same — identical\n"
    );
}

#[test]
fn differences_exit_1_with_markers() {
    let td = tempfile::tempdir().unwrap();
    let (data, zip) = setup(td.path());
    fs::write(data.join("sub/b.txt"), b"World").unwrap();
    fs::remove_file(data.join("a.txt")).unwrap();
    fs::write(data.join("new.txt"), b"new").unwrap();
    let out = diff(&zip, &data, &[]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{text}");
    assert_eq!(lines[0], "- a.txt");
    assert_eq!(lines[1], "+ new.txt");
    assert!(
        lines[2].starts_with("M sub/b.txt  (CRC-32 "),
        "{}",
        lines[2]
    );
    assert_eq!(
        lines[3],
        "1 same, 1 different, 1 only in archive, 1 only on disk — differences found"
    );

    // Excluding the differing paths makes them identical again.
    let out = diff(
        &zip,
        &data,
        &["--exclude", "sub/", "--exclude", "*.txt", "-q"],
    );
    assert_eq!(out.status.code(), Some(0), "stdout: {}", stdout(&out));
}

#[test]
fn json_lists_every_path() {
    let td = tempfile::tempdir().unwrap();
    let (data, zip) = setup(td.path());
    fs::write(data.join("sub/b.txt"), b"worlds").unwrap();
    let out = diff(&zip, &data, &["--json"]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 1, "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["archive"], zip.to_string_lossy().as_ref());
    assert_eq!(v["dir"], data.to_string_lossy().as_ref());
    assert_eq!(v["prefix"], "data/");
    assert_eq!(v["identical"], false);
    assert_eq!(v["counts"]["same"], 2);
    assert_eq!(v["counts"]["different"], 1);
    assert_eq!(v["counts"]["only_on_disk"], 0);
    let entries = v["entries"].as_array().unwrap();
    let paths: Vec<&str> = entries
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["a.txt", "sub/", "sub/b.txt"]);
    assert_eq!(entries[0]["kind"], "same");
    assert!(entries[0]["detail"].is_null());
    assert_eq!(entries[2]["kind"], "different");
    assert_eq!(entries[2]["detail"], "size 5 vs 6");
}

#[test]
fn could_not_compare_exits_3() {
    let td = tempfile::tempdir().unwrap();
    let (data, zip) = setup(td.path());
    let out = diff(&zip, &td.path().join("missing"), &[]);
    assert_eq!(out.status.code(), Some(3), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("not found"), "{}", stderr(&out));

    let junk = td.path().join("junk.zip");
    fs::write(&junk, b"definitely not a zip").unwrap();
    let out = diff(&junk, &data, &["--json"]);
    assert_eq!(out.status.code(), Some(3), "stderr: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(v["error"].as_str().unwrap().contains("not a valid ZIP"));
}

#[test]
fn diff_usage_errors_exit_2() {
    assert_eq!(zipc(["diff"]).status.code(), Some(2));
    assert_eq!(zipc(["diff", "a.zip"]).status.code(), Some(2));
    assert_eq!(
        zipc(["diff", "a.zip", "d", "-j", "lots"]).status.code(),
        Some(2)
    );
}

#[test]
fn explicit_prefix() {
    let td = tempfile::tempdir().unwrap();
    let (data, zip) = setup(td.path());
    let out = diff(&zip, &data.join("sub"), &["--prefix", "data/sub"]);
    assert_eq!(out.status.code(), Some(0), "stdout: {}", stdout(&out));
    let out = diff(&zip, &data, &["--prefix", ""]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("- data/\n"), "{}", stdout(&out));
}

#[test]
fn test_json_one_object_per_archive() {
    let td = tempfile::tempdir().unwrap();
    let (_data, zip) = setup(td.path());
    let bad = td.path().join("bad.zip");
    fs::write(&bad, b"nope").unwrap();
    let missing = td.path().join("missing.zip");

    let out = zipc([OsStr::new("test"), OsStr::new("--json"), zip.as_os_str()]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(v["archive"], zip.to_string_lossy().as_ref());
    assert_eq!(v["ok"], true);
    assert_eq!(v["entries"], 4);
    assert_eq!(v["bytes_checked"], 50 * 24 + 5);
    assert_eq!(v["problems"].as_array().unwrap().len(), 0);
    assert!(v["elapsed_seconds"].as_f64().unwrap() >= 0.0);

    // A damaged entry: one problem naming it.
    let mut b = fs::read(&zip).unwrap();
    let at = b
        .windows(14)
        .position(|w| w == b"data/sub/b.txt")
        .expect("local header name");
    // The first match is the local header's name, 30 bytes in; flip its
    // CRC-32 field so it disagrees with the central directory.
    let lh = at - 30;
    assert_eq!(&b[lh..lh + 4], b"PK\x03\x04");
    b[lh + 14] ^= 0xff;
    let damaged = td.path().join("damaged.zip");
    fs::write(&damaged, &b).unwrap();

    let out = zipc([
        OsStr::new("test"),
        OsStr::new("--json"),
        zip.as_os_str(),
        damaged.as_os_str(),
        bad.as_os_str(),
        missing.as_os_str(),
    ]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    let objs: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(objs.len(), 4, "{text}");
    assert_eq!(objs[0]["ok"], true);
    assert_eq!(objs[1]["ok"], false);
    let problems = objs[1]["problems"].as_array().unwrap();
    assert!(!problems.is_empty());
    assert_eq!(problems[0]["entry"], "data/sub/b.txt");
    assert!(problems[0]["message"].is_string());
    for o in &objs[2..] {
        assert_eq!(o["ok"], false);
        assert!(o["error"].is_string(), "{o}");
        assert!(o.get("problems").is_none());
    }
    assert!(!text.contains("archives OK"));
}
