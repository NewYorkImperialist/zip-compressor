//! Windows-only metadata: DOS attributes, the MS-DOS host in "version made
//! by", local DOS times, and junction cycles. Runs on Windows CI.
#![cfg(windows)]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;
use filetime::FileTime;
use zipcore::{Method, SymlinkPolicy};

const DEFLATE: Method = Method::Deflate { level: 6 };

const READONLY: u32 = 0x01;
const HIDDEN: u32 = 0x02;
const SYSTEM: u32 = 0x04;
const DIRECTORY: u32 = 0x10;
const ARCHIVE: u32 = 0x20;
const STORED_BITS: u32 = READONLY | HIDDEN | SYSTEM | ARCHIVE;

/// The central-directory fields these tests look at.
#[derive(Debug, Clone, Copy)]
struct Cd {
    made_by: u16,
    external: u32,
    time: u16,
    date: u16,
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}

/// Central-directory records by name (small, non-ZIP64 archives only).
fn central_records(zip: &[u8]) -> BTreeMap<String, Cd> {
    let eocd = (0..=zip.len() - 22)
        .rev()
        .find(|&i| u32_at(zip, i) == 0x0605_4b50)
        .expect("no end of central directory record");
    let count = usize::from(u16_at(zip, eocd + 10));
    let mut i = u32_at(zip, eocd + 16) as usize;
    let mut out = BTreeMap::new();
    for _ in 0..count {
        assert_eq!(u32_at(zip, i), 0x0201_4b50, "central header signature");
        let name_len = usize::from(u16_at(zip, i + 28));
        let extra_len = usize::from(u16_at(zip, i + 30));
        let comment_len = usize::from(u16_at(zip, i + 32));
        let name = String::from_utf8(zip[i + 46..i + 46 + name_len].to_vec()).unwrap();
        out.insert(
            name,
            Cd {
                made_by: u16_at(zip, i + 4),
                external: u32_at(zip, i + 38),
                time: u16_at(zip, i + 12),
                date: u16_at(zip, i + 14),
            },
        );
        i += 46 + name_len + extra_len + comment_len;
    }
    out
}

fn attrs(p: &Path) -> u32 {
    fs::metadata(p).unwrap().file_attributes()
}

fn create_with_attrs(p: &Path, data: &[u8], attributes: u32) {
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .attributes(attributes)
        .open(p)
        .unwrap();
    f.write_all(data).unwrap();
}

fn set_readonly(p: &Path, ro: bool) {
    let mut perm = fs::metadata(p).unwrap().permissions();
    perm.set_readonly(ro);
    fs::set_permissions(p, perm).unwrap();
}

#[test]
fn dos_attributes_and_dos_host() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("attrs");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::create_dir_all(root.join("hidden_dir")).unwrap();
    fs::create_dir_all(root.join("ro_dir")).unwrap();
    write_file(&root.join("plain.txt"), b"plain");
    write_file(&root.join("ro.txt"), b"read-only");
    create_with_attrs(&root.join("hidden.txt"), b"hidden", HIDDEN);
    create_with_attrs(&root.join("hidsys.txt"), b"hidden+system", HIDDEN | SYSTEM);
    write_file(&root.join("sub/inner.txt"), b"inner");
    set_readonly(&root.join("ro.txt"), true);
    set_readonly(&root.join("ro_dir"), true);
    let st = Command::new("attrib")
        .arg("+h")
        .arg(root.join("hidden_dir"))
        .status()
        .unwrap();
    assert!(st.success(), "attrib +h failed");

    assert_ne!(
        attrs(&root.join("ro.txt")) & READONLY,
        0,
        "test setup: read-only"
    );
    assert_ne!(
        attrs(&root.join("hidden.txt")) & HIDDEN,
        0,
        "test setup: hidden"
    );
    assert_ne!(
        attrs(&root.join("hidden_dir")) & HIDDEN,
        0,
        "test setup: hidden dir"
    );

    for method in [Method::Store, DEFLATE] {
        let (zip, _) = compress_ok(std::slice::from_ref(&root), &opts(method));
        let cd = central_records(&zip);
        for (name, rec) in &cd {
            assert_eq!(rec.made_by >> 8, 0, "{name}: made by MS-DOS host");
            assert_eq!(rec.made_by & 0xff, 63, "{name}: spec version 6.3");
            assert_eq!(rec.external >> 16, 0, "{name}: no Unix mode in high half");
            let rel = name.trim_start_matches("attrs").trim_start_matches('/');
            let on_disk = attrs(&root.join(rel.trim_end_matches('/')));
            if name.ends_with('/') {
                let want = (on_disk & (HIDDEN | SYSTEM | ARCHIVE)) | DIRECTORY;
                assert_eq!(rec.external, want, "{name}: directory attributes");
            } else {
                assert_eq!(
                    rec.external,
                    on_disk & STORED_BITS,
                    "{name}: file attributes"
                );
            }
        }
        let ext = |n: &str| cd[n].external;
        assert_eq!(ext("attrs/ro.txt") & READONLY, READONLY);
        assert_eq!(ext("attrs/plain.txt") & (READONLY | HIDDEN), 0);
        assert_eq!(ext("attrs/hidden.txt") & (READONLY | HIDDEN), HIDDEN);
        assert_eq!(ext("attrs/hidsys.txt") & (HIDDEN | SYSTEM), HIDDEN | SYSTEM);
        assert_eq!(
            ext("attrs/hidden_dir/") & (HIDDEN | DIRECTORY),
            HIDDEN | DIRECTORY
        );
        assert_eq!(
            ext("attrs/ro_dir/") & (READONLY | DIRECTORY),
            DIRECTORY,
            "read-only is not stored for directories"
        );

        // How a Unix-style reader (the zip crate) interprets the entries:
        // directories stay directories, nothing becomes unreadable, and
        // read-only files lose their write bits.
        let entries = read_entries(&zip);
        for e in &entries {
            if let Some(m) = e.unix_mode {
                assert_eq!(m & 0o444, 0o444, "{}: readable (mode {m:o})", e.name);
            }
        }
        assert!(find(&entries, "attrs/sub/").is_dir);
        assert!(find(&entries, "attrs/ro_dir/").is_dir);
        assert_eq!(find(&entries, "attrs/sub/inner.txt").data, b"inner");
        if let Some(m) = find(&entries, "attrs/ro.txt").unix_mode {
            assert_eq!(
                m & 0o222,
                0,
                "read-only file has no write bits (mode {m:o})"
            );
        }
    }

    // Let the temp dir be removed.
    set_readonly(&root.join("ro.txt"), false);
    set_readonly(&root.join("ro_dir"), false);
}

/// Expected local civil time of a UTC Unix timestamp, computed with the
/// (non-dynamic) `SystemTimeToTzSpecificLocalTime` and the current zone,
/// rather than the dynamic-zone function the library uses.
fn expected_local(secs: i64) -> (i64, u8, u8, u8, u8, u8) {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let ticks = (secs + 11_644_473_600) as u64 * 10_000_000;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: plain-old-data out-parameters; a null zone means the current one.
    unsafe {
        assert_ne!(FileTimeToSystemTime(&ft, &mut utc), 0);
        assert_ne!(
            SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local),
            0
        );
    }
    (
        i64::from(local.wYear),
        local.wMonth as u8,
        local.wDay as u8,
        local.wHour as u8,
        local.wMinute as u8,
        local.wSecond as u8,
    )
}

fn unpack_dos(time: u16, date: u16) -> (i64, u8, u8, u8, u8, u8) {
    (
        1980 + i64::from(date >> 9),
        ((date >> 5) & 0xf) as u8,
        (date & 0x1f) as u8,
        (time >> 11) as u8,
        ((time >> 5) & 0x3f) as u8,
        ((time & 0x1f) * 2) as u8,
    )
}

#[test]
fn dos_time_is_local_time_of_mtime() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("times");
    // Winter and summer dates (different DST state in most zones that have
    // DST), an odd second, a date near a US/EU DST switch, and pre-1980.
    let cases = [
        ("winter.txt", unix_secs(2021, 1, 15, 12, 0, 1)),
        ("summer.txt", unix_secs(2021, 7, 15, 12, 0, 0)),
        ("march.txt", unix_secs(2023, 3, 26, 1, 30, 0)),
        ("november.txt", unix_secs(2022, 11, 6, 9, 15, 30)),
        ("old.txt", unix_secs(1975, 6, 15, 12, 0, 0)),
    ];
    for (name, t) in cases {
        let p = root.join(name);
        write_file(&p, name.as_bytes());
        filetime::set_file_mtime(&p, FileTime::from_unix_time(t, 0)).unwrap();
    }
    let (zip, _) = compress_ok(std::slice::from_ref(&root), &opts(DEFLATE));
    let cd = central_records(&zip);
    let entries = read_entries(&zip);
    for (name, t) in cases {
        let full = format!("times/{name}");
        let rec = cd[&full];
        let got = unpack_dos(rec.time, rec.date);
        if t < unix_secs(1980, 1, 2, 0, 0, 0) {
            assert_eq!((got.0, got.1, got.2), (1980, 1, 1), "{full}: clamped");
        } else {
            let mut want = expected_local(t);
            want.5 &= !1;
            assert_eq!(got, want, "{full}: DOS time is the local time of the mtime");
        }
        assert_eq!(
            find(&entries, &full).ext_mtime.map(i64::from),
            Some(t),
            "{full}: 0x5455 keeps exact UTC"
        );
    }
}

/// A junction back to an ancestor must not make the walk loop: it is
/// reported as a cycle and skipped.
#[test]
fn junction_cycle_terminates() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("cyc");
    write_file(&root.join("a/f.txt"), b"f");
    let link = root.join("a").join("back");
    let st = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(&link)
        .arg(&root)
        .status()
        .unwrap();
    assert!(st.success(), "mklink /J failed");
    assert!(fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());

    for policy in [
        SymlinkPolicy::Follow,
        SymlinkPolicy::Store,
        SymlinkPolicy::Skip,
    ] {
        let inputs: Vec<PathBuf> = vec![root.clone()];
        let o = opts_full(DEFLATE, policy, false);
        let (zip, _) = with_timeout(60, "junction cycle", move || compress_ok(&inputs, &o));
        let got = names(&read_entries(&zip));
        assert_eq!(
            got,
            vec!["cyc/", "cyc/a/", "cyc/a/f.txt"],
            "{policy:?}: the cycle-forming junction is skipped"
        );
    }
    fs::remove_dir(&link).unwrap();
}
