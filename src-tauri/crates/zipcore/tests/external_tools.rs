//! Validation with independent ZIP implementations: Info-ZIP `unzip`/`zipinfo`,
//! Python's `zipfile`, and 7-Zip. Each check is skipped (with a note) when the
//! tool is not installed.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;
use zipcore::{Method, SymlinkPolicy};

const DEFLATE: Method = Method::Deflate { level: 6 };

/// A representative tree: the mixed tree plus unicode names, and on Unix an
/// executable and a symlink.
fn representative(parent: &Path) -> PathBuf {
    let root = mixed_tree(parent);
    write_file(&root.join("unicode/café.txt"), "café".as_bytes());
    write_file(&root.join("unicode/日本語.txt"), "日本語".as_bytes());
    write_file(&root.join("unicode/🎉 party.txt"), "🎉".as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let exe = root.join("bin/run.sh");
        write_file(&exe, b"#!/bin/sh\necho hi\n");
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("../a.txt", root.join("bin/link-to-a")).unwrap();
    }
    root
}

fn write_archive(dir: &Path, root: &Path, method: Method) -> PathBuf {
    let (zip, _) = compress_ok(
        &[root.to_path_buf()],
        &opts_full(method, SymlinkPolicy::Store, true),
    );
    let p = dir.join(format!("out-{method:?}.zip").replace([' ', '{', '}', ':'], ""));
    fs::write(&p, zip).unwrap();
    p
}

#[test]
fn unzip_tests_and_extracts_byte_for_byte() {
    let Some(unzip) = tool(&["unzip"], "unzip_tests_and_extracts_byte_for_byte") else {
        return;
    };
    let td = tempfile::tempdir().unwrap();
    let root = representative(&td.path().join("src"));
    for method in [Method::Store, DEFLATE] {
        let zip = write_archive(td.path(), &root, method);
        run_ok(Command::new(&unzip).arg("-t").arg(&zip));

        let dest = td
            .path()
            .join(format!("x-{}", matches!(method, Method::Store)));
        fs::create_dir(&dest).unwrap();
        run_ok(
            Command::new(&unzip)
                .arg("-q")
                .arg(&zip)
                .arg("-d")
                .arg(&dest),
        );
        assert_eq!(list_dir(&dest), vec!["root"]);
        let got = snapshot(&dest.join("root"));
        let want = snapshot(&root);
        assert_nodes_eq(&got, &want);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = fs::metadata(dest.join("root/bin/run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(m & 0o777, 0o755, "unzip did not restore the executable bit");
            let lt = fs::symlink_metadata(dest.join("root/bin/link-to-a")).unwrap();
            assert!(
                lt.file_type().is_symlink(),
                "unzip did not recreate the symlink"
            );
            assert_eq!(
                fs::read_link(dest.join("root/bin/link-to-a")).unwrap(),
                PathBuf::from("../a.txt")
            );
        }
    }
}

const PY_CHECK: &str = r#"
import sys, zipfile, struct
p = sys.argv[1]
with zipfile.ZipFile(p) as z:
    bad = z.testzip()
    print("testzip", bad)
    raw = open(p, "rb").read()
    for i in z.infolist():
        def ext_mtime(extra):
            j = 0
            while j + 4 <= len(extra):
                hid, ln = struct.unpack("<HH", extra[j:j+4])
                body = extra[j+4:j+4+ln]
                if hid == 0x5455 and len(body) >= 5 and body[0] & 1:
                    return struct.unpack("<i", body[1:5])[0]
                j += 4 + ln
            return None
        central = ext_mtime(i.extra)
        off = i.header_offset
        assert raw[off:off+4] == b"PK\x03\x04", "bad local header signature"
        nlen, xlen = struct.unpack("<HH", raw[off+26:off+30])
        local_name = raw[off+30:off+30+nlen]
        local = ext_mtime(raw[off+30+nlen:off+30+nlen+xlen])
        utf8 = bool(i.flag_bits & 0x800)
        nonascii = any(ord(c) > 127 for c in i.filename)
        print("entry", repr(i.filename), central, local, utf8, nonascii,
              local_name == i.filename.encode("utf-8" if utf8 else "cp437"))
"#;

#[test]
fn python_zipfile_validates_names_and_timestamps() {
    let Some(py) = python("python_zipfile_validates_names_and_timestamps") else {
        return;
    };
    let td = tempfile::tempdir().unwrap();
    let root = representative(&td.path().join("src"));
    let t = filetime::FileTime::from_unix_time(1_234_567_891, 0); // odd second
    filetime::set_file_mtime(root.join("a.txt"), t).unwrap();
    for method in [Method::Store, DEFLATE] {
        let zip = write_archive(td.path(), &root, method);
        let bytes = fs::read(&zip).unwrap();
        let ours = read_entries(&bytes);
        let out = run_python(&py, PY_CHECK, &[&zip]);
        let mut lines = out.lines();
        assert_eq!(
            lines.next(),
            Some("testzip None"),
            "python testzip failed:\n{out}"
        );
        let mut py_names = Vec::new();
        for line in lines {
            let rest = line.strip_prefix("entry ").expect(line);
            // repr(name) central local utf8 nonascii local_name_matches
            let (name_repr, tail) = split_repr(rest);
            let f: Vec<&str> = tail.split_whitespace().collect();
            let (central, local, utf8, nonascii, lname_ok) = (f[0], f[1], f[2], f[3], f[4]);
            assert_eq!(
                lname_ok, "True",
                "{name_repr}: local header name differs from central"
            );
            if nonascii == "True" {
                assert_eq!(
                    utf8, "True",
                    "{name_repr}: non-ASCII name without UTF-8 flag (bit 11)"
                );
            }
            assert_ne!(
                central, "None",
                "{name_repr}: no 0x5455 mtime in central directory"
            );
            if local != "None" {
                assert_eq!(local, central, "{name_repr}: local vs central 0x5455 mtime");
            }
            if name_repr == "'root/a.txt'" {
                assert_eq!(central, "1234567891", "root/a.txt exact mtime");
            }
            py_names.push(name_repr);
        }
        let want: Vec<String> = ours.iter().map(|e| py_repr(&e.name)).collect();
        assert_eq!(py_names, want, "python and zip crate disagree on names");
    }
}

/// Split `'name' rest` where the repr may contain spaces.
fn split_repr(s: &str) -> (String, &str) {
    let q = s.chars().next().unwrap();
    let mut end = 1;
    let b = s.as_bytes();
    while end < b.len() {
        if b[end] == b'\\' {
            end += 2;
            continue;
        }
        if b[end] as char == q {
            break;
        }
        end += 1;
    }
    (s[..=end].to_string(), &s[end + 1..])
}

/// Python's repr() for the simple strings we use (no quotes or backslashes).
fn py_repr(s: &str) -> String {
    assert!(!s.contains('\'') && !s.contains('\\'));
    format!("'{s}'")
}

#[test]
fn unicode_names_listed_by_unzip_and_zipinfo() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("uni");
    for p in ["café.txt", "日本語/ファイル.txt", "emoji 🎉.txt"] {
        write_file(&root.join(p), p.as_bytes());
    }
    let zip = write_archive(td.path(), &root, DEFLATE);
    let ours = names(&read_entries(&fs::read(&zip).unwrap()));

    let reference = python("unicode reference archive").map(|py| {
        // Same names written by Python (UTF-8 flag set): calibrates what this
        // build of unzip prints for a known-good archive.
        let refzip = td.path().join("reference.zip");
        let script = "import sys,zipfile\nz=zipfile.ZipFile(sys.argv[1],'w')\nfor n in sys.argv[2].split('\\n'):\n    z.writestr(n, b'')\nz.close()";
        let joined = ours.join("\n");
        let mut cmd = Command::new(py);
        cmd.args(["-I", "-c", script]).arg(&refzip).arg(&joined);
        run_ok(&mut cmd);
        refzip
    });

    let mut ran = false;
    for (tool_name, args) in [("unzip", &["-Z1"][..]), ("zipinfo", &["-1"][..])] {
        let Some(bin) = which(tool_name) else {
            eprintln!("NOTE: {tool_name} not found; skipping");
            continue;
        };
        ran = true;
        let list = |p: &Path| -> Vec<String> {
            let out = run_ok(Command::new(&bin).args(args).arg(p));
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        };
        let got = list(&zip);
        if got != ours {
            if let Some(r) = &reference {
                let ref_list = list(r);
                if ref_list != ours {
                    // This build mangles UTF-8 names even for a known-good
                    // archive; then ours must at least be listed identically.
                    eprintln!(
                        "NOTE: {tool_name} mangles UTF-8 names even for a Python-written \
                         archive; comparing against that listing instead"
                    );
                    assert_eq!(
                        got, ref_list,
                        "{tool_name} lists zipcore's names differently from the same names \
                         written by Python"
                    );
                    continue;
                }
            }
            panic!("{tool_name} {args:?} listed {got:?}, expected {ours:?}");
        }
    }
    if !ran {
        eprintln!("NOTE: skipping unicode_names_listed_by_unzip_and_zipinfo: no unzip/zipinfo");
    }

    if let Some(py) = python("unicode names via python") {
        let out = run_python(
            &py,
            "import sys,zipfile\nfor n in zipfile.ZipFile(sys.argv[1]).namelist(): print(n)",
            &[&zip],
        );
        let py_names: Vec<String> = out.lines().map(str::to_string).collect();
        assert_eq!(py_names, ours, "python zipfile decoded names differently");
    }
}

#[test]
fn seven_zip_tests_archive() {
    let Some(sz) = tool(&["7zz", "7z", "7za"], "seven_zip_tests_archive") else {
        return;
    };
    let td = tempfile::tempdir().unwrap();
    let root = representative(&td.path().join("src"));
    for method in [Method::Store, DEFLATE] {
        let zip = write_archive(td.path(), &root, method);
        let out = run_ok(Command::new(&sz).arg("t").arg(&zip));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("Everything is Ok"), "7z output:\n{stdout}");
    }
}
