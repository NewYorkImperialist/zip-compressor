//! `extract_archive`: round trips for every preset, archives from other
//! tools, and the safety rules (zip slip, symlinks, overwrite, damage,
//! unsupported entries), each checked to write nothing it shouldn't.

mod common;

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use filetime::FileTime;
use zipcore::{Error, ExtractOptions, ExtractSummary, Method, Overwrite, Preset, Progress};

fn xopts() -> ExtractOptions {
    ExtractOptions::default()
}

fn extract(zip: &Path, dest: &Path, opts: &ExtractOptions) -> zipcore::Result<ExtractSummary> {
    let cancel = AtomicBool::new(false);
    zipcore::extract_archive(zip, dest, opts, &mut |_| {}, &cancel)
}

fn extract_ok(zip: &Path, dest: &Path, opts: &ExtractOptions) -> ExtractSummary {
    extract(zip, dest, opts).unwrap_or_else(|e| panic!("extract {zip:?} failed: {e:?}"))
}

fn write_zip(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p
}

fn set_mtime(p: &Path, secs: i64) {
    let t = FileTime::from_unix_time(secs, 0);
    filetime::set_symlink_file_times(p, t, t).unwrap();
}

fn mtime(p: &Path) -> i64 {
    FileTime::from_last_modification_time(&fs::symlink_metadata(p).unwrap()).unix_seconds()
}

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
}

/// Every path under `root` (relative, forward slashes), including temp files.
fn all_paths(root: &Path) -> Vec<String> {
    if !root.exists() {
        return Vec::new();
    }
    snapshot(root).into_keys().collect()
}

/// No temporary file is left anywhere under `root`.
fn assert_no_temp(root: &Path) {
    let temps: Vec<_> = all_paths(root)
        .into_iter()
        .filter(|p| p.contains(".partial"))
        .collect();
    assert!(temps.is_empty(), "temporary files left behind: {temps:?}");
}

#[track_caller]
fn assert_unsafe(r: zipcore::Result<ExtractSummary>, what: &str) {
    match r {
        Err(Error::UnsafePath(_)) => {}
        other => panic!("{what}: expected UnsafePath, got {other:?}"),
    }
}

#[track_caller]
fn assert_corrupt(r: zipcore::Result<ExtractSummary>, what: &str) -> String {
    match r {
        Err(Error::Corrupt { entry, reason }) => format!("{entry}: {reason}"),
        other => panic!("{what}: expected Corrupt, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Hand-built archives (any name, method, flags, host and attributes).
// ---------------------------------------------------------------------------

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

const UNIX: u16 = 0x0314; // host 3, spec 2.0
const DOS: u16 = 0x0014;

#[derive(Clone)]
struct Raw {
    name: Vec<u8>,
    method: u16,
    flags: u16,
    /// Bytes stored as the entry's data.
    payload: Vec<u8>,
    crc: u32,
    usize: u64,
    made_by: u16,
    attrs: u32,
    /// Central extra field.
    extra: Vec<u8>,
    /// Data descriptor (flag bit 3) after the data.
    descriptor: bool,
}

impl Raw {
    fn stored(name: &str, data: &[u8]) -> Raw {
        Raw {
            name: name.as_bytes().to_vec(),
            method: 0,
            flags: 0,
            payload: data.to_vec(),
            crc: crc32fast::hash(data),
            usize: data.len() as u64,
            made_by: DOS,
            attrs: 0,
            extra: Vec::new(),
            descriptor: false,
        }
    }
    fn deflated(name: &str, data: &[u8]) -> Raw {
        Raw {
            method: 8,
            payload: deflate(data),
            ..Raw::stored(name, data)
        }
    }
    fn dir(name: &str) -> Raw {
        Raw {
            attrs: 0x10,
            ..Raw::stored(name, b"")
        }
    }
    #[cfg_attr(not(unix), allow(dead_code))]
    fn symlink(name: &str, target: &str) -> Raw {
        Raw::stored(name, target.as_bytes()).unix(0o120777)
    }
    fn unix(mut self, mode: u32) -> Raw {
        self.made_by = UNIX;
        self.attrs = mode << 16;
        self
    }
    fn ext_mtime(mut self, secs: i32) -> Raw {
        self.extra.extend_from_slice(&0x5455u16.to_le_bytes());
        self.extra.extend_from_slice(&5u16.to_le_bytes());
        self.extra.push(1);
        self.extra.extend_from_slice(&secs.to_le_bytes());
        self
    }
}

fn p16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn p32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// DOS time/date written for every hand-built entry: 2020-05-17 10:20:30.
const DOS_TIME: u16 = (10 << 11) | (20 << 5) | 15;
const DOS_DATE: u16 = (40 << 9) | (5 << 5) | 17;

fn build(entries: &[Raw]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offsets = Vec::new();
    for e in entries {
        offsets.push(out.len() as u32);
        let csize = e.payload.len() as u32;
        let flags = e.flags | if e.descriptor { 8 } else { 0 };
        p32(&mut out, 0x0403_4b50);
        p16(&mut out, 20);
        p16(&mut out, flags);
        p16(&mut out, e.method);
        p16(&mut out, DOS_TIME);
        p16(&mut out, DOS_DATE);
        let (crc, c, u) = if e.descriptor {
            (0, 0, 0)
        } else {
            (e.crc, csize, e.usize as u32)
        };
        p32(&mut out, crc);
        p32(&mut out, c);
        p32(&mut out, u);
        p16(&mut out, e.name.len() as u16);
        p16(&mut out, 0);
        out.extend_from_slice(&e.name);
        out.extend_from_slice(&e.payload);
        if e.descriptor {
            p32(&mut out, 0x0807_4b50);
            p32(&mut out, e.crc);
            p32(&mut out, csize);
            p32(&mut out, e.usize as u32);
        }
    }
    let cd_start = out.len() as u32;
    for (e, off) in entries.iter().zip(&offsets) {
        p32(&mut out, 0x0201_4b50);
        p16(&mut out, e.made_by);
        p16(&mut out, 20);
        p16(&mut out, e.flags | if e.descriptor { 8 } else { 0 });
        p16(&mut out, e.method);
        p16(&mut out, DOS_TIME);
        p16(&mut out, DOS_DATE);
        p32(&mut out, e.crc);
        p32(&mut out, e.payload.len() as u32);
        p32(&mut out, e.usize as u32);
        p16(&mut out, e.name.len() as u16);
        p16(&mut out, e.extra.len() as u16);
        p16(&mut out, 0);
        p16(&mut out, 0);
        p16(&mut out, 0);
        p32(&mut out, e.attrs);
        p32(&mut out, *off);
        out.extend_from_slice(&e.name);
        out.extend_from_slice(&e.extra);
    }
    let cd_size = out.len() as u32 - cd_start;
    p32(&mut out, 0x0605_4b50);
    p16(&mut out, 0);
    p16(&mut out, 0);
    p16(&mut out, entries.len() as u16);
    p16(&mut out, entries.len() as u16);
    p32(&mut out, cd_size);
    p32(&mut out, cd_start);
    p16(&mut out, 0);
    out
}

/// Offset of `needle` in `hay`.
fn find_bytes(hay: &[u8], needle: &[u8]) -> usize {
    hay.windows(needle.len())
        .position(|w| w == needle)
        .expect("needle not found")
}

// ---------------------------------------------------------------------------
// Round trips.
// ---------------------------------------------------------------------------

/// A tree with every kind of thing the writer stores, mtimes and (on Unix)
/// permissions set to known values.
fn rich_tree(parent: &Path) -> PathBuf {
    let root = mixed_tree(parent);
    write_file(&root.join("ünïcødé/文件 ✓.txt"), "unicode ✓\n".as_bytes());
    write_file(
        &root.join("big/chunked.bin"),
        &random_bytes(5 * MIB + 4321, 3),
    );
    write_file(&root.join("bin/tool.sh"), b"#!/bin/sh\necho hi\n");
    write_file(&root.join("private.txt"), b"secret\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let set = |p: &str, m: u32| {
            fs::set_permissions(root.join(p), fs::Permissions::from_mode(m)).unwrap()
        };
        set("bin/tool.sh", 0o755);
        set("private.txt", 0o600);
        set("deep/x", 0o700);
        symlink("a.txt", root.join("link_to_a")).unwrap();
        symlink("../a.txt", root.join("deep/up_a")).unwrap();
        symlink("..", root.join("deep/x/parent")).unwrap();
    }
    // Files first, then directories bottom-up (setting a file's time doesn't
    // touch its directory's mtime, but creating files does).
    let mut t = unix_secs(2022, 1, 2, 3, 4, 5);
    let snap = snapshot(&root);
    for (rel, node) in &snap {
        if *node != Node::Dir {
            set_mtime(&root.join(rel), t);
            t += 3601;
        }
    }
    for (rel, node) in snap.iter().rev() {
        if *node == Node::Dir {
            set_mtime(&root.join(rel), t);
            t += 7;
        }
    }
    set_mtime(&root, t);
    root
}

/// The extracted copy of `src` (under `dest`) matches it: contents, links,
/// empty directories, mtimes to the second, and Unix permission bits.
fn assert_same_tree(src: &Path, out: &Path, how: &str) {
    assert_nodes_eq(&snapshot(out), &snapshot(src));
    for (rel, node) in snapshot(src) {
        let (a, b) = (src.join(&rel), out.join(&rel));
        assert_eq!(mtime(&b), mtime(&a), "{how}: mtime of {rel}");
        #[cfg(unix)]
        if !matches!(node, Node::Symlink(_)) {
            assert_eq!(mode(&b), mode(&a), "{how}: mode of {rel}");
        }
        let _ = node;
    }
    assert_eq!(mtime(out), mtime(src), "{how}: mtime of the top directory");
}

#[test]
fn round_trip_every_preset() {
    let td = tempfile::tempdir().unwrap();
    let root = rich_tree(td.path());
    let expected = snapshot(&root);
    let n_files = expected
        .values()
        .filter(|n| matches!(n, Node::File(_)))
        .count() as u64;
    let n_links = expected
        .values()
        .filter(|n| matches!(n, Node::Symlink(_)))
        .count() as u64;
    let n_dirs = expected.values().filter(|n| **n == Node::Dir).count() as u64 + 1;
    let bytes: u64 = expected
        .values()
        .map(|n| match n {
            Node::File(d) => d.len() as u64,
            _ => 0,
        })
        .sum();
    for preset in Preset::ALL {
        let zip = td.path().join(format!("{}.zip", preset.name()));
        compress_path_ok(std::slice::from_ref(&root), &zip, &opts(preset.method()));
        let dest = td.path().join(format!("out-{}", preset.name()));
        let s = extract_ok(&zip, &dest, &xopts());
        assert_same_tree(&root, &dest.join("root"), preset.name());
        assert_eq!(s.files, n_files, "{preset:?}: files");
        assert_eq!(s.symlinks, n_links, "{preset:?}: symlinks");
        assert_eq!(s.dirs, n_dirs, "{preset:?}: dirs");
        assert_eq!((s.skipped, s.bytes_out), (0, bytes), "{preset:?}");
        assert_eq!(list_dir(&dest), vec!["root"]);
        assert_no_temp(&dest);
    }
}

#[test]
fn store_and_thread_counts_give_the_same_tree() {
    let td = tempfile::tempdir().unwrap();
    let root = rich_tree(td.path());
    let zip = td.path().join("s.zip");
    compress_path_ok(std::slice::from_ref(&root), &zip, &opts(Method::Store));
    for threads in [1, 2, 7] {
        let dest = td.path().join(format!("t{threads}"));
        let o = ExtractOptions { threads, ..xopts() };
        extract_ok(&zip, &dest, &o);
        assert_same_tree(&root, &dest.join("root"), &format!("{threads} threads"));
    }
}

#[test]
fn attributes_not_restored_when_off() {
    let td = tempfile::tempdir().unwrap();
    let root = rich_tree(td.path());
    let zip = td.path().join("a.zip");
    compress_path_ok(std::slice::from_ref(&root), &zip, &opts(Method::Store));
    let dest = td.path().join("out");
    let o = ExtractOptions {
        restore_permissions: false,
        restore_mtime: false,
        symlinks: false,
        ..xopts()
    };
    let s = extract_ok(&zip, &dest, &o);
    assert_eq!(s.symlinks, 0);
    let out = dest.join("root");
    assert!(
        fs::symlink_metadata(out.join("link_to_a")).is_err(),
        "links skipped"
    );
    let recent = mtime(&out.join("a.txt"));
    assert!(
        recent > unix_secs(2025, 1, 1, 0, 0, 0),
        "mtime is extraction time"
    );
    #[cfg(unix)]
    {
        assert_eq!(mode(&out.join("deep/x")) & 0o700, 0o700);
        assert_ne!(
            mode(&out.join("private.txt")),
            0o600,
            "umask default, not 0600"
        );
    }
}

#[test]
fn progress_reports_files_and_bytes() {
    let td = tempfile::tempdir().unwrap();
    let root = mixed_tree(td.path());
    write_file(&root.join("big.bin"), &random_bytes(9 * MIB, 5));
    let zip = td.path().join("p.zip");
    compress_path_ok(
        std::slice::from_ref(&root),
        &zip,
        &opts(Method::Deflate { level: 1 }),
    );
    let cancel = AtomicBool::new(false);
    let mut seen: Vec<Progress> = Vec::new();
    let dest = td.path().join("out");
    let s = zipcore::extract_archive(
        &zip,
        &dest,
        &xopts(),
        &mut |p| seen.push(p.clone()),
        &cancel,
    )
    .unwrap();
    let last = seen.last().unwrap();
    assert_eq!(last.files_done, last.files_total);
    assert_eq!(last.files_total, s.files + s.symlinks);
    assert_eq!(
        (last.bytes_done, last.bytes_total),
        (s.bytes_out, s.bytes_out)
    );
    assert!(seen
        .windows(2)
        .all(|w| w[0].bytes_done <= w[1].bytes_done && w[0].files_done <= w[1].files_done));
    assert!(seen.iter().any(|p| p.current == "root/big.bin"));
}

#[test]
fn empty_archive_and_missing_dest() {
    let td = tempfile::tempdir().unwrap();
    let zip = write_zip(td.path(), "e.zip", &build(&[]));
    let dest = td.path().join("new/nested");
    let s = extract_ok(&zip, &dest, &xopts());
    assert_eq!((s.files, s.dirs, s.bytes_out), (0, 0, 0));
    assert!(dest.is_dir(), "dest created");

    // A file where the destination should be.
    let file = td.path().join("file");
    fs::write(&file, b"x").unwrap();
    assert!(matches!(
        extract(&zip, &file, &xopts()),
        Err(Error::Io { .. })
    ));
    // Not an archive.
    assert!(matches!(
        extract(&file, &dest, &xopts()),
        Err(Error::InvalidArchive(_))
    ));
}

// ---------------------------------------------------------------------------
// Archives from other writers.
// ---------------------------------------------------------------------------

#[test]
fn zip_crate_archive_extracts() {
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, DateTime};
    let td = tempfile::tempdir().unwrap();
    let when = DateTime::from_date_and_time(2021, 6, 15, 12, 34, 56).unwrap();
    let base = SimpleFileOptions::default().last_modified_time(when);
    let defl = base.compression_method(CompressionMethod::Deflated);
    let stored = base.compression_method(CompressionMethod::Stored);
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    w.add_directory("docs/", base.unix_permissions(0o750))
        .unwrap();
    w.start_file("docs/readme.txt", defl.unix_permissions(0o640))
        .unwrap();
    w.write_all(&compressible_bytes(30_000)).unwrap();
    w.start_file("run.sh", stored.unix_permissions(0o755))
        .unwrap();
    w.write_all(b"#!/bin/sh\n").unwrap();
    w.start_file("empty", defl).unwrap();
    w.start_file("nested/implicit/dirs.bin", defl).unwrap();
    w.write_all(&random_bytes(100_000, 9)).unwrap();
    w.add_symlink("docs/link", "readme.txt", base).unwrap();
    let bytes = w.finish().unwrap().into_inner();
    let zip = write_zip(td.path(), "z.zip", &bytes);

    let dest = td.path().join("out");
    let s = extract_ok(&zip, &dest, &xopts());
    let mut expected = std::collections::BTreeMap::new();
    expected.insert("docs".to_string(), Node::Dir);
    expected.insert(
        "docs/readme.txt".to_string(),
        Node::File(compressible_bytes(30_000)),
    );
    expected.insert("run.sh".to_string(), Node::File(b"#!/bin/sh\n".to_vec()));
    expected.insert("empty".to_string(), Node::File(Vec::new()));
    expected.insert("nested".to_string(), Node::Dir);
    expected.insert("nested/implicit".to_string(), Node::Dir);
    expected.insert(
        "nested/implicit/dirs.bin".to_string(),
        Node::File(random_bytes(100_000, 9)),
    );
    if cfg!(unix) {
        expected.insert(
            "docs/link".to_string(),
            Node::Symlink(PathBuf::from("readme.txt")),
        );
    }
    assert_nodes_eq(&snapshot(&dest), &expected);
    assert_eq!((s.files, s.dirs), (4, 1));
    #[cfg(unix)]
    {
        assert_eq!(s.symlinks, 1);
        assert_eq!(mode(&dest.join("docs")), 0o750);
        assert_eq!(mode(&dest.join("docs/readme.txt")), 0o640);
        assert_eq!(mode(&dest.join("run.sh")), 0o755);
    }
    // No extended timestamp: the DOS time, read as local time. The zone is
    // unknown here, but the offset from UTC is a whole number of quarter hours.
    let naive = unix_secs(2021, 6, 15, 12, 34, 56);
    for p in ["docs/readme.txt", "run.sh", "docs"] {
        let d = mtime(&dest.join(p)) - naive;
        assert!(d.abs() <= 14 * 3600 && d % 900 == 0, "{p}: off by {d}s");
    }
}

#[test]
fn hand_built_variants_extract() {
    let td = tempfile::tempdir().unwrap();
    let text = compressible_bytes(20_000);
    let mut desc = Raw::deflated("desc.txt", b"with a data descriptor");
    desc.descriptor = true;
    let entries = vec![
        // Old Windows tools: backslash separators, DOS host.
        Raw::deflated("win\\sub\\file.txt", &text),
        Raw::dir("win\\empty\\"),
        desc,
        Raw::stored("./dot/./slash//name.txt", b"normalized"),
        Raw::stored("exact.txt", b"t").ext_mtime(1_600_000_000),
        Raw::stored("suid", b"s").unix(0o104755),
        Raw::stored("dir_mode/", b"").unix(0o040555),
        Raw::stored("dir_mode/inside.txt", b"in a read-only dir"),
        // An empty deflated file with no deflate stream at all.
        Raw {
            method: 8,
            payload: Vec::new(),
            ..Raw::stored("empty-deflate", b"")
        },
    ];
    let zip = write_zip(td.path(), "h.zip", &build(&entries));
    let dest = td.path().join("out");
    let s = extract_ok(&zip, &dest, &xopts());
    assert_eq!((s.files, s.dirs), (7, 2));
    let mut expected = std::collections::BTreeMap::new();
    for (k, v) in [
        ("win", Node::Dir),
        ("win/sub", Node::Dir),
        ("win/sub/file.txt", Node::File(text.clone())),
        ("win/empty", Node::Dir),
        ("desc.txt", Node::File(b"with a data descriptor".to_vec())),
        ("dot", Node::Dir),
        ("dot/slash", Node::Dir),
        ("dot/slash/name.txt", Node::File(b"normalized".to_vec())),
        ("exact.txt", Node::File(b"t".to_vec())),
        ("suid", Node::File(b"s".to_vec())),
        ("dir_mode", Node::Dir),
        (
            "dir_mode/inside.txt",
            Node::File(b"in a read-only dir".to_vec()),
        ),
        ("empty-deflate", Node::File(Vec::new())),
    ] {
        expected.insert(k.to_string(), v);
    }
    assert_nodes_eq(&snapshot(&dest), &expected);
    assert_eq!(mtime(&dest.join("exact.txt")), 1_600_000_000, "0x5455 wins");
    #[cfg(unix)]
    {
        assert_eq!(mode(&dest.join("suid")), 0o755, "setuid dropped");
        assert_eq!(
            mode(&dest.join("dir_mode")),
            0o555,
            "dir mode set after its contents"
        );
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dest.join("dir_mode"), fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Info-ZIP `zip -r -y` (stores symlinks), when installed.
#[test]
fn info_zip_archive_extracts() {
    let Some(zip_tool) = tool(&["zip"], "info_zip_archive_extracts") else {
        return;
    };
    let td = tempfile::tempdir().unwrap();
    let root = rich_tree(td.path());
    let zip = td.path().join("iz.zip");
    run_ok(
        std::process::Command::new(zip_tool)
            .current_dir(td.path())
            .args(["-r", "-y", "-q"])
            .arg(&zip)
            .arg("root"),
    );
    let dest = td.path().join("out");
    extract_ok(&zip, &dest, &xopts());
    assert_nodes_eq(&snapshot(&dest.join("root")), &snapshot(&root));
    // Info-ZIP writes the extended timestamp for files and directories.
    assert_eq!(mtime(&dest.join("root/a.txt")), mtime(&root.join("a.txt")));
    #[cfg(unix)]
    assert_eq!(mode(&dest.join("root/bin/tool.sh")), 0o755);
}

// ---------------------------------------------------------------------------
// Zip slip and symlinks.
// ---------------------------------------------------------------------------

#[test]
fn zip_slip_names_are_refused_and_nothing_is_written() {
    let td = tempfile::tempdir().unwrap();
    let outside = td.path().join("x");
    for (i, bad) in [
        "../x",
        "/etc/x",
        "a/../../x",
        "a/../b",
        "C:\\x",
        "C:x",
        "..\\x",
        "a\\..\\..\\x",
        "\\\\server\\share\\x",
        "//server/share/x",
        "nul\0byte",
        "",
    ]
    .iter()
    .enumerate()
    {
        for bad_first in [false, true] {
            let mut entries = vec![
                Raw::stored("ok.txt", b"fine"),
                Raw::dir("okdir/"),
                Raw::stored(bad, b"evil"),
            ];
            if bad_first {
                entries.rotate_right(1);
            }
            let zip = write_zip(td.path(), &format!("slip{i}.zip"), &build(&entries));
            let dest = td.path().join(format!("dest{i}"));
            assert_unsafe(extract(&zip, &dest, &xopts()), &format!("{bad:?}"));
            assert!(!dest.exists(), "{bad:?}: nothing may be written");
            assert!(!outside.exists(), "{bad:?}: escaped");
            // Also when it isn't selected: every name is checked.
            let o = ExtractOptions {
                only: vec!["ok.txt".into()],
                ..xopts()
            };
            assert_unsafe(extract(&zip, &dest, &o), &format!("{bad:?} (only)"));
            assert!(!dest.exists());
        }
    }
}

#[cfg(unix)]
#[test]
fn symlink_escapes_are_refused() {
    let td = tempfile::tempdir().unwrap();
    let outside = td.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let cases: Vec<(&str, Vec<Raw>)> = vec![
        (
            "link out, then a file through it",
            vec![
                Raw::symlink("link", "../outside"),
                Raw::stored("link/evil", b"evil"),
            ],
        ),
        (
            "file through a link that points inside",
            vec![
                Raw::dir("inside/"),
                Raw::symlink("link", "inside"),
                Raw::stored("link/evil", b"evil"),
            ],
        ),
        (
            "file through a link, case differs",
            vec![
                Raw::dir("inside/"),
                Raw::symlink("Link", "inside"),
                Raw::stored(
                    if cfg!(target_os = "macos") {
                        "link/evil"
                    } else {
                        "Link/evil"
                    },
                    b"evil",
                ),
            ],
        ),
        ("absolute target", vec![Raw::symlink("abs", "/etc")]),
        ("too many ..", vec![Raw::symlink("a/b", "../../outside")]),
        (
            "deflated target",
            vec![Raw::deflated("d", b"../../outside").unix(0o120777)],
        ),
        ("backslash target", vec![Raw::symlink("bs", "..\\outside")]),
        (
            "link to . then .. through it",
            vec![Raw::symlink("here", "."), Raw::symlink("up", "here/..")],
        ),
    ];
    for (i, (what, entries)) in cases.into_iter().enumerate() {
        let zip = write_zip(td.path(), &format!("l{i}.zip"), &build(&entries));
        let dest = td.path().join(format!("dest{i}"));
        assert_unsafe(extract(&zip, &dest, &xopts()), what);
        assert!(!dest.exists(), "{what}: nothing may be written");
        assert!(list_dir(&outside).is_empty(), "{what}: escaped");
    }

    // With symlinks off, the link is skipped and `link/evil` is an ordinary
    // file in an ordinary directory.
    let zip = write_zip(
        td.path(),
        "off.zip",
        &build(&[
            Raw::symlink("link", "../outside"),
            Raw::stored("link/evil", b"harmless"),
        ]),
    );
    let dest = td.path().join("off");
    let o = ExtractOptions {
        symlinks: false,
        ..xopts()
    };
    let s = extract_ok(&zip, &dest, &o);
    assert_eq!((s.files, s.symlinks), (1, 0));
    assert!(fs::symlink_metadata(dest.join("link")).unwrap().is_dir());
    assert_eq!(fs::read(dest.join("link/evil")).unwrap(), b"harmless");
    assert!(list_dir(&outside).is_empty());

    // Links that stay inside are created as given (normalized).
    let zip = write_zip(
        td.path(),
        "in.zip",
        &build(&[
            Raw::stored("a/file", b"f"),
            Raw::symlink("a/b/up", "../file"),
            Raw::symlink("top", "./a//b/"),
            Raw::symlink("self", "."),
        ]),
    );
    let dest = td.path().join("in");
    let s = extract_ok(&zip, &dest, &xopts());
    assert_eq!(s.symlinks, 3);
    assert_eq!(
        fs::read_link(dest.join("a/b/up")).unwrap(),
        Path::new("../file")
    );
    assert_eq!(fs::read_link(dest.join("top")).unwrap(), Path::new("a/b"));
    assert_eq!(fs::read_link(dest.join("self")).unwrap(), Path::new("."));
    assert_eq!(fs::read(dest.join("a/b/up")).unwrap(), b"f");
}

/// A symlink already in the destination is never written through, whatever
/// the overwrite mode.
#[cfg(unix)]
#[test]
fn existing_symlinks_in_dest_are_not_followed() {
    let td = tempfile::tempdir().unwrap();
    let outside = td.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let dest = td.path().join("dest");
    fs::create_dir(&dest).unwrap();
    std::os::unix::fs::symlink(&outside, dest.join("pre")).unwrap();
    for (i, entries) in [
        vec![Raw::stored("pre/evil", b"evil")],
        vec![Raw::dir("pre/")],
        vec![Raw::dir("pre/sub/")],
    ]
    .into_iter()
    .enumerate()
    {
        let zip = write_zip(td.path(), &format!("p{i}.zip"), &build(&entries));
        for overwrite in [Overwrite::Never, Overwrite::Skip, Overwrite::Always] {
            let o = ExtractOptions {
                overwrite,
                ..xopts()
            };
            assert_unsafe(extract(&zip, &dest, &o), &format!("{i} {overwrite:?}"));
            assert!(list_dir(&outside).is_empty());
        }
    }
    // A link where a file goes is a conflict like any file: replaced (the
    // link itself, not its target) only under Always.
    fs::write(outside.join("target"), b"keep").unwrap();
    std::os::unix::fs::symlink(outside.join("target"), dest.join("f")).unwrap();
    let zip = write_zip(td.path(), "f.zip", &build(&[Raw::stored("f", b"new")]));
    assert!(matches!(
        extract(&zip, &dest, &xopts()),
        Err(Error::AlreadyExists(_))
    ));
    let o = ExtractOptions {
        overwrite: Overwrite::Always,
        ..xopts()
    };
    extract_ok(&zip, &dest, &o);
    assert!(fs::symlink_metadata(dest.join("f")).unwrap().is_file());
    assert_eq!(fs::read(dest.join("f")).unwrap(), b"new");
    assert_eq!(fs::read(outside.join("target")).unwrap(), b"keep");
}

// ---------------------------------------------------------------------------
// Overwrite policies, duplicates, `only`.
// ---------------------------------------------------------------------------

#[test]
fn overwrite_never_skip_always() {
    let td = tempfile::tempdir().unwrap();
    let root = mixed_tree(td.path());
    let zip = td.path().join("o.zip");
    compress_path_ok(
        std::slice::from_ref(&root),
        &zip,
        &opts(Method::Deflate { level: 6 }),
    );
    let dest = td.path().join("out");
    let first = extract_ok(&zip, &dest, &xopts());
    let out = dest.join("root");

    // Change one file and delete another.
    fs::write(out.join("a.txt"), b"changed").unwrap();
    fs::remove_file(out.join("bin/random.dat")).unwrap();

    // Never: refused up front; the deleted file isn't recreated either.
    match extract(&zip, &dest, &xopts()) {
        Err(Error::AlreadyExists(p)) => assert!(p.starts_with(&out), "{p:?}"),
        other => panic!("expected AlreadyExists, got {other:?}"),
    }
    assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"changed");
    assert!(!out.join("bin/random.dat").exists());

    // Skip: existing files are left alone and counted; the missing one is
    // extracted.
    let s = extract_ok(
        &zip,
        &dest,
        &ExtractOptions {
            overwrite: Overwrite::Skip,
            ..xopts()
        },
    );
    assert_eq!(s.files, 1);
    assert_eq!(s.skipped, first.files - 1);
    assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"changed");
    assert_eq!(
        fs::read(out.join("bin/random.dat")).unwrap(),
        random_bytes(70_001, 7)
    );

    // Always: everything replaced.
    let s = extract_ok(
        &zip,
        &dest,
        &ExtractOptions {
            overwrite: Overwrite::Always,
            ..xopts()
        },
    );
    assert_eq!((s.files, s.skipped), (first.files, 0));
    assert_nodes_eq(&snapshot(&out), &snapshot(&root));
    assert_no_temp(&dest);
}

#[test]
fn overwrite_file_where_a_directory_goes() {
    let td = tempfile::tempdir().unwrap();
    let zip = write_zip(
        td.path(),
        "d.zip",
        &build(&[Raw::stored("d/f.txt", b"f"), Raw::stored("g", b"g")]),
    );
    let dest = td.path().join("out");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("d"), b"a file named d").unwrap();
    fs::create_dir(dest.join("g")).unwrap();

    assert!(matches!(
        extract(&zip, &dest, &xopts()),
        Err(Error::AlreadyExists(_))
    ));
    let skip = ExtractOptions {
        overwrite: Overwrite::Skip,
        ..xopts()
    };
    let s = extract_ok(&zip, &dest, &skip);
    assert_eq!((s.files, s.skipped), (0, 2));
    assert_eq!(fs::read(dest.join("d")).unwrap(), b"a file named d");

    // Always replaces the file `d` with a directory, but won't replace the
    // directory `g` with a file.
    let always = ExtractOptions {
        overwrite: Overwrite::Always,
        ..xopts()
    };
    match extract(&zip, &dest, &always) {
        Err(Error::Io { path, .. }) => assert_eq!(path, dest.join("g")),
        other => panic!("expected an Io error for g, got {other:?}"),
    }
    assert!(dest.join("d").is_file(), "nothing written before the error");
    fs::remove_dir(dest.join("g")).unwrap();
    extract_ok(&zip, &dest, &always);
    assert_eq!(fs::read(dest.join("d/f.txt")).unwrap(), b"f");
}

#[test]
fn duplicate_names_are_refused() {
    let td = tempfile::tempdir().unwrap();
    let cases: Vec<(&str, Vec<Raw>)> = vec![
        (
            "same file twice",
            vec![Raw::stored("a.txt", b"1"), Raw::stored("a.txt", b"2")],
        ),
        (
            "same after normalization",
            vec![
                Raw::stored("d/a.txt", b"1"),
                Raw::stored("d\\.\\a.txt", b"2"),
            ],
        ),
        (
            "file and directory",
            vec![Raw::stored("x", b"1"), Raw::dir("x/")],
        ),
        (
            "file needed as a directory",
            vec![Raw::stored("x", b"1"), Raw::stored("x/y", b"2")],
        ),
    ];
    for (i, (what, entries)) in cases.into_iter().enumerate() {
        let zip = write_zip(td.path(), &format!("d{i}.zip"), &build(&entries));
        let dest = td.path().join(format!("dest{i}"));
        let msg = assert_corrupt(extract(&zip, &dest, &xopts()), what);
        assert!(!dest.exists(), "{what}: nothing written ({msg})");
    }

    // Repeated directory entries merge.
    let zip = write_zip(
        td.path(),
        "dirs.zip",
        &build(&[Raw::dir("d/"), Raw::stored("d/f", b"f"), Raw::dir("d/")]),
    );
    let s = extract_ok(&zip, &td.path().join("dirs"), &xopts());
    assert_eq!((s.files, s.dirs), (1, 1));

    // Names differing only in case: the same file on macOS and Windows.
    let zip = write_zip(
        td.path(),
        "case.zip",
        &build(&[Raw::stored("README", b"1"), Raw::stored("readme", b"2")]),
    );
    let dest = td.path().join("case");
    let r = extract(&zip, &dest, &xopts());
    if cfg!(any(windows, target_os = "macos")) {
        let msg = assert_corrupt(r, "case-only duplicate");
        assert!(msg.contains("same path"), "{msg}");
    } else {
        assert_eq!(r.unwrap().files, 2);
    }
}

#[test]
fn only_selects_files_and_directories() {
    let td = tempfile::tempdir().unwrap();
    let root = mixed_tree(td.path());
    let zip = td.path().join("o.zip");
    compress_path_ok(
        std::slice::from_ref(&root),
        &zip,
        &opts(Method::Deflate { level: 6 }),
    );
    let run = |only: &[&str], name: &str| {
        let dest = td.path().join(name);
        let o = ExtractOptions {
            only: only.iter().map(|s| s.to_string()).collect(),
            ..xopts()
        };
        (extract(&zip, &dest, &o), all_paths(&dest))
    };

    let (r, paths) = run(&["root/a.txt"], "exact");
    assert_eq!(r.unwrap().files, 1);
    assert_eq!(paths, vec!["root", "root/a.txt"]);

    let (r, paths) = run(&["root/deep/", "root/bin/all_bytes"], "prefix");
    assert_eq!(r.unwrap().files, 2);
    assert_eq!(
        paths,
        vec![
            "root",
            "root/bin",
            "root/bin/all_bytes",
            "root/deep",
            "root/deep/x",
            "root/deep/x/y",
            "root/deep/x/y/z.txt",
        ]
    );

    // A prefix is a whole component: `root/a` doesn't select `root/a.txt`.
    let (r, paths) = run(&["root/a"], "partial");
    match r {
        Err(Error::NotFound(p)) => assert_eq!(p, PathBuf::from("root/a")),
        other => panic!("expected NotFound, got {other:?}"),
    }
    assert!(paths.is_empty(), "nothing written");

    let (r, _) = run(&["root/empty_dir"], "dir");
    let s = r.unwrap();
    assert_eq!((s.files, s.dirs), (0, 1));
}

// ---------------------------------------------------------------------------
// Damage, unsupported entries, cancel.
// ---------------------------------------------------------------------------

#[test]
fn corrupt_data_leaves_no_file() {
    let td = tempfile::tempdir().unwrap();
    let text = compressible_bytes(200_000);
    let random = random_bytes(150_000, 4);
    let base = vec![
        Raw::stored("good.txt", b"good"),
        Raw::deflated("text.txt", &text),
        Raw::stored("random.bin", &random),
    ];
    let clean = build(&base);
    // (what, damaged entry, offset into its data, xor mask)
    let damage = [
        ("flipped byte in deflate data", "text.txt", 500, 0x55u8),
        ("flipped byte in stored data", "random.bin", 100_000, 0x01),
    ];
    for (i, (what, name, offset, mask)) in damage.into_iter().enumerate() {
        let mut bytes = clean.clone();
        // The first occurrence of the name is in its local header, which the
        // data directly follows (no extra field).
        let at = find_bytes(&bytes, name.as_bytes()) + name.len() + offset;
        bytes[at] ^= mask;
        let zip = write_zip(td.path(), &format!("c{i}.zip"), &bytes);
        let dest = td.path().join(format!("dest{i}"));
        for threads in [1, 4] {
            let o = ExtractOptions { threads, ..xopts() };
            let msg = assert_corrupt(extract(&zip, &dest, &o), what);
            assert!(
                msg.contains("CRC") || msg.contains("compressed data"),
                "{msg}"
            );
            let paths = all_paths(&dest);
            assert!(
                !paths.iter().any(|p| p == name),
                "{what}: damaged file left behind: {paths:?}"
            );
            assert_no_temp(&dest);
            let _ = fs::remove_dir_all(&dest);
        }
    }

    // Declared sizes that don't match the data.
    let mut short = Raw::deflated("short.txt", &text);
    short.usize -= 1; // inflates to more than declared
    let mut long = Raw::stored("long.txt", b"abc");
    long.usize = 4;
    let mut trunc = Raw::deflated("trunc.txt", &text);
    trunc.payload.truncate(trunc.payload.len() / 2);
    for (i, r) in [short, long, trunc].into_iter().enumerate() {
        let zip = write_zip(td.path(), &format!("s{i}.zip"), &build(&[r]));
        let dest = td.path().join(format!("sdest{i}"));
        assert_corrupt(extract(&zip, &dest, &xopts()), &format!("size case {i}"));
        assert!(
            list_dir(&dest).is_empty(),
            "case {i}: {:?}",
            list_dir(&dest)
        );
    }

    // Overlapping entries (a non-recursive zip bomb): refused up front.
    let mut bytes = build(&[Raw::stored("a", b"aaaa"), Raw::stored("b", b"bbbb")]);
    let cd = find_bytes(&bytes, b"PK\x01\x02");
    let second = cd + find_bytes(&bytes[cd + 4..], b"PK\x01\x02") + 4;
    bytes[second + 42..second + 46].copy_from_slice(&0u32.to_le_bytes());
    let zip = write_zip(td.path(), "bomb.zip", &bytes);
    let dest = td.path().join("bomb");
    assert_corrupt(extract(&zip, &dest, &xopts()), "overlap");
    assert!(!dest.exists());
}

/// Damage in the middle of a big chunked entry of one of our archives: the
/// other files may be written, the damaged one never is.
#[test]
fn corrupt_big_entry_from_our_writer() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("t");
    write_file(&root.join("big.bin"), &compressible_bytes(6 * MIB));
    write_file(&root.join("small.txt"), b"small");
    let zip = td.path().join("b.zip");
    let (mut bytes, _) = compress_path_ok(
        std::slice::from_ref(&root),
        &zip,
        &opts(Method::Deflate { level: 6 }),
    );
    let raw = parse_raw_bytes(&bytes);
    let (cd, local) = raw
        .entries
        .iter()
        .find(|(cd, _)| cd.name == b"t/big.bin")
        .expect("big entry");
    let mid = (cd.offset + local.header_len + cd.csize / 2) as usize;
    bytes[mid] ^= 0xFF;
    fs::write(&zip, &bytes).unwrap();
    let dest = td.path().join("out");
    assert_corrupt(extract(&zip, &dest, &xopts()), "big entry");
    assert!(!dest.join("t/big.bin").exists());
    assert_no_temp(&dest);
}

#[test]
fn unsupported_entries_are_refused_up_front() {
    let td = tempfile::tempdir().unwrap();
    let mut bzip2 = Raw::stored("b.bz2", b"not really bzip2");
    bzip2.method = 12;
    let mut encrypted = Raw::stored("secret.txt", b"xxxxxxxxxxxxxxxxxxxx");
    encrypted.flags = 1;
    let mut aes = Raw::stored("aes.txt", b"xxxx");
    aes.method = 99;
    for (i, (bad, needle)) in [
        (bzip2, "BZIP2"),
        (encrypted, "encrypted"),
        (aes, "encrypted"),
    ]
    .into_iter()
    .enumerate()
    {
        let zip = write_zip(
            td.path(),
            &format!("u{i}.zip"),
            &build(&[Raw::stored("ok.txt", b"ok"), bad.clone()]),
        );
        let dest = td.path().join(format!("u{i}"));
        match extract(&zip, &dest, &xopts()) {
            Err(Error::Unsupported { entry, reason }) => {
                assert_eq!(entry.as_bytes(), &bad.name[..]);
                assert!(reason.contains(needle), "{reason}");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        assert!(!dest.exists(), "nothing written");
        // Not selected: no problem.
        let o = ExtractOptions {
            only: vec!["ok.txt".into()],
            ..xopts()
        };
        assert_eq!(extract_ok(&zip, &dest, &o).files, 1);
    }
}

#[test]
fn cancel_stops_and_leaves_only_complete_files() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("c");
    for i in 0..40 {
        write_file(
            &root.join(format!("f{i:02}.bin")),
            &random_bytes(512 * 1024 + i, i as u64),
        );
    }
    write_file(&root.join("huge.bin"), &compressible_bytes(24 * MIB));
    let zip = td.path().join("c.zip");
    compress_path_ok(
        std::slice::from_ref(&root),
        &zip,
        &opts(Method::Deflate { level: 1 }),
    );

    // Before starting: nothing at all.
    let cancel = AtomicBool::new(true);
    let dest = td.path().join("pre");
    let r = zipcore::extract_archive(&zip, &dest, &xopts(), &mut |_| {}, &cancel);
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(!dest.exists());

    // From the first progress report.
    for threads in [1, 3] {
        let cancel = AtomicBool::new(false);
        let dest = td.path().join(format!("mid{threads}"));
        let o = ExtractOptions { threads, ..xopts() };
        let mut calls = 0;
        let r = zipcore::extract_archive(
            &zip,
            &dest,
            &o,
            &mut |_| {
                calls += 1;
                cancel.store(true, Ordering::Relaxed);
            },
            &cancel,
        );
        assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
        assert!(calls >= 1);
        assert_no_temp(&dest);
        let done = snapshot(&dest.join("c"));
        let src = snapshot(&root);
        assert!(
            done.len() < src.len(),
            "cancel stopped early ({} files)",
            done.len()
        );
        for (k, v) in done {
            assert_eq!(Some(&v), src.get(&k), "{k}: incomplete or wrong");
        }
    }
}

/// Maximal-length names nested tens of thousands of levels deep: planning is
/// linear in the name length, so this fails (the OS refuses the path) or
/// succeeds quickly instead of grinding through depth x length work.
#[test]
fn deeply_nested_names_are_handled_quickly() {
    let td = tempfile::tempdir().unwrap();
    let deep = "d/".repeat(32_000);
    let entries: Vec<Raw> = (0..20)
        .map(|i| Raw::stored(&format!("{deep}f{i}"), b"x"))
        .chain([Raw::dir(&deep)])
        .collect();
    let zip = write_zip(td.path(), "deep.zip", &build(&entries));
    let dest = td.path().join("out");
    let out = dest.clone();
    // 64 KB names are over the 4096-byte path cap: refused up front, quickly,
    // on every platform (Windows long paths would otherwise let it build a
    // 16,000-level tree, quadratically slowly).
    let r = with_timeout(60, "deep names", move || {
        extract(&zip, &out, &xopts()).map(|s| s.files)
    });
    match r {
        Err(Error::Io { source, .. }) => {
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidFilename)
        }
        other => panic!("expected the path cap to refuse it, got {other:?}"),
    }
    assert!(!dest.exists(), "nothing may be written");
}
