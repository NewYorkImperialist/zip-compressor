//! `diff_archive`: comparing an archive with a directory.

mod common;

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use common::*;
use zipcore::{CompressOptions, DiffKind, DiffOptions, DiffReport, Error, Method};

fn diff(archive: &Path, dir: &Path, opts: &DiffOptions) -> zipcore::Result<DiffReport> {
    zipcore::diff_archive(archive, dir, opts, &mut |_| {}, &AtomicBool::new(false))
}

fn diff_ok(archive: &Path, dir: &Path, opts: &DiffOptions) -> DiffReport {
    diff(archive, dir, opts).unwrap_or_else(|e| panic!("diff failed: {e:?}"))
}

/// Paths that are not `Same`, with their kinds.
fn changes(r: &DiffReport) -> Vec<(String, DiffKind)> {
    r.entries
        .iter()
        .filter(|e| e.kind != DiffKind::Same)
        .map(|e| (e.path.clone(), e.kind))
        .collect()
}

fn entry<'a>(r: &'a DiffReport, path: &str) -> &'a zipcore::DiffEntry {
    r.entries
        .iter()
        .find(|e| e.path == path)
        .unwrap_or_else(|| panic!("{path} not in report: {:?}", r.entries))
}

fn detail(r: &DiffReport, path: &str) -> String {
    entry(r, path).detail.clone().unwrap_or_default()
}

/// `parent/data/` with a few files, a nested and an empty directory, an
/// empty file and (on Unix) a symlink.
fn tree(parent: &Path) -> PathBuf {
    let data = parent.join("data");
    write_file(&data.join("a.txt"), &compressible_bytes(20_000));
    write_file(&data.join("sub/b.bin"), &random_bytes(70_000, 3));
    write_file(&data.join("sub/deeper/c.txt"), b"hello");
    write_file(&data.join("empty"), b"");
    fs::create_dir_all(data.join("hollow")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("a.txt", data.join("link")).unwrap();
    data
}

/// Compress `inputs` (with `opts`) into `parent/out.zip`.
fn zip_of(parent: &Path, inputs: &[PathBuf], opts: &CompressOptions) -> PathBuf {
    let out = parent.join("out.zip");
    compress_path_ok(inputs, &out, opts);
    out
}

fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let td = tempfile::tempdir().unwrap();
    let data = tree(td.path());
    let zip = zip_of(
        td.path(),
        std::slice::from_ref(&data),
        &CompressOptions::default(),
    );
    (td, data, zip)
}

#[test]
fn identical_round_trip() {
    let (_td, data, zip) = setup();
    for full in [false, true] {
        let r = diff_ok(
            &zip,
            &data,
            &DiffOptions {
                full,
                compare_mode: true,
                compare_mtime: true,
                ..Default::default()
            },
        );
        assert!(r.is_identical(), "full={full}: {:?}", changes(&r));
        assert_eq!(r.prefix, "data/");
        let paths: Vec<&str> = r.entries.iter().map(|e| e.path.as_str()).collect();
        let mut expected = vec![
            "a.txt",
            "empty",
            "hollow/",
            "sub/",
            "sub/b.bin",
            "sub/deeper/",
            "sub/deeper/c.txt",
        ];
        if cfg!(unix) {
            expected.push("link");
        }
        expected.sort();
        assert_eq!(paths, expected);
    }
}

#[test]
fn changed_byte_same_size_is_found_by_crc() {
    let (_td, data, zip) = setup();
    let mut b = fs::read(data.join("sub/b.bin")).unwrap();
    b[1234] ^= 1;
    fs::write(data.join("sub/b.bin"), &b).unwrap();
    let r = diff_ok(&zip, &data, &DiffOptions::default());
    assert_eq!(
        changes(&r),
        [("sub/b.bin".to_string(), DiffKind::Different)]
    );
    assert!(detail(&r, "sub/b.bin").contains("CRC-32"), "{r:?}");
    assert!(!r.is_identical());

    let r = diff_ok(
        &zip,
        &data,
        &DiffOptions {
            full: true,
            ..Default::default()
        },
    );
    assert_eq!(
        changes(&r),
        [("sub/b.bin".to_string(), DiffKind::Different)]
    );
    assert_eq!(detail(&r, "sub/b.bin"), "contents differ at byte 1234");
}

#[test]
fn size_change() {
    let (_td, data, zip) = setup();
    fs::write(data.join("sub/deeper/c.txt"), b"hello!").unwrap();
    let r = diff_ok(&zip, &data, &DiffOptions::default());
    assert_eq!(
        changes(&r),
        [("sub/deeper/c.txt".to_string(), DiffKind::Different)]
    );
    assert_eq!(detail(&r, "sub/deeper/c.txt"), "size 5 vs 6");
}

#[test]
fn added_and_removed() {
    let (_td, data, zip) = setup();
    fs::remove_file(data.join("a.txt")).unwrap();
    #[cfg(unix)]
    fs::remove_file(data.join("link")).unwrap();
    fs::remove_dir_all(data.join("sub/deeper")).unwrap();
    write_file(&data.join("new/n.txt"), b"new");
    let r = diff_ok(&zip, &data, &DiffOptions::default());
    let mut expected = vec![
        ("a.txt".to_string(), DiffKind::OnlyInArchive),
        ("new/".to_string(), DiffKind::OnlyOnDisk),
        ("new/n.txt".to_string(), DiffKind::OnlyOnDisk),
        ("sub/deeper/".to_string(), DiffKind::OnlyInArchive),
        ("sub/deeper/c.txt".to_string(), DiffKind::OnlyInArchive),
    ];
    if cfg!(unix) {
        expected.push(("link".to_string(), DiffKind::OnlyInArchive));
        expected.sort_by(|a, b| a.0.cmp(&b.0));
    }
    assert_eq!(changes(&r), expected);
}

#[test]
fn type_mismatch() {
    let (_td, data, zip) = setup();
    fs::remove_file(data.join("empty")).unwrap();
    fs::create_dir(data.join("empty")).unwrap();
    fs::remove_dir(data.join("hollow")).unwrap();
    fs::write(data.join("hollow"), b"").unwrap();
    let r = diff_ok(&zip, &data, &DiffOptions::default());
    assert_eq!(
        changes(&r),
        [
            ("empty".to_string(), DiffKind::TypeMismatch),
            ("hollow/".to_string(), DiffKind::TypeMismatch),
        ]
    );
    assert_eq!(detail(&r, "empty"), "file in archive, directory on disk");
    assert_eq!(detail(&r, "hollow/"), "directory in archive, file on disk");
}

#[cfg(unix)]
#[test]
fn symlinks_compare_by_target() {
    let (_td, data, zip) = setup();
    // The link is compared as a link, not through it: changing what it
    // points at changes nothing.
    fs::write(data.join("a.txt"), compressible_bytes(20_000)).unwrap();
    assert!(diff_ok(&zip, &data, &DiffOptions::default()).is_identical());

    fs::remove_file(data.join("link")).unwrap();
    std::os::unix::fs::symlink("empty", data.join("link")).unwrap();
    let r = diff_ok(&zip, &data, &DiffOptions::default());
    assert_eq!(changes(&r), [("link".to_string(), DiffKind::Different)]);
    assert_eq!(detail(&r, "link"), "link target `a.txt` vs `empty`");

    fs::remove_file(data.join("link")).unwrap();
    fs::write(data.join("link"), b"a.txt").unwrap();
    let r = diff_ok(&zip, &data, &DiffOptions::default());
    assert_eq!(changes(&r), [("link".to_string(), DiffKind::TypeMismatch)]);
    assert_eq!(detail(&r, "link"), "symlink in archive, file on disk");

    // A directory symlink on disk is not descended into.
    fs::remove_file(data.join("link")).unwrap();
    std::os::unix::fs::symlink("sub", data.join("link")).unwrap();
    let r = diff_ok(&zip, &data, &DiffOptions::default());
    assert_eq!(changes(&r), [("link".to_string(), DiffKind::Different)]);
}

/// `extract out.zip -d data` makes `data/data/...`: comparing against that
/// destination must pick the archive root, not strip `data/`.
#[test]
fn prefix_detection_handles_an_extraction_destination() {
    let td = tempfile::tempdir().unwrap();
    let data = tree(td.path());
    let zip = zip_of(
        td.path(),
        std::slice::from_ref(&data),
        &CompressOptions::default(),
    );
    let dest = td.path().join("restore").join("data");
    zipcore::extract_archive(
        &zip,
        &dest,
        &zipcore::ExtractOptions::default(),
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    let r = diff_ok(&zip, &dest, &DiffOptions::default());
    assert_eq!(r.prefix, "");
    assert!(r.is_identical(), "{:?}", changes(&r));
    // The original folder still gets the `data/` prefix.
    assert_eq!(
        diff_ok(&zip, &data, &DiffOptions::default()).prefix,
        "data/"
    );
}

#[test]
fn prefix_detection_and_explicit_prefix() {
    let td = tempfile::tempdir().unwrap();
    let data = tree(td.path());

    // `compress data` -> everything under data/.
    let whole = zip_of(
        td.path(),
        std::slice::from_ref(&data),
        &CompressOptions::default(),
    );
    assert_eq!(
        diff_ok(&whole, &data, &DiffOptions::default()).prefix,
        "data/"
    );
    for p in ["data", "data/", "/data/"] {
        let r = diff_ok(
            &whole,
            &data,
            &DiffOptions {
                prefix: Some(p.into()),
                ..Default::default()
            },
        );
        assert!(r.is_identical(), "prefix {p}: {:?}", changes(&r));
        assert_eq!(r.prefix, "data/");
    }
    // Compared against the archive root, the directory's contents are
    // all one level down.
    let r = diff_ok(
        &whole,
        &data,
        &DiffOptions {
            prefix: Some(String::new()),
            ..Default::default()
        },
    );
    assert_eq!(r.prefix, "");
    assert_eq!(entry(&r, "data/").kind, DiffKind::OnlyInArchive);
    assert_eq!(entry(&r, "a.txt").kind, DiffKind::OnlyOnDisk);

    // The contents compressed individually -> the archive root.
    let inputs: Vec<PathBuf> = fs::read_dir(&data)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    let flat_zip = td.path().join("flat.zip");
    compress_path_ok(&inputs, &flat_zip, &CompressOptions::default());
    let r = diff_ok(&flat_zip, &data, &DiffOptions::default());
    assert_eq!(r.prefix, "");
    assert!(r.is_identical(), "{:?}", changes(&r));

    // A prefix that names a subdirectory compares just that.
    let r = diff_ok(
        &whole,
        &data.join("sub"),
        &DiffOptions {
            prefix: Some("data/sub".into()),
            ..Default::default()
        },
    );
    assert!(r.is_identical(), "{:?}", changes(&r));
    assert_eq!(r.entries.len(), 3, "{:?}", r.entries);
    // Detected by name: `sub` is not the archive's top folder.
    let r = diff_ok(&whole, &data.join("sub"), &DiffOptions::default());
    assert_eq!(r.prefix, "");
    assert!(!r.is_identical());

    let r = diff(
        &whole,
        &data,
        &DiffOptions {
            prefix: Some("../x".into()),
            ..Default::default()
        },
    );
    assert!(matches!(r, Err(Error::UnsafePath(_))), "{r:?}");
}

#[test]
fn archives_without_directory_entries() {
    use zip::write::SimpleFileOptions;
    let td = tempfile::tempdir().unwrap();
    let data = td.path().join("data");
    write_file(&data.join("x/y/z.txt"), b"zzz");
    write_file(&data.join("top.txt"), b"top");
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let o = SimpleFileOptions::default();
    w.start_file("data/x/y/z.txt", o).unwrap();
    w.write_all(b"zzz").unwrap();
    w.start_file("data/top.txt", o).unwrap();
    w.write_all(b"top").unwrap();
    let zip = td.path().join("z.zip");
    fs::write(&zip, w.finish().unwrap().into_inner()).unwrap();
    let r = diff_ok(&zip, &data, &DiffOptions::default());
    assert!(r.is_identical(), "{:?}", r.entries);
    assert_eq!(entry(&r, "x/y/").kind, DiffKind::Same);
}

#[test]
fn full_mode_reads_the_data() {
    let (_td, data, zip) = setup();
    let r = diff_ok(
        &zip,
        &data,
        &DiffOptions {
            full: true,
            threads: 1,
            ..Default::default()
        },
    );
    assert!(r.is_identical());
    let mut a = compressible_bytes(20_000);
    a[19_999] ^= 1;
    fs::write(data.join("a.txt"), a).unwrap();
    let r = diff_ok(
        &zip,
        &data,
        &DiffOptions {
            full: true,
            ..Default::default()
        },
    );
    assert_eq!(changes(&r), [("a.txt".to_string(), DiffKind::Different)]);
    assert_eq!(detail(&r, "a.txt"), "contents differ at byte 19999");
}

#[cfg(unix)]
#[test]
fn mode_differences() {
    use std::os::unix::fs::PermissionsExt;
    let (_td, data, zip) = setup();
    fs::set_permissions(data.join("a.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(diff_ok(&zip, &data, &DiffOptions::default()).is_identical());
    let r = diff_ok(
        &zip,
        &data,
        &DiffOptions {
            compare_mode: true,
            ..Default::default()
        },
    );
    assert_eq!(
        changes(&r),
        [("a.txt".to_string(), DiffKind::MetadataDiffers)]
    );
    let d = detail(&r, "a.txt");
    assert!(d.starts_with("mode 0") && d.ends_with(" vs 0600"), "{d}");
}

fn set_mtime(path: &Path, t: SystemTime) {
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(t)).unwrap();
}

#[test]
fn mtime_exact_with_extended_timestamp() {
    let td = tempfile::tempdir().unwrap();
    let data = td.path().join("data");
    write_file(&data.join("f.txt"), b"f");
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_001);
    set_mtime(&data.join("f.txt"), t);
    let zip = zip_of(
        td.path(),
        std::slice::from_ref(&data),
        &CompressOptions::default(),
    );
    let mtime = DiffOptions {
        compare_mtime: true,
        ..Default::default()
    };
    assert!(diff_ok(&zip, &data, &mtime).is_identical());

    // Our archives carry the exact second, so one second off is a difference.
    set_mtime(&data.join("f.txt"), t + Duration::from_secs(1));
    assert!(diff_ok(&zip, &data, &DiffOptions::default()).is_identical());
    let r = diff_ok(&zip, &data, &mtime);
    assert_eq!(
        changes(&r),
        [("f.txt".to_string(), DiffKind::MetadataDiffers)]
    );
    assert_eq!(
        detail(&r, "f.txt"),
        "mtime 2023-11-14T22:13:21Z vs 2023-11-14T22:13:22Z"
    );
}

#[test]
fn mtime_dos_tolerance() {
    use zip::write::SimpleFileOptions;
    let td = tempfile::tempdir().unwrap();
    let data = td.path().join("data");
    write_file(&data.join("f.txt"), b"f");
    // An even second, stored as local DOS time only (no 0x5455 field).
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    let l = zipcore::local_time(t);
    let when = zip::DateTime::from_date_and_time(
        l.year as u16,
        l.month,
        l.day,
        l.hour,
        l.minute,
        l.second,
    )
    .unwrap();
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    w.start_file(
        "f.txt",
        SimpleFileOptions::default().last_modified_time(when),
    )
    .unwrap();
    w.write_all(b"f").unwrap();
    let zip = td.path().join("dos.zip");
    fs::write(&zip, w.finish().unwrap().into_inner()).unwrap();
    let mtime = DiffOptions {
        compare_mtime: true,
        ..Default::default()
    };
    for off in [0, 1] {
        set_mtime(
            &data.join("f.txt"),
            t + Duration::from_millis(off * 1000 + 500),
        );
        let r = diff_ok(&zip, &data, &mtime);
        assert!(r.is_identical(), "+{off}.5 s: {:?}", r.entries);
    }
    set_mtime(&data.join("f.txt"), t + Duration::from_secs(3));
    let r = diff_ok(&zip, &data, &mtime);
    assert_eq!(
        changes(&r),
        [("f.txt".to_string(), DiffKind::MetadataDiffers)]
    );
}

#[test]
fn junk_is_ignored_on_both_sides() {
    let td = tempfile::tempdir().unwrap();
    let data = td.path().join("data");
    write_file(&data.join("a.txt"), b"a");
    write_file(&data.join(".DS_Store"), b"junk");
    write_file(&data.join("__MACOSX/._a.txt"), b"junk");
    let keep = CompressOptions {
        exclude_junk: false,
        ..Default::default()
    };
    let zip = zip_of(td.path(), std::slice::from_ref(&data), &keep);
    // Clutter that differs, or exists on one side only.
    fs::write(data.join(".DS_Store"), b"other junk").unwrap();
    write_file(&data.join("sub/Thumbs.db"), b"junk");
    let r = diff_ok(&zip, &data, &DiffOptions::default());
    assert_eq!(changes(&r), [("sub/".to_string(), DiffKind::OnlyOnDisk)]);

    let r = diff_ok(
        &zip,
        &data,
        &DiffOptions {
            exclude_junk: false,
            ..Default::default()
        },
    );
    assert_eq!(
        changes(&r),
        [
            (".DS_Store".to_string(), DiffKind::Different),
            ("sub/".to_string(), DiffKind::OnlyOnDisk),
            ("sub/Thumbs.db".to_string(), DiffKind::OnlyOnDisk),
        ]
    );
}

#[test]
fn exclude_patterns_apply_to_both_sides() {
    let (_td, data, zip) = setup();
    write_file(&data.join("sub/debug.log"), b"log");
    write_file(&data.join("build/out.o"), b"obj");
    fs::write(data.join("sub/deeper/c.txt"), b"changed").unwrap();
    let r = diff_ok(
        &zip,
        &data,
        &DiffOptions {
            exclude: vec!["*.log".into(), "build/".into(), "/sub/deeper".into()],
            ..Default::default()
        },
    );
    assert!(r.is_identical(), "{:?}", changes(&r));
    assert!(r.entries.iter().all(|e| !e.path.starts_with("sub/deeper")));
}

/// Offset of the local header of `name` and of its data.
fn local_offsets(bytes: &[u8], name: &str) -> (usize, usize) {
    let mut i = 0;
    loop {
        let h = &bytes[i..];
        assert_eq!(&h[..4], b"PK\x03\x04", "entry {name} not found");
        let n = u16::from_le_bytes([h[26], h[27]]) as usize;
        let x = u16::from_le_bytes([h[28], h[29]]) as usize;
        let c = u32::from_le_bytes(h[18..22].try_into().unwrap()) as usize;
        if &h[30..30 + n] == name.as_bytes() {
            return (i, i + 30 + n + x);
        }
        i += 30 + n + x + c;
    }
}

#[test]
fn damaged_entries_are_different() {
    let (td, data, zip) = setup();
    let good = fs::read(&zip).unwrap();

    // Damaged data: only a full comparison reads it.
    let mut b = good.clone();
    let (_, data_at) = local_offsets(&b, "data/sub/b.bin");
    b[data_at + 100] ^= 0xff;
    let bad = td.path().join("bad-data.zip");
    fs::write(&bad, &b).unwrap();
    let r = diff_ok(&bad, &data, &DiffOptions::default());
    assert!(r.is_identical(), "the central CRC still matches the file");
    let r = diff_ok(
        &bad,
        &data,
        &DiffOptions {
            full: true,
            ..Default::default()
        },
    );
    assert_eq!(
        changes(&r),
        [("sub/b.bin".to_string(), DiffKind::Different)]
    );
    let d = detail(&r, "sub/b.bin");
    assert!(
        d.contains("damaged") || d.starts_with("contents differ"),
        "{d}"
    );

    // A damaged local header is noticed either way.
    let mut b = good.clone();
    let (lh, _) = local_offsets(&b, "data/a.txt");
    b[lh + 30] = b'X';
    let bad = td.path().join("bad-header.zip");
    fs::write(&bad, &b).unwrap();
    for full in [false, true] {
        let r = diff_ok(
            &bad,
            &data,
            &DiffOptions {
                full,
                ..Default::default()
            },
        );
        assert_eq!(changes(&r), [("a.txt".to_string(), DiffKind::Different)]);
        let d = detail(&r, "a.txt");
        assert!(d.starts_with("archive entry is damaged: "), "{d}");
    }
}

#[test]
fn unsafe_names_do_not_escape() {
    let td = tempfile::tempdir().unwrap();
    let data = td.path().join("data");
    write_file(&data.join("ok.txt"), b"ok");
    // Outside the compared directory, where `data/../evil.txt` would point.
    write_file(&td.path().join("evil.txt"), b"evil");
    write_file(&td.path().join("src/data/ok.txt"), b"ok");
    write_file(&td.path().join("src/data/xx/evil.txt"), b"evil");
    let zip = zip_of(
        td.path(),
        &[td.path().join("src/data")],
        &CompressOptions::default(),
    );
    // Rename `data/xx/evil.txt` to `data/../evil.txt` in both headers.
    let mut b = fs::read(&zip).unwrap();
    let from = b"data/xx/evil.txt";
    let mut hits = 0;
    let mut i = 0;
    while i + from.len() <= b.len() {
        if &b[i..i + from.len()] == from {
            b[i + 5..i + 7].copy_from_slice(b"..");
            hits += 1;
        }
        i += 1;
    }
    assert_eq!(hits, 2);
    fs::write(&zip, &b).unwrap();

    let r = diff_ok(&zip, &data, &DiffOptions::default());
    assert_eq!(r.prefix, "data/");
    assert_eq!(
        changes(&r),
        [
            ("data/../evil.txt".to_string(), DiffKind::OnlyInArchive),
            ("xx/".to_string(), DiffKind::OnlyInArchive),
        ]
    );
    assert!(detail(&r, "data/../evil.txt").contains("unsafe"));
}

#[test]
fn unreadable_inputs_are_errors() {
    let (td, data, zip) = setup();
    let r = diff(&zip, &td.path().join("missing"), &DiffOptions::default());
    assert!(matches!(r, Err(Error::NotFound(_))), "{r:?}");
    let r = diff(&zip, &zip, &DiffOptions::default());
    assert!(matches!(r, Err(Error::Io { .. })), "{r:?}");
    let not_zip = td.path().join("x.zip");
    fs::write(&not_zip, b"not a zip at all").unwrap();
    let r = diff(&not_zip, &data, &DiffOptions::default());
    assert!(matches!(r, Err(Error::InvalidArchive(_))), "{r:?}");
}

#[test]
fn cancel_and_progress() {
    let (_td, data, zip) = setup();
    let r = zipcore::diff_archive(
        &zip,
        &data,
        &DiffOptions::default(),
        &mut |_| {},
        &AtomicBool::new(true),
    );
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");

    let mut last = None;
    let r = zipcore::diff_archive(
        &zip,
        &data,
        &DiffOptions::default(),
        &mut |p| last = Some(p.clone()),
        &AtomicBool::new(false),
    )
    .unwrap();
    let last = last.expect("progress was reported");
    assert_eq!(last.files_done, last.files_total);
    assert_eq!(last.files_total, r.entries.len() as u64);
    assert_eq!(last.bytes_done, last.bytes_total);
    assert_eq!(last.bytes_total, 20_000 + 70_000 + 5);
}

#[test]
fn many_files_in_parallel() {
    let td = tempfile::tempdir().unwrap();
    let data = td.path().join("data");
    for i in 0..300 {
        write_file(
            &data.join(format!("d{}/f{i}.bin", i % 7)),
            &random_bytes(i * 37, i as u64),
        );
    }
    let zip = zip_of(
        td.path(),
        std::slice::from_ref(&data),
        &common::opts(Method::Deflate { level: 1 }),
    );
    fs::write(data.join("d3/f10.bin"), random_bytes(370, 999)).unwrap();
    for threads in [1, 8] {
        for full in [false, true] {
            let r = diff_ok(
                &zip,
                &data,
                &DiffOptions {
                    threads,
                    full,
                    ..Default::default()
                },
            );
            assert_eq!(r.entries.len(), 307);
            assert_eq!(
                changes(&r),
                [("d3/f10.bin".to_string(), DiffKind::Different)]
            );
        }
    }
}
