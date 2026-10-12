//! Phase 3: `Method::Zopfli`.
//!
//! - Output is plain Deflate (method 8) or Stored; never another method.
//! - For whole-file units (< 4 MiB) the engine keeps the smaller of Zopfli and
//!   zlib-rs level 9, so each entry is <= the Deflate { level: 9 } entry.
//! - On real-ish text Zopfli beats Deflate 9 in total (asserted >= 0.5%).
//! - Odd iteration counts (0, 1, 1000) do not panic.
//! - Cancellation is prompt even though a Zopfli call is not interruptible.
//!
//! Heavy cases are release-only: `cargo test -p zipcore --release --test zopfli`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use zip::CompressionMethod;
use zipcore::{Error, Method};

const Z15: Method = Method::Zopfli { iterations: 15 };
const D9: Method = Method::Deflate { level: 9 };

fn corpus_tree(parent: &Path, total: usize) -> PathBuf {
    let root = parent.join("corpus");
    let mut left = total;
    let mut i = 0u64;
    while left > 0 {
        let n = left.min(10_000 + (i as usize * 7919) % 30_000);
        let data = if i % 4 == 3 {
            source_like(n, i)
        } else {
            text_corpus(n, i + 1)
        };
        write_file(&root.join(format!("doc{i:03}.txt")), &data);
        left -= n;
        i += 1;
    }
    root
}

fn compress_checked(root: &Path, method: Method, threads: usize) -> Vec<Entry> {
    let (zip, _) = compress_ok(
        std::slice::from_ref(&root.to_path_buf()),
        &opts_t(method, threads),
    );
    let how = format!("{method:?}");
    let entries = verify_archive(&zip, &expected_entries(root), &how);
    assert_methods_store_or_deflate(&zip, &how);
    let bad = size_rule_problems(&entries);
    assert!(bad.is_empty(), "{how}: size rule: {bad:?}");
    entries
}

/// Per entry Zopfli <= Deflate 9, and the total is at least 0.5% smaller.
#[test]
fn zopfli_beats_deflate9_on_text_corpus() {
    let td = tempfile::tempdir().unwrap();
    let total = if cfg!(debug_assertions) {
        80_000
    } else {
        400_000
    };
    let root = corpus_tree(td.path(), total);
    let z = compress_checked(&root, Z15, 0);
    let d = compress_checked(&root, D9, 0);
    let dm = by_name(&d);
    let mut problems = Vec::new();
    for e in z.iter().filter(|e| !e.is_dir) {
        let o = &dm[&e.name];
        if e.compressed_size > o.compressed_size {
            problems.push(format!(
                "{}: zopfli {} > deflate9 {}",
                e.name, e.compressed_size, o.compressed_size
            ));
        }
        assert_eq!(e.method, CompressionMethod::Deflated, "{}", e.name);
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    let (tz, td9) = (total_compressed(&z), total_compressed(&d));
    let gain = 1.0 - tz as f64 / td9 as f64;
    eprintln!(
        "zopfli {tz} vs deflate9 {td9}: {:.2}% smaller",
        gain * 100.0
    );
    assert!(
        gain >= 0.005,
        "Zopfli only {:.3}% smaller than Deflate 9 ({tz} vs {td9}): is Zopfli actually used?",
        gain * 100.0
    );
}

/// Per entry Zopfli <= Deflate 9 on varied small inputs, including ones where
/// zlib-rs might win (tiny, highly repetitive, incompressible, mixed).
#[test]
fn zopfli_never_larger_than_deflate9_per_entry() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("v");
    let mut mixed = text_corpus(3_000, 1);
    mixed.extend(random_bytes(3_000, 2));
    mixed.extend(text_corpus(3_000, 3));
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("one", b"z".to_vec()),
        ("two", b"zz".to_vec()),
        ("short", b"hello hello hello hello\n".to_vec()),
        ("zeros", vec![0u8; 20_000]),
        ("pattern", compressible_bytes(10_000)),
        ("random", random_bytes(30_000, 4)),
        ("random_tiny", random_bytes(40, 5)),
        ("mixed", mixed),
        ("ramp", (0..=255u8).cycle().take(5_000).collect()),
        ("text", text_corpus(8_000, 6)),
        ("src", source_like(6_000, 7)),
    ];
    for (n, d) in &files {
        write_file(&root.join(n), d);
    }
    let z = compress_checked(&root, Z15, 4);
    let d = by_name(&compress_checked(&root, D9, 4));
    let mut problems = Vec::new();
    for e in z.iter().filter(|e| !e.is_dir) {
        let o = &d[&e.name];
        if e.compressed_size > o.compressed_size {
            problems.push(format!(
                "{}: zopfli {} ({:?}) > deflate9 {} ({:?})",
                e.name, e.compressed_size, e.method, o.compressed_size, o.method
            ));
        }
        // Incompressible input stays Stored.
        if e.name.ends_with("random") || e.name.ends_with("random_tiny") {
            assert_eq!(e.method, CompressionMethod::Stored, "{}", e.name);
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Iteration counts at the edges do not panic and round-trip.
#[test]
fn zopfli_iteration_edge_cases() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("it");
    write_file(&root.join("a.txt"), &text_corpus(6_000, 1));
    write_file(&root.join("b.txt"), b"tiny tiny tiny");
    write_file(&root.join("e"), b"");
    write_file(&root.join("r"), &random_bytes(2_000, 2));
    for iterations in [0u16, 1, 2] {
        compress_checked(&root, Method::Zopfli { iterations }, 3);
    }
    let tiny = td.path().join("tiny");
    write_file(&tiny.join("t.txt"), &text_corpus(600, 3));
    write_file(&tiny.join("u.txt"), b"abcabcabcabc");
    let tiny2 = tiny.clone();
    with_timeout(120, "zopfli 1000 iterations on a tiny input", move || {
        compress_checked(&tiny2, Method::Zopfli { iterations: 1000 }, 2);
    });
    // u16::MAX on a few bytes: must not panic (time is bounded by input size).
    let tiny3 = td.path().join("tiny3");
    write_file(&tiny3.join("x"), b"xyxyxyxyxyxyxyxy");
    with_timeout(120, "zopfli u16::MAX iterations on 16 bytes", move || {
        compress_checked(
            &tiny3,
            Method::Zopfli {
                iterations: u16::MAX,
            },
            1,
        );
    });
}

/// Incompressible data skips Zopfli: a 3 MiB random file under Max is Stored
/// and takes about as long as Deflate 9 would, not minutes.
#[test]
fn zopfli_skips_incompressible_quickly() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("rnd");
    write_file(&root.join("r.bin"), &random_bytes(3 * MIB, 1));
    write_file(&root.join("r2.bin"), &random_bytes(200_000, 2));
    let r = root.clone();
    let entries = with_timeout(60, "Max on 3 MiB random", move || {
        compress_checked(&r, Z15, 2)
    });
    for e in entries.iter().filter(|e| !e.is_dir) {
        assert_eq!(e.method, CompressionMethod::Stored, "{}", e.name);
        assert_eq!(e.compressed_size, e.size);
    }
}

/// Large (chunked) files under Zopfli: round-trip and size rule only.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "zopfli on several MiB is slow in debug; run with --release"
)]
fn zopfli_chunked_files_round_trip() {
    let td = tempfile::tempdir().unwrap();
    let src = td.path().join("src");
    let root = src.join("zbig");
    let mut a = text_corpus(4 * MIB, 1);
    a.extend(random_bytes(MIB + 3, 2));
    a.extend(source_like(700_001, 3));
    write_file(&root.join("a.txt"), &a);
    // Exactly 4 MiB: the first size on the chunked path.
    write_file(&root.join("b_4m.txt"), &text_corpus(4 * MIB, 4));
    let e = compress_checked(&root, Z15, 0);
    for x in e.iter().filter(|e| !e.is_dir) {
        assert_eq!(x.method, CompressionMethod::Deflated, "{}", x.name);
    }
    let td_path = td.path().join("out");
    fs::create_dir_all(&td_path).unwrap();
    let zp = td_path.join("z.zip");
    compress_path_ok(std::slice::from_ref(&root), &zp, &opts_t(Z15, 0));
    external_validate(&zp, 3, "zopfli chunked");
    assert_external_extracts(&zp, &src, &td_path, "zopfli chunked");
}

/// Cancel during Max with 8 threads: Cancelled within ~3 s of the flag, no
/// leftovers from `compress_to_path`.
#[test]
fn cancel_during_max_with_8_threads() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("cx");
    // Debug zopfli is ~50x slower (one 16 KB unit takes seconds with 8
    // threads busy), so debug uses small files and a lenient latency bound;
    // the spec's 3 s bound is asserted in release builds.
    let (n, each) = if cfg!(debug_assertions) {
        (150, 3_000)
    } else {
        (300, 40_000)
    };
    for i in 0..n {
        write_file(
            &root.join(format!("d{}/f{i:04}.txt", i % 7)),
            &text_corpus(each + i, i as u64),
        );
    }
    if !cfg!(debug_assertions) {
        write_file(&root.join("zz_big.txt"), &text_corpus(6 * MIB, 99));
    }
    let out_dir = td.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    let out = out_dir.join("max.zip");
    let (res, lag) = with_timeout(15, "cancelled Max run", move || {
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        let set_at = Arc::new(std::sync::Mutex::new(None::<Instant>));
        let s2 = set_at.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            *s2.lock().unwrap() = Some(Instant::now());
            c2.store(true, Ordering::SeqCst);
        });
        let r = zipcore::compress_to_path(&[root], &out, &opts_t(Z15, 8), &mut |_| {}, &cancel);
        let done = Instant::now();
        t.join().unwrap();
        let at = *set_at.lock().unwrap();
        (r, at.map(|a| done.saturating_duration_since(a)))
    });
    let err = res.expect_err("Max run finished before a 600 ms cancel? input too small");
    assert!(matches!(err, Error::Cancelled), "got {err:?}");
    let lag = lag.expect("cancel flag was never set");
    eprintln!("Max cancel latency: {lag:?}");
    let limit = Duration::from_secs(if cfg!(debug_assertions) { 10 } else { 3 });
    assert!(
        lag < limit,
        "took {lag:?} after cancel to stop (limit {limit:?})"
    );
    assert!(
        list_dir(&out_dir).is_empty(),
        "leftovers: {:?}",
        list_dir(&out_dir)
    );
}

/// Zopfli entries are ordinary Deflate: the raw local data decodes with an
/// independent inflater (flate2) to the original bytes.
#[test]
fn zopfli_streams_inflate_independently() {
    use std::io::Read;
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("inf");
    write_file(&root.join("a.txt"), &text_corpus(30_000, 1));
    write_file(&root.join("b.rs"), &source_like(20_000, 2));
    let (zip, _) = compress_ok(std::slice::from_ref(&root), &opts_t(Z15, 2));
    let raw = parse_raw_bytes(&zip);
    let mut checked = 0;
    for (cd, local) in &raw.entries {
        if cd.method != 8 {
            continue;
        }
        let start = (cd.offset + local.header_len) as usize;
        let data = &zip[start..start + cd.csize as usize];
        let mut out = Vec::new();
        flate2::read::DeflateDecoder::new(data)
            .read_to_end(&mut out)
            .unwrap_or_else(|e| {
                panic!(
                    "{:?}: inflate failed: {e}",
                    String::from_utf8_lossy(&cd.name)
                )
            });
        assert_eq!(out.len() as u64, cd.usize);
        assert_eq!(crc32fast_hash(&out), cd.crc);
        checked += 1;
    }
    assert_eq!(checked, 2);
}

fn crc32fast_hash(d: &[u8]) -> u32 {
    // Bitwise CRC-32 (IEEE), independent of the engine's crc32fast.
    let mut c = !0u32;
    for &b in d {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// Cancel while a large Zopfli (chunked) file is in flight. Release-only.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "zopfli on MiBs is slow in debug; run with --release"
)]
fn cancel_during_max_chunked_file() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("cbig");
    write_file(&root.join("a.txt"), b"first");
    write_file(&root.join("b_big.txt"), &text_corpus(12 * MIB, 5));
    let out_dir = td.path().join("out");
    fs::create_dir(&out_dir).unwrap();
    let out = out_dir.join("max.zip");
    let (res, lag, via_progress) = with_timeout(15, "cancelled Max chunked run", move || {
        let cancel = AtomicBool::new(false);
        let at = std::sync::Mutex::new(None::<(Instant, bool)>);
        let fire = |via_progress: bool| {
            let mut g = at.lock().unwrap();
            if g.is_none() {
                *g = Some((Instant::now(), via_progress));
                cancel.store(true, Ordering::SeqCst);
            }
        };
        let mut cb = |p: &zipcore::Progress| {
            if p.current == "cbig/b_big.txt" && p.bytes_done > 0 && p.bytes_done < p.bytes_total {
                fire(true);
            }
        };
        // Fallback in case progress only arrives per finished chunk.
        let r = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(1500));
                fire(false);
            });
            zipcore::compress_to_path(&[root], &out, &opts_t(Z15, 8), &mut cb, &cancel)
        });
        let done = Instant::now();
        let (t, via) = at.lock().unwrap().expect("cancel never fired");
        (r, done.saturating_duration_since(t), via)
    });
    let err = res.expect_err("Max run on 12 MiB of text finished before the cancel?");
    assert!(matches!(err, Error::Cancelled), "got {err:?}");
    eprintln!("Max chunked cancel latency: {lag:?} (fired by progress: {via_progress})");
    assert!(
        lag < Duration::from_secs(3),
        "took {lag:?} after cancel to stop"
    );
    assert!(
        list_dir(&out_dir).is_empty(),
        "leftovers: {:?}",
        list_dir(&out_dir)
    );
}
