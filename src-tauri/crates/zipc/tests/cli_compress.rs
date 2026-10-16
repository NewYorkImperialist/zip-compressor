//! End-to-end tests for `zipc compress`: --exclude, --gitignore,
//! --compress-all and --json.

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

fn write(path: &Path, data: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

/// `root/proj` with a .gitignore, a .git dir and a few things to exclude.
fn make_proj(root: &Path) -> PathBuf {
    let p = root.join("proj");
    write(&p.join("src/main.rs"), b"fn main() {}\n");
    write(&p.join("src/debug.log"), b"log\n");
    write(
        &p.join("notes.txt"),
        "notes notes notes\n".repeat(20).as_bytes(),
    );
    write(&p.join("target/out.bin"), b"\x00\x01");
    write(&p.join("node_modules/m/index.js"), b"x");
    write(&p.join("tmp/scratch.tmp"), b"x");
    write(&p.join(".git/HEAD"), b"ref: refs/heads/main\n");
    write(&p.join(".gitignore"), b"/target/\n*.tmp\n");
    p
}

/// Compress `inputs` to `zip` with extra args; panic unless it succeeds.
fn compress_ok(inputs: &[&Path], zip: &Path, extra: &[&str]) -> Output {
    let mut args: Vec<&OsStr> = vec![OsStr::new("compress")];
    args.extend(inputs.iter().map(|p| p.as_os_str()));
    args.extend([OsStr::new("-o"), zip.as_os_str()]);
    args.extend(extra.iter().map(OsStr::new));
    let out = zipc(&args);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    out
}

fn names(zip: &Path) -> Vec<String> {
    let out = zipc([OsStr::new("list"), zip.as_os_str()]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    stdout(&out).lines().map(str::to_owned).collect()
}

fn parse_json(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("invalid JSON ({e}): {text}"))
}

#[test]
fn exclude_is_repeatable_and_has_a_short_form() {
    let dir = tempfile::tempdir().unwrap();
    let p = make_proj(dir.path());
    let zip = dir.path().join("out.zip");
    compress_ok(
        &[&p],
        &zip,
        &[
            "--exclude",
            "*.log",
            "-x",
            "node_modules/",
            "-x",
            "/target",
            "-x",
            ".git",
        ],
    );
    assert_eq!(
        names(&zip),
        [
            "proj/",
            "proj/.gitignore",
            "proj/notes.txt",
            "proj/src/",
            "proj/src/main.rs",
            "proj/tmp/",
            "proj/tmp/scratch.tmp",
        ]
    );
}

#[test]
fn gitignore_flag_reads_gitignore_files_and_skips_dot_git() {
    let dir = tempfile::tempdir().unwrap();
    let p = make_proj(dir.path());
    let zip = dir.path().join("out.zip");
    compress_ok(&[&p], &zip, &["--gitignore"]);
    let n = names(&zip);
    assert_eq!(
        n,
        [
            "proj/",
            "proj/.gitignore",
            "proj/node_modules/",
            "proj/node_modules/m/",
            "proj/node_modules/m/index.js",
            "proj/notes.txt",
            "proj/src/",
            "proj/src/debug.log",
            "proj/src/main.rs",
            "proj/tmp/",
        ]
    );

    // Without the flag everything goes in.
    let all = dir.path().join("all.zip");
    compress_ok(&[&p], &all, &[]);
    let n = names(&all);
    for want in [
        "proj/.git/HEAD",
        "proj/target/out.bin",
        "proj/tmp/scratch.tmp",
    ] {
        assert!(n.contains(&want.to_string()), "{want} missing: {n:?}");
    }
}

#[test]
fn exclude_never_drops_a_named_input() {
    let dir = tempfile::tempdir().unwrap();
    let p = make_proj(dir.path());
    let zip = dir.path().join("out.zip");
    let notes = p.join("notes.txt");
    compress_ok(&[&notes], &zip, &["-x", "*.txt", "-x", "notes.txt"]);
    assert_eq!(names(&zip), ["notes.txt"]);
}

#[test]
fn compress_all_makes_a_valid_archive_and_conflicts_with_store() {
    let dir = tempfile::tempdir().unwrap();
    let p = make_proj(dir.path());
    let zip = dir.path().join("out.zip");
    compress_ok(&[&p], &zip, &["--compress-all"]);
    let t = zipc([OsStr::new("test"), zip.as_os_str()]);
    assert!(t.status.success(), "stderr: {}", stderr(&t));

    let out = zipc([
        OsStr::new("compress"),
        p.as_os_str(),
        OsStr::new("-o"),
        dir.path().join("s.zip").as_os_str(),
        OsStr::new("--store"),
        OsStr::new("--compress-all"),
    ]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
}

#[test]
fn json_summary_is_one_complete_object() {
    let dir = tempfile::tempdir().unwrap();
    let p = make_proj(dir.path());
    let zip = dir.path().join("out \"q\".zip");
    let out = compress_ok(&[&p], &zip, &["--json", "-t", "2", "-x", ".git/"]);
    let text = stdout(&out);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(stderr(&out).is_empty(), "stderr: {}", stderr(&out));
    let v = parse_json(&text);

    assert_eq!(v["output"], zip.to_string_lossy().as_ref());
    // proj/: .gitignore, notes.txt, src/{main.rs,debug.log}, target/out.bin,
    // node_modules/m/index.js, tmp/scratch.tmp; dirs proj/, src/, target/,
    // node_modules/, node_modules/m/, tmp/.
    assert_eq!(v["files"], 7);
    assert_eq!(v["dirs"], 6);
    assert_eq!(v["symlinks"], 0);
    let bytes_in = v["bytes_in"].as_u64().unwrap();
    let bytes_out = v["bytes_out"].as_u64().unwrap();
    assert_eq!(bytes_out, fs::metadata(&zip).unwrap().len());
    let ratio = v["ratio"].as_f64().unwrap();
    assert!(
        (ratio - bytes_out as f64 / bytes_in as f64).abs() < 1e-9,
        "{v}"
    );
    assert!(v["elapsed_seconds"].as_f64().unwrap() >= 0.0);
    assert_eq!(v["preset"], "balanced");
    assert_eq!(v["method"], "deflate");
    assert_eq!(v["level"], 6);
    assert!(v["iterations"].is_null());
    assert_eq!(v["threads"], 2);
}

/// Extra args, then the expected method, level, iterations and preset.
type MethodCase<'a> = (
    &'a [&'a str],
    &'a str,
    Option<u64>,
    Option<u64>,
    Option<&'a str>,
);

#[test]
fn json_describes_method_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let p = make_proj(dir.path());
    let cases: [MethodCase; 4] = [
        (&["-l", "3"], "deflate", Some(3), None, None),
        (&["--store"], "store", None, None, None),
        (&["--zopfli-iterations", "2"], "zopfli", None, Some(2), None),
        (&["-p", "best"], "deflate-best", None, None, Some("best")),
    ];
    for (i, (extra, method, level, iters, preset)) in cases.into_iter().enumerate() {
        let zip = dir.path().join(format!("{i}.zip"));
        let mut args = extra.to_vec();
        args.push("--json");
        let v = parse_json(&stdout(&compress_ok(&[&p], &zip, &args)));
        assert_eq!(v["method"], method, "{extra:?}: {v}");
        assert_eq!(v["level"].as_u64(), level, "{extra:?}: {v}");
        assert_eq!(v["iterations"].as_u64(), iters, "{extra:?}: {v}");
        assert_eq!(v["preset"].as_str(), preset, "{extra:?}: {v}");
    }
}

#[test]
fn json_empty_input_has_null_ratio() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty");
    fs::create_dir(&empty).unwrap();
    let zip = dir.path().join("out.zip");
    let v = parse_json(&stdout(&compress_ok(&[&empty], &zip, &["--json"])));
    assert_eq!(v["files"], 0);
    assert_eq!(v["dirs"], 1);
    assert!(v["ratio"].is_null(), "{v}");
}

#[test]
fn json_errors_go_to_stderr_with_empty_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let p = make_proj(dir.path());
    let zip = dir.path().join("out.zip");
    fs::write(&zip, b"precious").unwrap();

    // Existing output without --force.
    let out = zipc([
        OsStr::new("compress"),
        p.as_os_str(),
        OsStr::new("-o"),
        zip.as_os_str(),
        OsStr::new("--json"),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "stdout: {}", stdout(&out));
    assert!(stderr(&out).contains("already exists"), "{}", stderr(&out));
    assert_eq!(fs::read(&zip).unwrap(), b"precious");

    // Missing input.
    let out = zipc([
        OsStr::new("compress"),
        dir.path().join("nope").as_os_str(),
        OsStr::new("-o"),
        dir.path().join("x.zip").as_os_str(),
        OsStr::new("--json"),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "stdout: {}", stdout(&out));

    // Invalid pattern (a trailing lone backslash).
    let x = dir.path().join("x.zip");
    let out = zipc([
        OsStr::new("compress"),
        p.as_os_str(),
        OsStr::new("-o"),
        x.as_os_str(),
        OsStr::new("--json"),
        OsStr::new("-x"),
        OsStr::new("bad\\"),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "stdout: {}", stdout(&out));
    assert!(
        stderr(&out).contains("invalid exclude pattern"),
        "{}",
        stderr(&out)
    );
    assert!(!x.exists());
}

#[test]
fn help_lists_the_new_flags() {
    let out = zipc(["compress", "--help"]);
    assert!(out.status.success());
    let help = stdout(&out);
    for want in [
        "--exclude <PATTERN>",
        "-x",
        "--gitignore",
        "--compress-all",
        "deflate every file, even data that won't shrink (slower)",
        "--json",
    ] {
        assert!(
            help.to_lowercase().contains(&want.to_lowercase()),
            "help lacks {want:?}:\n{help}"
        );
    }
}
