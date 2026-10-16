//! Phase 2 store-vs-deflate rules and the chunked (>= 4 MiB) deflate path.
//!
//! - < 4 MiB: deflate iff strictly smaller, else store (exact).
//! - At or above 4 MiB: chunked deflate whose size is bounded by
//!   `size + size/13000 + 64`; Stored whole only when `store_incompressible`
//!   finds the file incompressible (see tests/incompressible.rs for that
//!   option's rules; the exact-rule tests here turn it off).

mod common;

use std::io::Write;

use common::*;
use zip::CompressionMethod;
use zipcore::{CompressOptions, Method};

/// [`opts_t`] without `store_incompressible`: the exact rules alone.
fn exact(method: Method, threads: usize) -> CompressOptions {
    CompressOptions {
        store_incompressible: false,
        ..opts_t(method, threads)
    }
}

/// Raw deflate size of `data` at `level` with an independent encoder.
fn deflate_len(data: &[u8], level: u32) -> usize {
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level));
    e.write_all(data).unwrap();
    e.finish().unwrap().len()
}

/// Random bytes with a run of `zeros` zero bytes spliced in at `at`.
fn random_with_zeros(len: usize, at: usize, zeros: usize, seed: u64) -> Vec<u8> {
    let mut v = random_bytes(len, seed);
    v[at..at + zeros].fill(0);
    v
}

fn saving(data: &[u8], level: u32) -> f64 {
    1.0 - deflate_len(data, level) as f64 / data.len() as f64
}

/// The JPEG-like regression: a small file deflate shrinks by only ~0.3% must
/// still be deflated by the exact rule. (With `store_incompressible`, such a
/// file can be Stored when the samples don't see its gain; see
/// tests/incompressible.rs.)
#[test]
fn small_file_saving_point_three_percent_is_deflated() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("photo.jpg");
    let data = random_with_zeros(1_000_000, 400_000, 3_200, 42);
    // At level 1 zlib-rs expands random data, so only levels 6 and 9 give
    // the ~0.3% saving this regression is about.
    for level in [6u32, 9] {
        let s = saving(&data, level);
        assert!(
            (0.0015..0.0045).contains(&s),
            "test input broken: level {level} saving is {:.3}%",
            s * 100.0
        );
    }
    write_file(&f, &data);
    for level in [6u8, 9] {
        for threads in [1, 8] {
            let (zip, _) = compress_ok(
                std::slice::from_ref(&f),
                &exact(Method::Deflate { level }, threads),
            );
            let e = &assert_one(&zip);
            assert_eq!(
                e.method,
                CompressionMethod::Deflated,
                "level {level} threads {threads}: ~0.3% saving file was stored ({} -> {})",
                e.size,
                e.compressed_size
            );
            assert!(e.compressed_size < e.size);
            assert!(e.data == data);
        }
    }
}

/// Tiny gains still count for small files: one byte saved is enough.
#[test]
fn small_file_saving_a_few_bytes_is_deflated() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("almost.bin");
    // 300 KB random + 64 zero bytes: saves a handful of bytes at most.
    let data = random_with_zeros(300_000, 150_000, 64, 7);
    let d = deflate_len(&data, 6);
    write_file(&f, &data);
    let (zip, _) = compress_ok(
        std::slice::from_ref(&f),
        &opts_t(Method::Deflate { level: 6 }, 4),
    );
    let e = assert_one(&zip);
    if d < data.len() {
        assert_eq!(
            e.method,
            CompressionMethod::Deflated,
            "independent deflate gives {d} < {}, but entry was {:?} {}",
            data.len(),
            e.method,
            e.compressed_size
        );
    } else {
        assert_eq!(e.method, CompressionMethod::Stored);
    }
    assert!(e.compressed_size <= e.size);
}

/// Without `store_incompressible`, a >= 4 MiB file is chunk-deflated at every
/// level, however little it saves (here ~0.05%, all in its first MiB), and
/// stays within the large-file size bound.
#[test]
fn large_file_without_detection_is_deflated_within_bound() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("video.mp4");
    let data = random_with_zeros(6 * MIB, 300_000, 3_300, 5);
    for level in [6u32, 9] {
        let s = saving(&data[..MIB], level);
        assert!(
            (0.0015..0.0045).contains(&s),
            "test input broken: level {level} first-MiB saving {s}"
        );
    }
    write_file(&f, &data);
    for (level, threads) in [(1u8, 1), (1, 8), (6, 1), (6, 8), (9, 3)] {
        let (zip, _) = compress_ok(
            std::slice::from_ref(&f),
            &exact(Method::Deflate { level }, threads),
        );
        let e = assert_one(&zip);
        assert!(
            e.data == data,
            "level {level} threads {threads}: data differs"
        );
        assert_eq!(
            e.method,
            CompressionMethod::Deflated,
            "level {level} threads {threads}"
        );
        assert!(
            e.compressed_size <= large_bound(e.size),
            "level {level} threads {threads}: {} -> {} exceeds bound {}",
            e.size,
            e.compressed_size,
            large_bound(e.size)
        );
        assert!(size_rule_problems(std::slice::from_ref(&e)).is_empty());
        // With detection: whatever the samples conclude, the rules hold.
        let (zip, _) = compress_ok(
            std::slice::from_ref(&f),
            &opts_t(Method::Deflate { level }, threads),
        );
        let e = assert_one(&zip);
        assert!(e.data == data);
        assert!(size_rule_problems(std::slice::from_ref(&e)).is_empty());
    }
}

/// A >= 4 MiB file that saves only ~0.01% is Stored whole at every level
/// with `store_incompressible` (the default).
#[test]
fn large_nearly_random_file_is_stored() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("almost_random.bin");
    let data = random_with_zeros(5 * MIB + 7, 200_000, 680, 21);
    for level in [6u32, 7, 8, 9] {
        let s = saving(&data[..MIB], level);
        assert!(
            (0.0001..0.0008).contains(&s),
            "test input broken: level {level} first-MiB saving {:.4}%",
            s * 100.0
        );
    }
    write_file(&f, &data);
    for (level, threads) in [(6u8, 1), (6, 8), (9, 1), (9, 8), (1, 4)] {
        let (zip, _) = compress_ok(
            std::slice::from_ref(&f),
            &opts_t(Method::Deflate { level }, threads),
        );
        let e = assert_one(&zip);
        assert_eq!(
            e.method,
            CompressionMethod::Stored,
            "level {level} threads {threads}: ~0.01% saving must store"
        );
        assert_eq!(e.compressed_size, e.size);
        assert!(e.data == data);
    }
}

/// The same rules apply to Zopfli. Release-only: Zopfli on a MiB of
/// near-random data is slow in debug.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "zopfli is slow in debug; run with --release"
)]
fn zopfli_store_and_deflate_rules() {
    let td = tempfile::tempdir().unwrap();
    let weak = td.path().join("weak.bin");
    write_file(&weak, &random_with_zeros(5 * MIB, 200_000, 680, 21));
    let ok = td.path().join("ok.bin");
    let ok_data = random_with_zeros(6 * MIB, 300_000, 3_300, 5);
    write_file(&ok, &ok_data);
    let m = Method::Zopfli { iterations: 15 };
    let (zip, _) = compress_ok(std::slice::from_ref(&weak), &opts_t(m, 8));
    let e = assert_one(&zip);
    assert_eq!(e.method, CompressionMethod::Stored, "zopfli, nearly random");
    let (zip, _) = compress_ok(std::slice::from_ref(&ok), &exact(m, 8));
    let e = assert_one(&zip);
    assert_eq!(
        e.method,
        CompressionMethod::Deflated,
        "zopfli, no detection"
    );
    assert!(e.compressed_size <= large_bound(e.size));
    assert!(e.data == ok_data);
}

/// A >= 4 MiB file whose first MiB compresses well is chunk-deflated, and the
/// incompressible remainder (which zlib-rs level 1 would expand by ~5%) stays
/// within the size bound via stored blocks.
#[test]
fn large_file_with_good_probe_is_deflated_within_bound() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("mostly_random.bin");
    let mut data = random_with_zeros(MIB, 100_000, MIB / 2, 3);
    data.extend(random_bytes(5 * MIB + 12_345, 4));
    for level in [1u32, 6, 9] {
        let s = saving(&data[..MIB], level);
        assert!(s > 0.3, "test input broken: level {level} probe saving {s}");
        let r = saving(&data[2 * MIB..3 * MIB], level);
        eprintln!(
            "level {level}: independent deflate of a random MiB saves {:.3}%",
            r * 100.0
        );
    }
    write_file(&f, &data);
    for (level, threads) in [(1u8, 1), (1, 8), (6, 8), (9, 3)] {
        {
            let (zip, _) = compress_ok(
                std::slice::from_ref(&f),
                &opts_t(Method::Deflate { level }, threads),
            );
            let e = assert_one(&zip);
            assert_eq!(
                e.method,
                CompressionMethod::Deflated,
                "level {level} threads {threads}"
            );
            assert!(
                e.compressed_size <= large_bound(e.size),
                "level {level} threads {threads}: {} -> {} exceeds bound {}",
                e.size,
                e.compressed_size,
                large_bound(e.size)
            );
            assert!(
                e.data == data,
                "level {level} threads {threads}: data differs"
            );
        }
    }
}

/// Alternating compressible / random 1 MiB chunks (and chunks straddling the
/// 1 MiB grid) round-trip and respect the bound.
#[test]
fn alternating_chunks_round_trip() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("alt");
    let mut a = Vec::new();
    for i in 0..9u64 {
        if i % 2 == 0 {
            a.extend(compressible_bytes(MIB));
        } else {
            a.extend(random_bytes(MIB, i));
        }
    }
    write_file(&root.join("aligned.bin"), &a);
    let mut b = Vec::new();
    for i in 0..13u64 {
        if i % 3 == 0 {
            b.extend(random_bytes(777_777, i));
        } else {
            b.extend(compressible_bytes(700_001));
        }
    }
    write_file(&root.join("straddling.bin"), &b);
    // Long-range repeats: a 30 KB random block repeated, so every chunk
    // depends on the 32 KiB of the previous chunk for its matches.
    let blk = random_bytes(30_000, 99);
    let c: Vec<u8> = blk.iter().copied().cycle().take(7 * MIB + 3).collect();
    write_file(&root.join("window.bin"), &c);
    let expected = expected_entries(&root);
    let mut reference = None;
    for threads in [1, 8] {
        let (zip, _) = compress_ok(
            std::slice::from_ref(&root),
            &opts_t(Method::Deflate { level: 6 }, threads),
        );
        let entries = verify_archive(&zip, &expected, &format!("threads {threads}"));
        let bad = size_rule_problems(&entries);
        assert!(bad.is_empty(), "{bad:?}");
        for e in entries.iter().filter(|e| !e.is_dir) {
            assert_eq!(e.method, CompressionMethod::Deflated, "{}", e.name);
        }
        let w = find(&entries, "alt/window.bin");
        assert!(
            w.compressed_size < w.size / 20,
            "window.bin barely compressed ({} -> {}): chunks not primed with the previous 32 KiB?",
            w.size,
            w.compressed_size
        );
        match &reference {
            None => reference = Some(zip),
            Some(r) => assert!(*r == zip, "threads {threads} differs"),
        }
    }
}

/// Small incompressible files: always stored, never larger (strict).
#[test]
fn small_incompressible_files_never_grow() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("rnd");
    for (i, n) in [
        1usize,
        2,
        100,
        65_535,
        65_536,
        65_537,
        MIB - 1,
        MIB,
        MIB + 1,
        4 * MIB - 1,
    ]
    .iter()
    .enumerate()
    {
        write_file(
            &root.join(format!("r{i:02}_{n}")),
            &random_bytes(*n, i as u64),
        );
    }
    for level in [1u8, 9] {
        let (zip, _) = compress_ok(
            std::slice::from_ref(&root),
            &opts_t(Method::Deflate { level }, 8),
        );
        let entries = verify_archive(&zip, &expected_entries(&root), "rnd");
        for e in entries.iter().filter(|e| !e.is_dir) {
            assert_eq!(
                e.method,
                CompressionMethod::Stored,
                "level {level}: {}",
                e.name
            );
            assert_eq!(e.compressed_size, e.size, "level {level}: {}", e.name);
        }
    }
}

fn assert_one(zip: &[u8]) -> Entry {
    let entries = read_entries(zip);
    assert_eq!(entries.len(), 1);
    assert_stream_matches(zip, &entries, "single file");
    assert_layout(zip, "single file");
    entries.into_iter().next().unwrap()
}
