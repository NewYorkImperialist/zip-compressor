//! `list_archive`: our own archives, archives from the `zip` crate, and
//! input that is not an archive.

mod common;

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::*;
use filetime::FileTime;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};
use zipcore::{list_archive, EntryKind, Error, Listing, Method, Preset};

const CENTRAL_SIG: &[u8] = b"PK\x01\x02";

/// A small tree: `root/data/{a.txt, sub/b.txt, empty/}`.
fn make_tree(root: &Path) -> PathBuf {
    let data = root.join("data");
    fs::create_dir_all(data.join("sub")).unwrap();
    fs::create_dir_all(data.join("empty")).unwrap();
    write_file(
        &data.join("a.txt"),
        &"hello hello hello\n".repeat(200).into_bytes(),
    );
    write_file(&data.join("sub/b.txt"), b"world");
    data
}

fn write_archive(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, bytes).unwrap();
    p
}

fn list_ok(path: &Path) -> Listing {
    list_archive(path).unwrap_or_else(|e| panic!("list_archive({}) failed: {e:?}", path.display()))
}

fn at(secs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

fn set_mtime(p: &Path, secs: i64) {
    let t = FileTime::from_unix_time(secs, 0);
    filetime::set_symlink_file_times(p, t, t).unwrap();
}

/// Call `f` with each central directory record (fixed header through the end
/// of its extra field) so a test can damage or rewrite it in place.
fn patch_central(bytes: &mut [u8], mut f: impl FnMut(&mut [u8])) {
    let mut i = 0;
    let mut seen = 0;
    while let Some(pos) = bytes[i..].windows(4).position(|w| w == CENTRAL_SIG) {
        let start = i + pos;
        let u16_at = |b: &[u8], o: usize| usize::from(u16::from_le_bytes([b[o], b[o + 1]]));
        let name_len = u16_at(bytes, start + 28);
        let extra_len = u16_at(bytes, start + 30);
        let end = start + 46 + name_len + extra_len;
        f(&mut bytes[start..end]);
        seen += 1;
        i = end;
    }
    assert!(seen > 0, "no central directory records found");
}

/// Rename every 0x5455 extra field in the central directory to an unknown ID,
/// so readers fall back to the DOS date/time.
fn hide_ext_timestamps(bytes: &mut [u8]) {
    patch_central(bytes, |rec| {
        let name_len = usize::from(u16::from_le_bytes([rec[28], rec[29]]));
        let mut j = 46 + name_len;
        while j + 4 <= rec.len() {
            let id = u16::from_le_bytes([rec[j], rec[j + 1]]);
            let len = usize::from(u16::from_le_bytes([rec[j + 2], rec[j + 3]]));
            if id == 0x5455 {
                rec[j..j + 2].copy_from_slice(&0x9999u16.to_le_bytes());
            }
            j += 4 + len;
        }
    });
}

#[test]
fn every_preset_lists_like_the_zip_crate() {
    let td = tempfile::tempdir().unwrap();
    let data = make_tree(td.path());
    for preset in Preset::ALL {
        let (zip, summary) = compress_ok(std::slice::from_ref(&data), &opts(preset.method()));
        let path = write_archive(td.path(), &format!("{}.zip", preset.name()), &zip);
        let listing = list_ok(&path);
        let oracle = read_entries(&zip);

        assert_eq!(listing.archive_bytes, zip.len() as u64, "{preset:?}");
        assert!(
            listing.problems.is_empty(),
            "{preset:?}: {:?}",
            listing.problems
        );
        let got: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(got, names(&oracle), "{preset:?}: central directory order");
        for (e, o) in listing.entries.iter().zip(&oracle) {
            let kind = if o.is_dir {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            assert_eq!(e.kind, kind, "{}", e.name);
            assert_eq!(e.uncompressed, o.size, "{}", e.name);
            assert_eq!(e.compressed, o.compressed_size, "{}", e.name);
            assert_eq!(e.crc32, o.crc32, "{}", e.name);
            let method = match o.method {
                CompressionMethod::Stored => 0,
                CompressionMethod::Deflated => 8,
                m => panic!("unexpected method {m:?}"),
            };
            assert_eq!(e.method, method, "{}", e.name);
            assert!(!e.encrypted, "{}", e.name);
            assert!(e.modified.is_some(), "{}", e.name);
        }
        let files = listing
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::File)
            .count();
        let dirs = listing
            .entries
            .iter()
            .filter(|e| e.kind == EntryKind::Dir)
            .count();
        assert_eq!(files as u64, summary.files, "{preset:?}");
        assert_eq!(dirs as u64, summary.dirs, "{preset:?}");
        assert!(listing
            .entries
            .iter()
            .all(|e| e.kind != EntryKind::Dir || e.name.ends_with('/')));
    }
}

#[test]
fn store_method_lists_as_zero() {
    let td = tempfile::tempdir().unwrap();
    let data = make_tree(td.path());
    let (zip, _) = compress_ok(std::slice::from_ref(&data), &opts(Method::Store));
    let path = write_archive(td.path(), "store.zip", &zip);
    for e in list_ok(&path).entries {
        assert_eq!(e.method, 0, "{}", e.name);
        assert_eq!(e.compressed, e.uncompressed, "{}", e.name);
    }
}

#[cfg(unix)]
#[test]
fn symlinks_and_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("perm");
    fs::create_dir_all(root.join("private")).unwrap();
    write_file(&root.join("run.sh"), b"#!/bin/sh\n");
    write_file(&root.join("secret"), b"x");
    std::os::unix::fs::symlink("run.sh", root.join("link")).unwrap();
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(root.join("secret"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(root.join("private"), fs::Permissions::from_mode(0o700)).unwrap();

    let (zip, _) = compress_ok(std::slice::from_ref(&root), &opts(Method::default()));
    let path = write_archive(td.path(), "perm.zip", &zip);
    let listing = list_ok(&path);
    let get = |n: &str| {
        listing
            .entries
            .iter()
            .find(|e| e.name == n)
            .unwrap_or_else(|| panic!("{n} missing"))
    };
    assert_eq!(get("perm/run.sh").unix_mode, Some(0o755));
    assert_eq!(get("perm/run.sh").kind, EntryKind::File);
    assert_eq!(get("perm/secret").unix_mode, Some(0o600));
    assert_eq!(get("perm/private/").unix_mode, Some(0o700));
    assert_eq!(get("perm/private/").kind, EntryKind::Dir);
    let link = get("perm/link");
    assert_eq!(link.kind, EntryKind::Symlink);
    assert_eq!(link.uncompressed, "run.sh".len() as u64);
}

#[test]
fn mtimes_from_extended_timestamp() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("times");
    let odd = unix_secs(2021, 3, 4, 5, 6, 7);
    let pre1980 = unix_secs(1975, 6, 15, 12, 0, 0);
    let dir_t = unix_secs(2010, 7, 8, 9, 10, 11);
    write_file(&root.join("odd.txt"), b"odd");
    write_file(&root.join("old.txt"), b"old");
    fs::create_dir_all(root.join("sub")).unwrap();
    set_mtime(&root.join("odd.txt"), odd);
    set_mtime(&root.join("old.txt"), pre1980);
    set_mtime(&root.join("sub"), dir_t);

    let (zip, _) = compress_ok(std::slice::from_ref(&root), &opts(Method::default()));
    let path = write_archive(td.path(), "times.zip", &zip);
    let listing = list_ok(&path);
    for (name, t) in [
        ("times/odd.txt", odd),
        ("times/old.txt", pre1980),
        ("times/sub/", dir_t),
    ] {
        let e = listing.entries.iter().find(|e| e.name == name).unwrap();
        // Exact to the second, odd seconds and pre-1980 included: the 0x5455
        // field wins over the (clamped, 2-second) DOS time.
        assert_eq!(e.modified, Some(at(t)), "{name}");
    }
}

#[test]
fn mtimes_fall_back_to_dos_local_time() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("dos");
    // Even seconds, and far from any daylight-saving transition.
    let even = unix_secs(2019, 1, 20, 15, 30, 42);
    let odd = unix_secs(2022, 7, 10, 3, 4, 5);
    write_file(&root.join("even.txt"), b"e");
    write_file(&root.join("odd.txt"), b"o");
    set_mtime(&root.join("even.txt"), even);
    set_mtime(&root.join("odd.txt"), odd);

    let (mut zip, _) = compress_ok(std::slice::from_ref(&root), &opts(Method::default()));
    hide_ext_timestamps(&mut zip);
    let path = write_archive(td.path(), "dos.zip", &zip);
    let listing = list_ok(&path);
    // We wrote the DOS fields as local time; reading them back as local time
    // gives the original instant at 2-second resolution.
    for (name, t) in [("dos/even.txt", even), ("dos/odd.txt", odd - 1)] {
        let e = listing.entries.iter().find(|e| e.name == name).unwrap();
        assert_eq!(e.modified, Some(at(t)), "{name}");
    }
}

#[test]
fn invalid_dos_time_is_none() {
    let td = tempfile::tempdir().unwrap();
    let data = make_tree(td.path());
    let (mut zip, _) = compress_ok(std::slice::from_ref(&data), &opts(Method::default()));
    hide_ext_timestamps(&mut zip);
    // Month 0, day 0.
    patch_central(&mut zip, |rec| {
        rec[14..16].copy_from_slice(&0u16.to_le_bytes())
    });
    let path = write_archive(td.path(), "baddate.zip", &zip);
    for e in list_ok(&path).entries {
        assert_eq!(e.modified, None, "{}", e.name);
    }
}

#[test]
fn encrypted_flag_and_dos_host() {
    let td = tempfile::tempdir().unwrap();
    let data = make_tree(td.path());
    let (mut zip, _) = compress_ok(std::slice::from_ref(&data), &opts(Method::default()));
    patch_central(&mut zip, |rec| {
        rec[8] |= 1; // general-purpose flag bit 0: encrypted
        rec[5] = 0; // version made by: host MS-DOS
                    // DOS attributes only: the directory bit for directories.
        let is_dir = rec[46 + usize::from(u16::from_le_bytes([rec[28], rec[29]])) - 1] == b'/';
        rec[38..42].copy_from_slice(&(if is_dir { 0x10u32 } else { 0 }).to_le_bytes());
    });
    let path = write_archive(td.path(), "enc.zip", &zip);
    let listing = list_ok(&path);
    assert!(!listing.entries.is_empty());
    for e in &listing.entries {
        assert!(e.encrypted, "{}", e.name);
        assert_eq!(e.unix_mode, None, "{}: not a Unix host", e.name);
        assert_eq!(
            e.kind == EntryKind::Dir,
            e.name.ends_with('/'),
            "{}",
            e.name
        );
    }
}

/// An archive written by the `zip` crate, with a forced ZIP64 entry.
fn zip_crate_archive() -> Vec<u8> {
    let mut w = ZipWriter::new(Cursor::new(Vec::new()));
    let when = zip::DateTime::from_date_and_time(2020, 5, 6, 7, 8, 10).unwrap();
    let base = SimpleFileOptions::default().last_modified_time(when);
    w.add_directory("docs/", base.unix_permissions(0o750))
        .unwrap();
    w.start_file(
        "docs/readme.txt",
        base.compression_method(CompressionMethod::Deflated)
            .unix_permissions(0o640),
    )
    .unwrap();
    w.write_all(&"read me\n".repeat(100).into_bytes()).unwrap();
    w.start_file(
        "docs/raw.bin",
        base.compression_method(CompressionMethod::Stored),
    )
    .unwrap();
    w.write_all(&[1, 2, 3, 4]).unwrap();
    w.start_file(
        "big.txt",
        base.compression_method(CompressionMethod::Deflated)
            .large_file(true),
    )
    .unwrap();
    w.write_all(b"forced zip64 entry").unwrap();
    w.add_symlink("docs/link", "readme.txt", base).unwrap();
    w.finish().unwrap().into_inner()
}

#[test]
fn archive_from_the_zip_crate() {
    let td = tempfile::tempdir().unwrap();
    let mut bytes = zip_crate_archive();
    // big.txt's central record really uses ZIP64 (saturated 32-bit sizes).
    // Also note each record's host system: the zip crate writes some entries
    // (directories, at least) as DOS-made on Windows, dropping the Unix mode,
    // and its reader then invents one, which zipcore rightly doesn't report.
    let mut zip64_records = 0;
    let mut unix_made = Vec::new();
    patch_central(&mut bytes, |rec| {
        if u32::from_le_bytes(rec[20..24].try_into().unwrap()) == u32::MAX {
            zip64_records += 1;
        }
        unix_made.push(rec[5] == 3);
    });
    assert_eq!(zip64_records, 1, "expected one ZIP64 central record");
    if cfg!(unix) {
        assert!(unix_made.iter().all(|&u| u), "all Unix-made on Unix");
    }
    let path = write_archive(td.path(), "zipcrate.zip", &bytes);
    let listing = list_ok(&path);
    let oracle = read_entries(&bytes);
    assert!(listing.problems.is_empty(), "{:?}", listing.problems);
    let got: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        got,
        [
            "docs/",
            "docs/readme.txt",
            "docs/raw.bin",
            "big.txt",
            "docs/link"
        ]
    );
    for ((e, o), &unix) in listing.entries.iter().zip(&oracle).zip(&unix_made) {
        assert_eq!(e.uncompressed, o.size, "{}", e.name);
        assert_eq!(e.compressed, o.compressed_size, "{}", e.name);
        assert_eq!(e.crc32, o.crc32, "{}", e.name);
        let want = o.unix_mode.map(|m| m & 0o7777).filter(|_| unix);
        assert_eq!(e.unix_mode, want, "{}", e.name);
        // The zip crate writes only DOS time: local time within a zone offset
        // of the UTC reading, on a quarter-hour boundary.
        let m = e.modified.expect("modified");
        let d = zipcore_secs(m) - unix_secs(2020, 5, 6, 7, 8, 10);
        assert!(
            d.abs() <= 14 * 3600 && d.rem_euclid(900) == 0,
            "{}: {d}",
            e.name
        );
    }
    let get = |n: &str| listing.entries.iter().find(|e| e.name == n).unwrap();
    let mode_if_unix = |n: &str, mode: u32| {
        let i = listing.entries.iter().position(|e| e.name == n).unwrap();
        unix_made[i].then_some(mode)
    };
    assert_eq!(get("docs/").kind, EntryKind::Dir);
    assert_eq!(get("docs/").unix_mode, mode_if_unix("docs/", 0o750));
    assert_eq!(get("docs/readme.txt").method, 8);
    assert_eq!(
        get("docs/readme.txt").unix_mode,
        mode_if_unix("docs/readme.txt", 0o640)
    );
    assert_eq!(get("docs/raw.bin").method, 0);
    assert_eq!(
        get("big.txt").uncompressed,
        b"forced zip64 entry".len() as u64
    );
    assert_eq!(get("docs/link").kind, EntryKind::Symlink);
    assert_eq!(get("docs/link").uncompressed, "readme.txt".len() as u64);
}

fn zipcore_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

#[test]
fn not_an_archive_is_invalid() {
    let td = tempfile::tempdir().unwrap();
    let data = make_tree(td.path());
    let (zip, _) = compress_ok(std::slice::from_ref(&data), &opts(Method::default()));
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("tiny", b"PK".to_vec()),
        ("text", "not a zip file at all\n".repeat(20).into_bytes()),
        ("truncated", zip[..zip.len() / 2].to_vec()),
    ];
    for (name, bytes) in cases {
        let path = write_archive(td.path(), &format!("{name}.zip"), &bytes);
        match list_archive(&path) {
            Err(Error::InvalidArchive(_)) => {}
            other => panic!("{name}: expected InvalidArchive, got {other:?}"),
        }
    }
    match list_archive(td.path()) {
        Err(Error::InvalidArchive(_)) | Err(Error::Io { .. }) => {}
        other => panic!("directory: got {other:?}"),
    }
    match list_archive(&td.path().join("missing.zip")) {
        Err(Error::Io { path, .. }) => assert!(path.ends_with("missing.zip")),
        other => panic!("missing: got {other:?}"),
    }
}

#[test]
fn empty_archive_lists_nothing() {
    let td = tempfile::tempdir().unwrap();
    let bytes = ZipWriter::new(Cursor::new(Vec::new()))
        .finish()
        .unwrap()
        .into_inner();
    let path = write_archive(td.path(), "empty.zip", &bytes);
    let listing = list_ok(&path);
    assert!(listing.entries.is_empty());
    assert_eq!(listing.archive_bytes, 22);
}
