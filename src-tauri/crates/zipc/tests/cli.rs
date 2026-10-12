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
