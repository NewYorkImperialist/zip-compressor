//! Trees of many small files: they are compressed in batches with read-ahead
//! and small deflate windows, none of which may change a byte of the archive.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;
use zip::CompressionMethod;
use zipcore::Method;

const MIB: usize = 1 << 20;

/// `n` small files of every kind (empty, 1 byte, text, source, noise; sizes
/// across every deflate window size up to 40 KiB), in directories of 37
/// entries, with empty directories and (on Unix) a symlink in between, and
/// a big file in the middle so batches have to stop at it.
fn small_tree(parent: &Path, n: usize) -> PathBuf {
    let root = parent.join("small");
    for i in 0..n {
        let size = match i % 9 {
            0 => 0,
            1 => 1,
            2 => 250 + i % 13,
            3 => 4096 - 262 + i % 3,
            4 => 40_000 - i % 1000,
            _ => (i * 7919) % 4096,
        };
        let data = match i % 4 {
            0 => text_corpus(size, i as u64),
            1 => source_like(size, i as u64),
            2 => random_bytes(size, i as u64),
            _ => compressible_bytes(size),
        };
        let dir = root.join(format!("d{:02}", i / 37));
        write_file(&dir.join(format!("f{i:04}.txt")), &data);
        if i % 101 == 0 {
            fs::create_dir_all(dir.join(format!("empty{i}"))).unwrap();
        }
    }
    let mut big = text_corpus(4 * MIB + 77, 5);
    big.extend(random_bytes(MIB, 6));
    write_file(&root.join("d03/m_big.bin"), &big);
    #[cfg(unix)]
    std::os::unix::fs::symlink("f0001.txt", root.join("d01/link")).unwrap();
    root
}

/// One-shot raw deflate of `data` by an independent encoder (flate2, zlib-rs
/// backend, default window / memLevel / strategy), as the engine runs it:
/// all input, then `Finish`, into a big enough buffer.
fn flate2_one_shot(data: &[u8], level: u32) -> Vec<u8> {
    let mut c = flate2::Compress::new(flate2::Compression::new(level), false);
    let mut out = Vec::with_capacity(data.len() + data.len() / 8 + 128);
    let st = c
        .compress_vec(data, &mut out, flate2::FlushCompress::Finish)
        .unwrap();
    assert_eq!(st, flate2::Status::StreamEnd);
    out
}

/// Every thread count gives the same bytes; the archive is sound and
/// follows the size rule; a small file's deflate is exactly what an
/// independent encoder with the full 32 KiB window produces at that level.
#[test]
fn small_files_identical_across_thread_counts() {
    let td = tempfile::tempdir().unwrap();
    let root = small_tree(td.path(), 700);
    let input = std::slice::from_ref(&root);
    let expected = expected_entries(&root);
    for method in [
        Method::Store,
        Method::Deflate { level: 1 },
        Method::Deflate { level: 6 },
        Method::Deflate { level: 9 },
        Method::DeflateBest,
    ] {
        let (reference, _) = compress_ok(input, &opts_t(method, 1));
        let entries = verify_archive(&reference, &expected, &format!("{method:?} threads=1"));
        let bad = size_rule_problems(&entries);
        assert!(bad.is_empty(), "{method:?}: {bad:?}");
        if let Method::Deflate { level } = method {
            for e in &entries {
                if e.method == CompressionMethod::Deflated && e.size < 4 * MIB as u64 {
                    let theirs = flate2_one_shot(&e.data, u32::from(level));
                    assert_eq!(
                        e.compressed_size,
                        theirs.len() as u64,
                        "{method:?} {}: {} bytes",
                        e.name,
                        e.size
                    );
                }
            }
        }
        for t in [2, 3, 8, 16] {
            let (b, _) = compress_ok(input, &opts_t(method, t));
            assert!(
                b == reference,
                "{method:?}: threads={t} differs from threads=1"
            );
        }
    }
}

#[test]
fn zopfli_small_files_identical_across_thread_counts() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("z");
    for i in 0..60usize {
        let data = match i % 3 {
            0 => text_corpus(100 + i * 97, i as u64),
            1 => source_like(3000 + i, i as u64),
            _ => random_bytes(i * 31, i as u64),
        };
        write_file(&root.join(format!("d{}/f{i:02}", i % 4)), &data);
    }
    let input = std::slice::from_ref(&root);
    let method = Method::Zopfli { iterations: 2 };
    let (a, _) = compress_ok(input, &opts_t(method, 1));
    verify_archive(&a, &expected_entries(&root), "zopfli threads=1");
    let (b, _) = compress_ok(input, &opts_t(method, 5));
    assert!(a == b, "zopfli: threads=5 differs from threads=1");
}

/// A failing file inside a batch: the error reported is the first failing
/// file in scan order, whatever the thread count.
#[cfg(unix)]
#[test]
fn first_unreadable_file_in_scan_order_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("r");
    for i in 0..600usize {
        write_file(&root.join(format!("f{i:04}")), &compressible_bytes(i * 3));
    }
    let bad = [root.join("f0150"), root.join("f0400")];
    for p in &bad {
        fs::set_permissions(p, fs::Permissions::from_mode(0o000)).unwrap();
    }
    if fs::read(&bad[0]).is_ok() {
        eprintln!(
            "NOTE: skipping first_unreadable_file_in_scan_order_is_reported: running as root"
        );
        return;
    }
    for t in [1, 3, 8] {
        let res = compress_mem(
            std::slice::from_ref(&root),
            &opts_t(Method::Deflate { level: 6 }, t),
        );
        match res {
            Err(zipcore::Error::Io { path, .. }) => assert_eq!(path, bad[0], "threads={t}"),
            other => panic!(
                "threads={t}: expected an I/O error, got {:?}",
                other.map(|_| ())
            ),
        }
    }
    for p in &bad {
        fs::set_permissions(p, fs::Permissions::from_mode(0o644)).unwrap();
    }
}

#[cfg(unix)]
const LOW_LIMIT_ENV: &str = "ZIPCORE_SMALL_FILES_LOW_LIMIT_TREE";

/// With a tiny open-files limit (read-ahead has almost no descriptors, and
/// opens may fail for lack of them) archives still come out, and identical.
/// Runs itself in a child process under `ulimit -n`, so the limit does not
/// affect the other tests.
#[cfg(unix)]
#[test]
fn low_open_files_limit() {
    if let Some(tree) = std::env::var_os(LOW_LIMIT_ENV) {
        let root = PathBuf::from(tree);
        let input = std::slice::from_ref(&root);
        let reference = fs::read(root.with_extension("ref")).unwrap();
        for t in [1, 2, 4] {
            let (b, _) = compress_ok(input, &opts_t(Method::Deflate { level: 6 }, t));
            assert!(
                b == reference,
                "threads={t} differs under a low open-files limit"
            );
        }
        return;
    }
    let td = tempfile::tempdir().unwrap();
    let root = small_tree(td.path(), 300);
    let (reference, _) = compress_ok(
        std::slice::from_ref(&root),
        &opts_t(Method::Deflate { level: 6 }, 1),
    );
    fs::write(root.with_extension("ref"), &reference).unwrap();
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"ulimit -n 16 && exec "$0" "$@""#)
        .arg(&exe)
        .args([
            "--exact",
            "low_open_files_limit",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(LOW_LIMIT_ENV, &root)
        .output()
        .unwrap();
    let log =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "child failed:\n{log}");
    assert!(
        log.contains("1 passed"),
        "child did not run the test:\n{log}"
    );
}
