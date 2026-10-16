//! `CompressOptions::store_incompressible`: data that deflate can't usefully
//! shrink (estimated from samples, see `src/detect.rs`) is Stored without a
//! deflate attempt; everything else goes through the method as before.
//!
//! - Random data of at least 512 KiB is Stored; text never is.
//! - A file's compressible header or trailer (a ZIP central directory, a
//!   video index) is enough to deflate it; a tiny header (EXIF-sized) is not.
//! - Big files are Stored whole only if they look incompressible throughout;
//!   otherwise they are deflated and their incompressible chunks become
//!   stored blocks.
//! - With the flag off, every file goes through the method.
//! - Every method stores exactly the same files; output is identical for
//!   any thread count; archives pass `test_archive` and extract intact.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use common::*;
use zip::CompressionMethod::{Deflated, Stored};
use zipcore::{CompressOptions, ExtractOptions, Method};

const KIB: usize = 1 << 10;

fn opts_d(method: Method, detect: bool, threads: usize) -> CompressOptions {
    CompressOptions {
        method,
        store_incompressible: detect,
        threads,
        ..CompressOptions::default()
    }
}

const BALANCED: Method = Method::Deflate { level: 6 };

fn one(path: &Path, o: &CompressOptions) -> Entry {
    let (zip, _) = compress_ok(&[path.to_path_buf()], o);
    let entries = read_entries(&zip);
    assert_eq!(entries.len(), 1);
    assert_layout(&zip, "single file");
    entries.into_iter().next().unwrap()
}

/// `len` random bytes with `run` zero bytes at `at`.
fn random_with_zeros(len: usize, at: usize, run: usize, seed: u64) -> Vec<u8> {
    let mut v = random_bytes(len, seed);
    v[at..at + run].fill(0);
    v
}

/// The default is on.
#[test]
fn default_is_on() {
    assert!(CompressOptions::default().store_incompressible);
}

#[test]
fn random_data_is_stored_and_text_is_deflated() {
    let td = tempfile::tempdir().unwrap();
    for (name, len) in [
        ("r_512k", 512 * KIB),
        ("r_1m", MIB),
        ("r_3m", 3 * MIB + 5),
        ("r_big", 6 * MIB + 77),
    ] {
        let f = td.path().join(name);
        write_file(&f, &random_bytes(len, len as u64));
        for m in [
            Method::Deflate { level: 1 },
            BALANCED,
            Method::Deflate { level: 9 },
            Method::DeflateBest,
        ] {
            let e = one(&f, &opts_d(m, true, 4));
            assert_eq!(e.method, Stored, "{name} {m:?}");
            assert_eq!(e.compressed_size, e.size, "{name} {m:?}");
        }
    }
    for (name, len) in [("t_600k", 600 * KIB), ("t_2m", 2 * MIB), ("t_5m", 5 * MIB)] {
        let f = td.path().join(name);
        let data = text_corpus(len, 3);
        write_file(&f, &data);
        let e = one(&f, &opts_d(BALANCED, true, 4));
        assert_eq!(e.method, Deflated, "{name}");
        assert!(e.compressed_size < e.size / 2, "{name}");
        assert!(e.data == data);
    }
}

/// Small files are never checked: a 500 KiB random file still gets its
/// deflate attempt (and is Stored because that attempt isn't smaller),
/// and one that deflate shrinks by a few bytes is Deflated.
#[test]
fn files_under_512_kib_are_not_checked() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("almost.bin");
    let data = random_with_zeros(500 * KIB, 100_000, 2_000, 7);
    assert!(
        indep_deflate_len(&data, 6) < data.len(),
        "test input broken"
    );
    write_file(&f, &data);
    let e = one(&f, &opts_d(BALANCED, true, 2));
    assert_eq!(e.method, Deflated);
    assert!(e.compressed_size < e.size);
}

/// Data that deflate shrinks by less than 0.25%, with the gain away from
/// the samples: Stored when detecting, Deflated (smaller) when not. This is
/// the size the option trades for speed.
#[test]
fn marginal_gain_is_stored_only_when_detecting() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("photo.heic");
    // 2 MiB: samples at 0, the end and the middle of 8 strata of ~248 KiB;
    // the zero run at 40 KB is in none of them.
    let data = random_with_zeros(2 * MIB, 40_000, 3_000, 11);
    let d6 = indep_deflate_len(&data, 6);
    assert!(
        d6 < data.len() && d6 * 10_000 > data.len() * 9_975,
        "test input broken: level 6 gives {d6} of {}",
        data.len()
    );
    write_file(&f, &data);
    let on = one(&f, &opts_d(BALANCED, true, 2));
    assert_eq!(on.method, Stored);
    let off = one(&f, &opts_d(BALANCED, false, 2));
    assert_eq!(off.method, Deflated);
    assert!(off.compressed_size < off.size);
    assert!(on.data == data && off.data == data);
    // What detection gave up: under 0.25% of the file.
    assert!((on.compressed_size - off.compressed_size) * 400 < on.size);
}

/// A compressible header or trailer of a few percent is worth deflating
/// (a JAR's central directory, a video index); a header of a couple of KiB
/// (EXIF in a JPEG) on megabytes of entropy is not.
#[test]
fn compressible_header_or_trailer() {
    let td = tempfile::tempdir().unwrap();
    let cases: [(&str, Vec<u8>, bool); 4] = [
        (
            "header.bin",
            {
                let mut d = text_corpus(128 * KIB, 1);
                d.extend(random_bytes(2 * MIB, 2));
                d
            },
            true,
        ),
        (
            "trailer.bin",
            {
                let mut d = random_bytes(3 * MIB, 3);
                d.extend(source_like(100 * KIB, 4));
                d
            },
            true,
        ),
        (
            "exif.jpg",
            {
                let mut d = text_corpus(2 * KIB, 5);
                d.extend(random_bytes(3 * MIB, 6));
                d
            },
            false,
        ),
        (
            "big_trailer.mov",
            {
                let mut d = random_bytes(12 * MIB, 7);
                d.extend(source_like(300 * KIB, 8));
                d
            },
            true,
        ),
    ];
    for (name, data, deflate) in cases {
        let f = td.path().join(name);
        write_file(&f, &data);
        let e = one(&f, &opts_d(BALANCED, true, 4));
        assert!(e.data == data, "{name}");
        let want = if deflate { Deflated } else { Stored };
        assert_eq!(e.method, want, "{name}");
        assert!(
            size_rule_problems(std::slice::from_ref(&e)).is_empty(),
            "{name}"
        );
    }
}

/// The 2026-10-16 JAR regression: a big file whose first MiB is
/// incompressible (a signed JAR's huge deflated manifest) but whose rest
/// compresses was Stored whole by the old first-chunk probe. Now it is
/// deflated, and its incompressible chunks cost only stored-block framing.
#[test]
fn big_file_with_incompressible_first_mib_is_deflated() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("client.jar");
    let mut data = random_bytes(3 * MIB, 21);
    let text = text_corpus(4 * MIB, 22);
    data.extend(&text);
    data.extend(random_bytes(MIB + 999, 23));
    write_file(&f, &data);
    // The random 4 MiB stored, the text deflated (give or take a chunk edge).
    let expect = (4 * MIB + 999 + indep_deflate_len(&text, 6)) as u64;
    for threads in [1, 8] {
        let e = one(&f, &opts_d(BALANCED, true, threads));
        assert_eq!(e.method, Deflated, "threads {threads}");
        assert!(e.data == data);
        assert!(
            e.compressed_size < expect + expect / 50,
            "threads {threads}: {} -> {}",
            e.size,
            e.compressed_size
        );
        assert!(e.compressed_size <= large_bound(e.size));
    }
}

/// A big file compressible only in the middle, between the probe's
/// samples' strata: still deflated.
#[test]
fn big_file_compressible_in_the_middle_is_deflated() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("disk.img");
    let mut data = random_bytes(20 * MIB, 31);
    data[9 * MIB..11 * MIB].copy_from_slice(&text_corpus(2 * MIB, 32));
    write_file(&f, &data);
    let e = one(&f, &opts_d(BALANCED, true, 4));
    assert_eq!(e.method, Deflated);
    assert!(e.compressed_size < (19 * MIB) as u64);
    assert!(e.data == data);
}

/// With the flag off, every file goes through the method: incompressible
/// small files get a deflate attempt (Stored only because it isn't
/// smaller), and big ones are Deflated with stored-block fallback.
#[test]
fn flag_off_deflates_everything() {
    let td = tempfile::tempdir().unwrap();
    let small = td.path().join("small.bin");
    write_file(&small, &random_bytes(2 * MIB, 41));
    let big = td.path().join("big.bin");
    let big_data = random_bytes(5 * MIB + 3, 42);
    write_file(&big, &big_data);
    for m in [Method::Deflate { level: 1 }, BALANCED, Method::DeflateBest] {
        let e = one(&small, &opts_d(m, false, 4));
        assert_eq!(e.method, Stored, "{m:?}: deflate is larger, so Stored");
        let e = one(&big, &opts_d(m, false, 4));
        assert_eq!(e.method, Deflated, "{m:?}");
        assert!(e.compressed_size <= large_bound(e.size), "{m:?}");
        assert!(e.data == big_data);
        let e = one(&big, &opts_d(m, true, 4));
        assert_eq!(e.method, Stored, "{m:?}");
    }
    // Store stays Store either way.
    for detect in [false, true] {
        let e = one(&big, &opts_d(Method::Store, detect, 4));
        assert_eq!(e.method, Stored);
    }
}

/// A tree mixing every case above.
fn mixed(root: &Path) {
    write_file(&root.join("a/notes.txt"), &text_corpus(40 * KIB, 51));
    write_file(&root.join("a/photo.heic"), &random_bytes(700 * KIB, 52));
    write_file(&root.join("a/clip.mp4"), &random_bytes(3 * MIB, 53));
    let mut lib = random_bytes(MIB, 54);
    lib.extend(source_like(600 * KIB, 55));
    write_file(&root.join("b/lib.jar"), &lib);
    write_file(&root.join("b/video.mov"), &random_bytes(9 * MIB + 1, 56));
    let mut big = random_bytes(2 * MIB, 57);
    big.extend(text_corpus(3 * MIB, 58));
    write_file(&root.join("b/mixed.bin"), &big);
    write_file(&root.join("c/src.rs"), &source_like(900 * KIB, 59));
    write_file(&root.join("c/tiny.bin"), &random_bytes(100, 60));
}

/// Same archive whatever the thread count, with the flag on and off.
#[test]
fn deterministic_across_thread_counts() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("tree");
    mixed(&root);
    for (detect, m) in [
        (true, Method::Deflate { level: 1 }),
        (true, BALANCED),
        (true, Method::DeflateBest),
        (false, BALANCED),
    ] {
        {
            let mut first: Option<Vec<u8>> = None;
            for threads in [1, 3, 16] {
                let (zip, _) =
                    compress_ok(std::slice::from_ref(&root), &opts_d(m, detect, threads));
                match &first {
                    None => first = Some(zip),
                    Some(f) => {
                        assert!(*f == zip, "{m:?} detect {detect} threads {threads} differs")
                    }
                }
            }
        }
    }
}

/// Every method stores the same files (detection doesn't depend on the
/// method), so Best is still never larger than level 9, file by file.
#[test]
fn methods_store_the_same_files() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("tree");
    mixed(&root);
    let methods = |m| {
        let (zip, _) = compress_ok(std::slice::from_ref(&root), &opts_d(m, true, 8));
        read_entries(&zip)
    };
    let l9 = methods(Method::Deflate { level: 9 });
    let stored = |es: &[Entry]| -> Vec<String> {
        es.iter()
            .filter(|e| !e.is_dir && e.method == Stored && e.size >= 512 * KIB as u64)
            .map(|e| e.name.clone())
            .collect()
    };
    let want = stored(&l9);
    assert_eq!(
        want,
        ["tree/a/clip.mp4", "tree/a/photo.heic", "tree/b/video.mov"],
        "detected set"
    );
    for m in [Method::Deflate { level: 1 }, BALANCED, Method::DeflateBest] {
        let es = methods(m);
        assert_eq!(stored(&es), want, "{m:?}");
        if m == Method::DeflateBest {
            for (b, n) in es.iter().zip(&l9) {
                assert!(b.compressed_size <= n.compressed_size, "{}", b.name);
            }
        }
    }
}

/// Zopfli too. Release-only: Zopfli is slow in debug.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "zopfli is slow in debug; run with --release"
)]
fn zopfli_stores_the_same_files() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("clip.mp4");
    write_file(&f, &random_bytes(3 * MIB, 61));
    let big = td.path().join("video.mov");
    write_file(&big, &random_bytes(5 * MIB, 62));
    for p in [&f, &big] {
        let e = one(p, &opts_d(Method::Zopfli { iterations: 15 }, true, 8));
        assert_eq!(e.method, Stored, "{}", p.display());
    }
}

/// Archives with detected entries pass `test_archive` and extract to the
/// original tree.
#[test]
fn round_trip_through_test_and_extract() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("tree");
    mixed(&root);
    let cancel = AtomicBool::new(false);
    for detect in [true, false] {
        let out = td.path().join(format!("out-{detect}.zip"));
        let o = opts_d(BALANCED, detect, 0);
        zipcore::compress_to_path(std::slice::from_ref(&root), &out, &o, &mut |_| {}, &cancel)
            .unwrap();
        let report = zipcore::test_archive(&out, &mut |_| {}, &cancel).unwrap();
        assert!(report.is_ok(), "detect {detect}: {:?}", report.problems);
        let dest: PathBuf = td.path().join(format!("x-{detect}"));
        zipcore::extract_archive(
            &out,
            &dest,
            &ExtractOptions::default(),
            &mut |_| {},
            &cancel,
        )
        .unwrap();
        assert_nodes_eq(&snapshot(&dest.join("tree")), &snapshot(&root));
        let zip = std::fs::read(&out).unwrap();
        let entries = verify_archive(&zip, &expected_entries(&root), "round trip");
        let bad = size_rule_problems(&entries);
        assert!(bad.is_empty(), "{bad:?}");
    }
}
