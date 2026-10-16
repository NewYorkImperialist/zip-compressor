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

#[test]
fn threads_option_is_reported_and_output_is_identical() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_tree(dir.path());
    let one = dir.path().join("one.zip");
    let four = dir.path().join("four.zip");

    for (zip, threads, expect) in [(&one, "1", "(1 thread)"), (&four, "4", "(4 threads)")] {
        let out = zipc([
            OsStr::new("compress"),
            data.as_os_str(),
            OsStr::new("-o"),
            zip.as_os_str(),
            OsStr::new("-t"),
            OsStr::new(threads),
            OsStr::new("-q"),
        ]);
        assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains(expect), "stdout: {stdout}");
    }
    assert_eq!(fs::read(&one).unwrap(), fs::read(&four).unwrap());
}

#[test]
fn threads_must_be_a_number() {
    let out = zipc(["compress", "a", "-o", "a.zip", "--threads", "many"]);
    assert_eq!(out.status.code(), Some(2));
}

/// Run `zipc compress DATA -o ZIP -q EXTRA...` and return its output.
fn compress_with(data: &Path, zip: &Path, extra: &[&str]) -> Output {
    let mut args = vec![
        OsStr::new("compress"),
        data.as_os_str(),
        OsStr::new("-o"),
        zip.as_os_str(),
        OsStr::new("-q"),
    ];
    args.extend(extra.iter().map(OsStr::new));
    zipc(args)
}

/// `unzip -tq` the archive when Info-ZIP's unzip is installed.
fn assert_unzip_ok(zip: &Path) {
    match Command::new("unzip").arg("-tq").arg(zip).output() {
        Ok(out) => assert!(
            out.status.success(),
            "unzip -tq rejected {}: {}{}",
            zip.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(_) => eprintln!("unzip not available; skipping archive check"),
    }
}

#[test]
fn every_preset_makes_a_valid_archive() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_tree(dir.path());
    for preset in ["fast", "balanced", "best", "max"] {
        let zip = dir.path().join(format!("{preset}.zip"));
        let out = compress_with(&data, &zip, &["--preset", preset]);
        assert_eq!(out.status.code(), Some(0), "{preset}: {}", stderr(&out));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(&format!("preset {preset} (")),
            "stdout: {stdout}"
        );
        assert!(fs::read(&zip).unwrap().starts_with(b"PK"));
        assert_unzip_ok(&zip);
    }
}

#[test]
fn short_preset_flag_works() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_tree(dir.path());
    let zip = dir.path().join("out.zip");
    let out = compress_with(&data, &zip, &["-p", "fast"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("preset fast (Deflate level 1)"),
        "stdout: {stdout}"
    );
}

#[test]
fn default_is_balanced_and_matches_the_preset() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_tree(dir.path());
    let default_zip = dir.path().join("default.zip");
    let balanced_zip = dir.path().join("balanced.zip");

    let out = compress_with(&data, &default_zip, &[]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("preset balanced (Deflate level 6)"),
        "stdout: {stdout}"
    );

    let out = compress_with(&data, &balanced_zip, &["--preset", "balanced"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(
        fs::read(&default_zip).unwrap(),
        fs::read(&balanced_zip).unwrap()
    );
}

#[test]
fn overrides_are_reported_as_custom_methods() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_tree(dir.path());
    for (i, (extra, expect)) in [
        (&["-l", "3"][..], "Deflate level 3"),
        (&["--store"][..], "Store, no compression"),
        (&["--zopfli-iterations", "2"][..], "Zopfli, 2 iterations"),
    ]
    .into_iter()
    .enumerate()
    {
        let zip = dir.path().join(format!("o{i}.zip"));
        let out = compress_with(&data, &zip, extra);
        assert_eq!(out.status.code(), Some(0), "{extra:?}: {}", stderr(&out));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains(expect), "stdout: {stdout}");
        assert!(!stdout.contains("preset"), "stdout: {stdout}");
        assert_unzip_ok(&zip);
    }
}

#[test]
fn unknown_preset_is_usage_error() {
    let out = zipc(["compress", "a", "-o", "a.zip", "--preset", "ultra"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn preset_conflicts_with_overrides() {
    for extra in [
        &["--level", "3"][..],
        &["--store"][..],
        &["--zopfli-iterations", "5"][..],
    ] {
        let mut args = vec!["compress", "a", "-o", "a.zip", "--preset", "best"];
        args.extend_from_slice(extra);
        let out = zipc(&args);
        assert_eq!(out.status.code(), Some(2), "{extra:?}: {}", stderr(&out));
    }
}

#[test]
fn zopfli_iterations_conflicts_with_level_and_store() {
    for extra in [&["--level", "3"][..], &["--store"][..]] {
        let mut args = vec!["compress", "a", "-o", "a.zip", "--zopfli-iterations", "5"];
        args.extend_from_slice(extra);
        let out = zipc(&args);
        assert_eq!(out.status.code(), Some(2), "{extra:?}: {}", stderr(&out));
    }
}

#[test]
fn zopfli_iterations_must_be_positive() {
    let out = zipc(["compress", "a", "-o", "a.zip", "--zopfli-iterations", "0"]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn help_explains_presets() {
    for args in [&["--help"][..], &["compress", "--help"][..]] {
        let out = zipc(args);
        assert_eq!(out.status.code(), Some(0));
        let text = String::from_utf8_lossy(&out.stdout);
        for preset in ["fast", "balanced", "best", "max"] {
            assert!(
                text.contains(preset),
                "{args:?} help lacks {preset}: {text}"
            );
        }
        assert!(text.contains("slower"), "{args:?}: {text}");
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn test_ok_archive_exits_0() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_tree(dir.path());
    let zip = dir.path().join("out.zip");
    assert_eq!(compress_with(&data, &zip, &[]).status.code(), Some(0));

    let out = zipc([OsStr::new("test"), zip.as_os_str()]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("OK  "), "stdout: {text}");
    assert!(text.contains("out.zip"), "stdout: {text}");
    // data/, data/a.txt, data/sub/, data/sub/b.txt
    assert!(
        text.contains("(4 entries, 1.2 KiB checked)"),
        "stdout: {text}"
    );

    let out = zipc([
        OsStr::new("test"),
        OsStr::new("-q"),
        zip.as_os_str(),
        zip.as_os_str(),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("2 of 2 archives OK"));
}

#[test]
fn test_corrupted_archive_exits_1_and_names_the_entry() {
    let dir = tempfile::tempdir().unwrap();
    let data = make_tree(dir.path());
    let zip = dir.path().join("bad.zip");
    assert_eq!(
        compress_with(&data, &zip, &["--store"]).status.code(),
        Some(0)
    );
    let mut bytes = fs::read(&zip).unwrap();
    let pos = bytes
        .windows(11)
        .position(|w| w == b"hello hello")
        .expect("stored data");
    bytes[pos + 3] ^= 0x20;
    fs::write(&zip, &bytes).unwrap();

    let good = dir.path().join("good.zip");
    assert_eq!(compress_with(&data, &good, &[]).status.code(), Some(0));

    let out = zipc([
        OsStr::new("test"),
        good.as_os_str(),
        zip.as_os_str(),
        OsStr::new("--quiet"),
    ]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("FAILED  "), "stdout: {text}");
    assert!(text.contains("  data/a.txt: bad CRC-32"), "stdout: {text}");
    assert!(text.contains("OK  "), "good archive still reported: {text}");
    assert!(text.contains("1 of 2 archives OK"), "stdout: {text}");
}

#[test]
fn test_non_zip_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("notes.txt");
    fs::write(&junk, "definitely not a zip file\n".repeat(10)).unwrap();
    let out = zipc([OsStr::new("test"), junk.as_os_str()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("not a valid ZIP archive"),
        "{}",
        stderr(&out)
    );

    let missing = dir.path().join("missing.zip");
    let out = zipc([OsStr::new("test"), missing.as_os_str()]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn test_without_archives_is_usage_error() {
    assert_eq!(zipc(["test"]).status.code(), Some(2));
}

// ---- zipc list ----

/// `make_tree` compressed (with `extra` options) into `dir/NAME`.
fn make_zip(dir: &Path, name: &str, extra: &[&str]) -> std::path::PathBuf {
    let data = dir.join("data");
    if !data.exists() {
        make_tree(dir);
    }
    let zip = dir.join(name);
    let out = compress_with(&data, &zip, extra);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    zip
}

const TREE_NAMES: [&str; 4] = ["data/", "data/a.txt", "data/sub/", "data/sub/b.txt"];

#[test]
fn list_prints_one_name_per_line() {
    let dir = tempfile::tempdir().unwrap();
    let zip = make_zip(dir.path(), "out.zip", &[]);
    let out = zipc([OsStr::new("list"), zip.as_os_str()]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out).lines().collect::<Vec<_>>(), TREE_NAMES);
    assert!(stderr(&out).is_empty(), "stderr: {}", stderr(&out));
}

#[test]
fn list_several_archives_has_headings() {
    let dir = tempfile::tempdir().unwrap();
    let a = make_zip(dir.path(), "a.zip", &[]);
    let b = make_zip(dir.path(), "b.zip", &["--store"]);
    let out = zipc([OsStr::new("list"), a.as_os_str(), b.as_os_str()]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with(&format!("{}:\n", a.display())), "{text}");
    assert!(text.contains(&format!("\n\n{}:\n", b.display())), "{text}");
    assert_eq!(text.lines().filter(|l| *l == "data/a.txt").count(), 2);
}

#[test]
fn list_long_shows_a_table_and_total() {
    let dir = tempfile::tempdir().unwrap();
    let zip = make_zip(dir.path(), "out.zip", &[]);
    let out = zipc([OsStr::new("list"), OsStr::new("--long"), zip.as_os_str()]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1 + TREE_NAMES.len() + 1, "{text}");
    for col in ["Size", "Compressed", "Ratio", "Method", "Modified", "Name"] {
        assert!(lines[0].contains(col), "header lacks {col}: {text}");
    }
    let a = lines.iter().find(|l| l.ends_with("  data/a.txt")).unwrap();
    assert!(a.contains("1.2 KiB") && a.contains("Deflate"), "{a}");
    // Local modified time, e.g. 2024-03-04 05:06:07.
    assert!(
        a.split_whitespace()
            .any(|w| w.len() == 10 && w.as_bytes()[4] == b'-' && w.as_bytes()[7] == b'-'),
        "{a}"
    );
    let b = lines
        .iter()
        .find(|l| l.ends_with("  data/sub/b.txt"))
        .unwrap();
    assert!(b.contains("5 B") && b.contains("Stored"), "{b}");
    assert!(
        lines
            .last()
            .unwrap()
            .ends_with("2 files, 2 dirs, 0 symlinks"),
        "{text}"
    );

    let short = zipc([OsStr::new("list"), OsStr::new("-l"), zip.as_os_str()]);
    assert_eq!(stdout(&short), text);
}

fn parse_json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("invalid JSON ({e}): {text}"))
}

#[test]
fn list_json_is_valid_and_complete() {
    let dir = tempfile::tempdir().unwrap();
    let zip = make_zip(dir.path(), "out.zip", &[]);
    let out = zipc([OsStr::new("list"), OsStr::new("--json"), zip.as_os_str()]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let v = parse_json(&stdout(&out));
    assert_eq!(v["archive"], zip.to_str().unwrap());
    assert_eq!(v["archive_bytes"], fs::metadata(&zip).unwrap().len());
    assert_eq!(v["problems"], serde_json::json!([]));
    let entries = v["entries"].as_array().unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, TREE_NAMES);
    let a = &entries[1];
    assert_eq!(a["kind"], "file");
    assert_eq!(a["method"], 8);
    assert_eq!(a["uncompressed"], 1200);
    assert!(a["compressed"].as_u64().unwrap() < 1200);
    assert!(a["crc32"].is_u64());
    assert_eq!(a["encrypted"], false);
    let modified = a["modified"].as_str().unwrap();
    assert!(
        modified.len() == 20 && modified.ends_with('Z') && modified.as_bytes()[10] == b'T',
        "not RFC 3339 UTC: {modified}"
    );
    assert_eq!(entries[0]["kind"], "dir");
    if cfg!(unix) {
        assert!(a["unix_mode"].as_str().unwrap().starts_with('0'), "{a}");
    } else {
        assert!(a["unix_mode"].is_null(), "{a}");
    }
}

#[cfg(unix)]
#[test]
fn list_json_escapes_awkward_names() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("odd");
    fs::create_dir_all(&data).unwrap();
    let name = "quote\"back\\slash\ttab\u{1}é.txt";
    fs::write(data.join(name), b"x").unwrap();
    std::os::unix::fs::symlink("target", data.join("link")).unwrap();
    let zip = dir.path().join("odd.zip");
    let out = compress_with(&data, &zip, &[]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));

    let out = zipc([OsStr::new("list"), OsStr::new("--json"), zip.as_os_str()]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let v = parse_json(&stdout(&out));
    let entries = v["entries"].as_array().unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&format!("odd/{name}").as_str()), "{names:?}");
    let link = entries.iter().find(|e| e["name"] == "odd/link").unwrap();
    assert_eq!(link["kind"], "symlink");
}

#[test]
fn list_json_several_archives_one_object_per_line() {
    let dir = tempfile::tempdir().unwrap();
    let a = make_zip(dir.path(), "a.zip", &[]);
    let b = make_zip(dir.path(), "b.zip", &["--store"]);
    let out = zipc([
        OsStr::new("list"),
        OsStr::new("--json"),
        a.as_os_str(),
        b.as_os_str(),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    let docs: Vec<serde_json::Value> = text.lines().map(parse_json).collect();
    assert_eq!(docs.len(), 2, "{text}");
    assert_eq!(docs[1]["archive"], b.to_str().unwrap());
    assert_eq!(docs[1]["entries"][1]["method"], 0);
}

#[test]
fn list_bad_archives_exit_1() {
    let dir = tempfile::tempdir().unwrap();
    let good = make_zip(dir.path(), "good.zip", &[]);
    let junk = dir.path().join("junk.zip");
    fs::write(&junk, "definitely not a zip file\n".repeat(10)).unwrap();
    let missing = dir.path().join("missing.zip");

    let out = zipc([OsStr::new("list"), junk.as_os_str()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("not a valid ZIP archive"),
        "{}",
        stderr(&out)
    );
    assert!(stdout(&out).is_empty());

    let out = zipc([
        OsStr::new("list"),
        OsStr::new("--json"),
        missing.as_os_str(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("missing.zip"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty());

    // The good archive is still listed.
    let out = zipc([OsStr::new("list"), missing.as_os_str(), good.as_os_str()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("data/sub/b.txt"), "{}", stdout(&out));
}

#[test]
fn list_usage_errors() {
    assert_eq!(zipc(["list"]).status.code(), Some(2));
    assert_eq!(
        zipc(["list", "--long", "--json", "a.zip"]).status.code(),
        Some(2)
    );
}

// ---- zipc extract ----

#[test]
fn extract_usage_errors() {
    assert_eq!(zipc(["extract"]).status.code(), Some(2));
    assert_eq!(
        zipc(["extract", "a.zip", "--overwrite", "--skip-existing"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        zipc(["extract", "a.zip", "--threads", "many"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(zipc(["extract", "a.zip", "--bogus"]).status.code(), Some(2));
}

#[test]
fn extract_missing_archive_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.zip");
    let out = zipc([OsStr::new("extract"), missing.as_os_str()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("not found"), "{}", stderr(&out));
    assert!(!dir.path().join("missing").exists());

    let out = zipc([OsStr::new("extract"), dir.path().as_os_str()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("not a file"), "{}", stderr(&out));
}

fn list_dir_names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

#[test]
fn extract_refuses_existing_default_destination() {
    let dir = tempfile::tempdir().unwrap();
    let zip = make_zip(dir.path(), "out.zip", &[]);
    let dest = dir.path().join("out");
    fs::create_dir(&dest).unwrap();
    fs::write(dest.join("keep.txt"), b"mine").unwrap();

    let out = zipc([OsStr::new("extract"), zip.as_os_str(), OsStr::new("-q")]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("already exists"), "{err}");
    assert!(
        err.contains("-d") && err.contains("--skip-existing"),
        "{err}"
    );
    assert_eq!(list_dir_names(&dest), ["keep.txt"]);
}

#[test]
fn extract_into_default_destination() {
    let dir = tempfile::tempdir().unwrap();
    let zip = make_zip(dir.path(), "out.zip", &[]);
    let out = zipc([OsStr::new("extract"), zip.as_os_str()]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    let dest = dir.path().join("out");
    assert_eq!(
        fs::read(dest.join("data/a.txt")).unwrap(),
        fs::read(dir.path().join("data/a.txt")).unwrap()
    );
    assert_eq!(fs::read(dest.join("data/sub/b.txt")).unwrap(), b"world");
    let text = stdout(&out);
    assert!(text.contains("2 files, 2 dirs, 0 symlinks"), "{text}");
    assert!(text.contains(&format!("-> {}", dest.display())), "{text}");

    // Again: the folder exists now, so a flag is needed.
    let out = zipc([OsStr::new("extract"), zip.as_os_str()]);
    assert_eq!(out.status.code(), Some(1));
    let out = zipc([
        OsStr::new("extract"),
        zip.as_os_str(),
        OsStr::new("--skip-existing"),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(stdout(&out).contains("2 skipped"), "{}", stdout(&out));
}

#[test]
fn extract_dest_paths_and_json() {
    let dir = tempfile::tempdir().unwrap();
    let zip = make_zip(dir.path(), "out.zip", &[]);
    let dest = dir.path().join("chosen");
    let out = zipc([
        OsStr::new("extract"),
        zip.as_os_str(),
        OsStr::new("data/sub/"),
        OsStr::new("-d"),
        dest.as_os_str(),
        OsStr::new("--json"),
        OsStr::new("-j"),
        OsStr::new("2"),
    ]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(fs::read(dest.join("data/sub/b.txt")).unwrap(), b"world");
    assert!(!dest.join("data/a.txt").exists());
    let v = parse_json(&stdout(&out));
    assert_eq!(v["dest"], dest.to_str().unwrap());
    assert_eq!(v["archive"], zip.to_str().unwrap());
    assert_eq!(v["files"], 1);
    assert_eq!(v["skipped"], 0);
    assert_eq!(v["bytes_out"], 5);
    assert!(v["elapsed_seconds"].is_f64());
}

#[test]
fn extract_existing_file_needs_a_flag() {
    let dir = tempfile::tempdir().unwrap();
    let zip = make_zip(dir.path(), "out.zip", &[]);
    let dest = dir.path().join("dest");
    fs::create_dir_all(dest.join("data/sub")).unwrap();
    fs::write(dest.join("data/sub/b.txt"), b"old").unwrap();
    let run = |flag: Option<&str>| {
        let mut args = vec![
            OsStr::new("extract"),
            zip.as_os_str(),
            OsStr::new("-d"),
            dest.as_os_str(),
            OsStr::new("-q"),
        ];
        args.extend(flag.map(OsStr::new));
        zipc(args)
    };

    let out = run(None);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("--overwrite"), "{}", stderr(&out));
    assert_eq!(fs::read(dest.join("data/sub/b.txt")).unwrap(), b"old");
    assert!(!dest.join("data/a.txt").exists(), "nothing written");

    let out = run(Some("--skip-existing"));
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(fs::read(dest.join("data/sub/b.txt")).unwrap(), b"old");
    assert!(dest.join("data/a.txt").exists());

    let out = run(Some("--overwrite"));
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(fs::read(dest.join("data/sub/b.txt")).unwrap(), b"world");
}

#[test]
fn extract_corrupt_archive_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    let junk = dir.path().join("junk.zip");
    fs::write(&junk, "definitely not a zip file\n".repeat(10)).unwrap();
    let out = zipc([OsStr::new("extract"), junk.as_os_str()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("not a valid ZIP archive"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn help_mentions_list_and_extract() {
    let text = stdout(&zipc(["--help"]));
    for word in ["list", "extract", "Exit status"] {
        assert!(text.contains(word), "help lacks {word}: {text}");
    }
    let text = stdout(&zipc(["extract", "--help"]));
    assert!(
        text.contains("--skip-existing") && text.contains("photos/"),
        "{text}"
    );
}
