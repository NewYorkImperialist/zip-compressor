//! Permissions, modification times and symlink policies.

mod common;

use std::fs;
use std::path::Path;

use common::*;
use filetime::FileTime;
use zipcore::Method;
#[cfg(unix)]
use zipcore::SymlinkPolicy;

const DEFLATE: Method = Method::Deflate { level: 6 };

/// The DOS time must be the UTC or the local civil time of `utc` (we cannot
/// know the test machine's zone here), at 2-second resolution.
fn assert_dos_time_matches(name: &str, dos: &zip::DateTime, utc: i64) {
    let d = dos_naive_secs(dos) - utc;
    let r = d.rem_euclid(900);
    assert!(
        d.abs() <= 14 * 3600 + 2 && (r <= 2 || r >= 898),
        "{name}: DOS time {dos:?} is not within 2s of UTC/local time of unix {utc} (off by {d}s)"
    );
}

fn set_mtime(p: &Path, secs: i64) {
    let t = FileTime::from_unix_time(secs, 0);
    filetime::set_symlink_file_times(p, t, t).unwrap();
}

#[test]
fn mtimes_preserved_in_extended_timestamp_and_dos_time() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("times");
    let odd = unix_secs(2021, 3, 4, 5, 6, 7);
    let even = unix_secs(2019, 12, 31, 23, 59, 58);
    let pre1980 = unix_secs(1975, 6, 15, 12, 0, 0);
    let dir_t = unix_secs(2010, 7, 8, 9, 10, 11);
    write_file(&root.join("odd.txt"), b"odd");
    write_file(&root.join("even.txt"), b"even");
    write_file(&root.join("old.txt"), b"old");
    fs::create_dir_all(root.join("sub")).unwrap();
    set_mtime(&root.join("odd.txt"), odd);
    set_mtime(&root.join("even.txt"), even);
    set_mtime(&root.join("old.txt"), pre1980);
    set_mtime(&root.join("sub"), dir_t);

    for method in [Method::Store, DEFLATE] {
        let (zip, _) = compress_ok(std::slice::from_ref(&root), &opts(method));
        let entries = read_entries(&zip);
        for (name, t) in [
            ("times/odd.txt", odd),
            ("times/even.txt", even),
            ("times/old.txt", pre1980),
            ("times/sub/", dir_t),
        ] {
            let e = find(&entries, name);
            assert_eq!(
                e.ext_mtime.map(i64::from),
                Some(t),
                "{name}: extended timestamp (0x5455) mtime"
            );
            let dos = e.dos_time.unwrap_or_else(|| panic!("{name}: no DOS time"));
            if t < unix_secs(1980, 1, 2, 0, 0, 0) {
                assert_eq!(
                    (dos.year(), dos.month(), dos.day()),
                    (1980, 1, 1),
                    "{name}: pre-1980 DOS time must clamp to 1980-01-01, got {dos:?}"
                );
            } else {
                assert_dos_time_matches(name, &dos, t);
            }
        }
    }
}

#[test]
fn pre_1970_mtime_does_not_break_archive() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("ancient");
    write_file(&root.join("f.txt"), b"1965");
    let t = unix_secs(1965, 1, 1, 0, 0, 0);
    set_mtime(&root.join("f.txt"), t);
    if FileTime::from_last_modification_time(&fs::metadata(root.join("f.txt")).unwrap())
        .unix_seconds()
        != t
    {
        eprintln!("NOTE: filesystem cannot store pre-1970 mtimes; skipping");
        return;
    }
    let (zip, _) = compress_ok(&[root], &opts(DEFLATE));
    let entries = read_entries(&zip);
    let e = find(&entries, "ancient/f.txt");
    assert_eq!(e.data, b"1965");
    // 0x5455 holds a signed 32-bit value; if written it must decode to the real time.
    if let Some(m) = e.ext_mtime {
        assert_eq!(m as i32 as i64, t, "pre-1970 extended timestamp");
    }
    let dos = e.dos_time.expect("DOS time");
    assert_eq!((dos.year(), dos.month(), dos.day()), (1980, 1, 1));
}

#[cfg(unix)]
#[test]
fn unix_permissions_preserved() {
    use std::os::unix::fs::PermissionsExt;
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("perms");
    let cases = [
        ("run.sh", 0o755u32),
        ("secret", 0o600),
        ("plain.txt", 0o644),
        ("ro", 0o444),
    ];
    for (n, m) in cases {
        let p = root.join(n);
        write_file(&p, n.as_bytes());
        fs::set_permissions(&p, fs::Permissions::from_mode(m)).unwrap();
    }
    let private = root.join("private_dir");
    write_file(&private.join("inner.txt"), b"x");
    fs::set_permissions(&private, fs::Permissions::from_mode(0o750)).unwrap();

    for method in [Method::Store, DEFLATE] {
        let (zip, _) = compress_ok(std::slice::from_ref(&root), &opts(method));
        let entries = read_entries(&zip);
        for (n, m) in cases {
            let name = format!("perms/{n}");
            let mode = find(&entries, &name)
                .unix_mode
                .unwrap_or_else(|| panic!("{name}: no unix mode"));
            assert_eq!(mode & 0o7777, m, "{name}: permissions {mode:o}");
            assert_eq!(
                mode & 0o170000,
                0o100000,
                "{name}: not marked as a regular file ({mode:o})"
            );
        }
        let d = find(&entries, "perms/private_dir/");
        let mode = d.unix_mode.expect("dir unix mode");
        assert_eq!(mode & 0o7777, 0o750, "dir permissions {mode:o}");
        assert_eq!(
            mode & 0o170000,
            0o040000,
            "dir not marked as directory ({mode:o})"
        );
    }
}

#[cfg(unix)]
fn symlink_tree(parent: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::symlink;
    let root = parent.join("links");
    write_file(&root.join("target.txt"), b"target contents");
    write_file(&root.join("sub/inner.txt"), b"inner contents");
    symlink("target.txt", root.join("link")).unwrap();
    symlink("sub", root.join("dirlink")).unwrap();
    root
}

#[cfg(unix)]
#[test]
fn symlink_store_records_link() {
    use std::os::unix::fs::symlink;
    let td = tempfile::tempdir().unwrap();
    let root = symlink_tree(td.path());
    symlink("no/such/target", root.join("dangling")).unwrap();
    let (zip, summary) = compress_ok(
        std::slice::from_ref(&root),
        &opts_full(DEFLATE, SymlinkPolicy::Store, true),
    );
    let entries = read_entries(&zip);
    assert_safe_names(&entries);
    for (name, target) in [
        ("links/link", "target.txt"),
        ("links/dirlink", "sub"),
        ("links/dangling", "no/such/target"),
    ] {
        let e = find(&entries, name);
        let mode = e
            .unix_mode
            .unwrap_or_else(|| panic!("{name}: no unix mode"));
        assert_eq!(
            mode & 0o170000,
            0o120000,
            "{name}: not a symlink entry ({mode:o})"
        );
        assert!(e.is_symlink && !e.is_dir);
        assert_eq!(
            String::from_utf8_lossy(&e.data),
            target,
            "{name}: link target"
        );
    }
    assert!(
        !names(&entries)
            .iter()
            .any(|n| n.starts_with("links/dirlink/")),
        "Store policy must not descend into a directory symlink: {:?}",
        names(&entries)
    );
    assert_eq!(summary.symlinks, 3);
    assert_nodes_eq(&archive_nodes(&entries), &expected_entries(&root));
}

#[cfg(unix)]
#[test]
fn symlink_follow_archives_targets() {
    let td = tempfile::tempdir().unwrap();
    let root = symlink_tree(td.path());
    let (zip, _) = compress_ok(&[root], &opts_full(DEFLATE, SymlinkPolicy::Follow, true));
    let entries = read_entries(&zip);
    assert_safe_names(&entries);
    assert!(
        entries.iter().all(|e| !e.is_symlink),
        "Follow left a symlink entry"
    );
    assert_eq!(find(&entries, "links/link").data, b"target contents");
    assert!(find(&entries, "links/dirlink/").is_dir);
    assert_eq!(
        find(&entries, "links/dirlink/inner.txt").data,
        b"inner contents"
    );
    assert_eq!(
        find(&entries, "links/sub/inner.txt").data,
        b"inner contents"
    );
}

/// Run `f` on a thread and fail if it does not finish within `secs`.
#[cfg(unix)]
fn with_timeout<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
        Ok(v) => v,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("operation did not finish within {secs}s (symlink loop?)")
        }
        Err(e) => panic!("worker thread died (panicked?): {e}"),
    }
}

#[cfg(unix)]
#[test]
fn symlink_follow_cycle_terminates() {
    use std::os::unix::fs::symlink;
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("cyc");
    write_file(&root.join("sub/file.txt"), b"f");
    symlink("..", root.join("sub/up")).unwrap();
    symlink(".", root.join("self")).unwrap();
    let r = root.clone();
    let res = with_timeout(10, move || {
        compress_mem(&[r], &opts_full(DEFLATE, SymlinkPolicy::Follow, true))
            .map(|(zip, _)| read_entries(&zip))
    });
    match res {
        Ok(entries) => {
            assert_safe_names(&entries);
            assert!(
                entries.len() < 100,
                "cycle expanded into {} entries",
                entries.len()
            );
            assert_eq!(find(&entries, "cyc/sub/file.txt").data, b"f");
        }
        Err(e) => eprintln!("NOTE: cycle under Follow reported an error (acceptable): {e}"),
    }
}

#[cfg(unix)]
#[test]
fn symlink_follow_dangling_does_not_panic() {
    use std::os::unix::fs::symlink;
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("dang");
    write_file(&root.join("ok.txt"), b"ok");
    symlink("missing", root.join("broken")).unwrap();
    let r = root.clone();
    let res = with_timeout(10, move || {
        compress_mem(&[r], &opts_full(DEFLATE, SymlinkPolicy::Follow, true))
            .map(|(zip, _)| read_entries(&zip))
    });
    if let Ok(entries) = res {
        assert!(
            entries.iter().all(|e| !e.is_symlink),
            "Follow stored a symlink"
        );
    }
}

#[cfg(unix)]
#[test]
fn symlink_skip_omits_links() {
    let td = tempfile::tempdir().unwrap();
    let root = symlink_tree(td.path());
    let (zip, summary) = compress_ok(&[root], &opts_full(DEFLATE, SymlinkPolicy::Skip, true));
    let entries = read_entries(&zip);
    let mut n = names(&entries);
    n.sort();
    assert_eq!(
        n,
        vec![
            "links/",
            "links/sub/",
            "links/sub/inner.txt",
            "links/target.txt"
        ]
    );
    assert_eq!(summary.symlinks, 0);
}
