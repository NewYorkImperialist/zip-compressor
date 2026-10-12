//! Phase 2: parallel compression must not change a single output byte.
//!
//! The "edge" tree has files on every side of the engine's size thresholds
//! (4 MiB small/large cut-over, 1 MiB chunk boundaries) and content that hits
//! each store/deflate path, and is compressed with several thread counts
//! through both `compress()` and `compress_to_path()`.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use common::*;
use zip::CompressionMethod;
use zipcore::{Method, Progress, Summary};

const DEFLATE: Method = Method::Deflate { level: 6 };
const THREADS: [usize; 5] = [1, 2, 3, 8, 0];

/// A random block of `period` bytes repeated: compressible only through
/// back-references, which cross every 1 MiB chunk boundary (period < 32 KiB,
/// so a chunk primed with the wrong dictionary decodes to garbage).
fn periodic(len: usize, period: usize, seed: u64) -> Vec<u8> {
    random_bytes(period, seed)
        .into_iter()
        .cycle()
        .take(len)
        .collect()
}

/// Expected method for selected edge-tree entries.
fn edge_tree(parent: &Path) -> (PathBuf, Vec<(&'static str, CompressionMethod)>) {
    use CompressionMethod::{Deflated, Stored};
    let root = parent.join("edge");
    let mut expect = Vec::new();
    let mut put = |name: &'static str, data: Vec<u8>, m: Option<CompressionMethod>| {
        write_file(&root.join(name), &data);
        if let Some(m) = m {
            expect.push((name, m));
        }
    };
    put("a_empty.bin", Vec::new(), None);
    put("b_one.bin", vec![b'x'], None);
    // Small (< 4 MiB) files: exact store-vs-deflate decision.
    put(
        "c_rand_4m-1.bin",
        random_bytes(4 * MIB - 1, 1),
        Some(Stored),
    );
    put(
        "c_text_4m-1.txt",
        periodic(4 * MIB - 1, 7_000, 2),
        Some(Deflated),
    );
    // Exactly at the cut-over and just above it.
    put("d_text_4m.txt", periodic(4 * MIB, 9_001, 3), Some(Deflated));
    put("d_rand_4m.bin", random_bytes(4 * MIB, 4), Some(Stored));
    put(
        "e_text_4m+1.txt",
        periodic(4 * MIB + 1, 12_345, 5),
        Some(Deflated),
    );
    // Exactly k x 1 MiB and one byte either side.
    put(
        "f_text_5m.txt",
        periodic(5 * MIB, 20_000, 6),
        Some(Deflated),
    );
    put(
        "f_text_5m-1.txt",
        periodic(5 * MIB - 1, 31_000, 7),
        Some(Deflated),
    );
    put(
        "f_text_5m+1.txt",
        compressible_bytes(5 * MIB + 1),
        Some(Deflated),
    );
    // ~20 MiB of compressible text.
    put(
        "g_big_20m.txt",
        compressible_bytes(20 * MIB + 777),
        Some(Deflated),
    );
    // First MiB compressible, rest random: chunked deflate with stored blocks.
    let mut mixed = compressible_bytes(MIB);
    mixed.extend(random_bytes(5 * MIB, 8));
    put("h_mixed_6m.bin", mixed, Some(Deflated));
    // First MiB random, rest compressible: the probe sees < 0.5% savings,
    // so the whole file is stored.
    let mut rthenc = random_bytes(MIB, 9);
    rthenc.extend(compressible_bytes(4 * MIB));
    put("i_rand_then_text_5m.bin", rthenc, Some(Stored));
    // Fully random 6 MiB: probe -> stored.
    put("j_rand_6m.bin", random_bytes(6 * MIB, 10), Some(Stored));
    // All zeros, 8 MiB: chunks compress to almost nothing.
    put("k_zeros_8m.bin", vec![0u8; 8 * MIB], Some(Deflated));
    // A few small files and directories mixed in.
    put("sub/small.txt", compressible_bytes(3000), Some(Deflated));
    put("sub/rand.bin", random_bytes(5000, 11), Some(Stored));
    put("sub/deeper/tiny.txt", b"tiny\n".to_vec(), None);
    fs::create_dir_all(root.join("sub/empty_dir")).unwrap();
    (root, expect)
}

fn check_edge_entries(entries: &[Entry], expect: &[(&str, CompressionMethod)]) {
    let mut bad = size_rule_problems(entries);
    for (name, m) in expect {
        let e = find(entries, &format!("edge/{name}"));
        if e.method != *m {
            bad.push(format!(
                "{}: expected {m:?}, got {:?} ({} -> {})",
                e.name, e.method, e.size, e.compressed_size
            ));
        }
    }
    let mixed = find(entries, "edge/h_mixed_6m.bin");
    if mixed.compressed_size < 5 * MIB as u64 {
        bad.push(format!(
            "h_mixed_6m.bin: compressed to {} < its 5 MiB random tail (data must be wrong)",
            mixed.compressed_size
        ));
    }
    for name in [
        "d_text_4m.txt",
        "e_text_4m+1.txt",
        "f_text_5m.txt",
        "f_text_5m-1.txt",
        "f_text_5m+1.txt",
        "g_big_20m.txt",
    ] {
        let e = find(entries, &format!("edge/{name}"));
        if e.compressed_size * 5 > e.size {
            bad.push(format!(
                "{name}: poor compression {} -> {} (chunk dictionary priming broken?)",
                e.size, e.compressed_size
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "store/deflate rule violations:\n  {}",
        bad.join("\n  ")
    );
}

fn same_summary(a: &Summary, b: &Summary, how: &str) {
    assert_eq!(
        (a.files, a.dirs, a.symlinks, a.bytes_in, a.bytes_out),
        (b.files, b.dirs, b.symlinks, b.bytes_in, b.bytes_out),
        "{how}: Summary differs"
    );
}

fn first_diff(a: &[u8], b: &[u8]) -> String {
    let i = a.iter().zip(b).position(|(x, y)| x != y);
    format!(
        "lengths {} vs {}, first differing byte at {:?}",
        a.len(),
        b.len(),
        i.or((a.len() != b.len()).then_some(a.len().min(b.len())))
    )
}

/// One threads=1 reference; every other thread count, through both
/// `compress()` and `compress_to_path()`, must reproduce it byte for byte.
#[test]
fn edge_tree_identical_bytes_for_all_thread_counts() {
    let td = tempfile::tempdir().unwrap();
    let (root, expect) = edge_tree(td.path());
    let inputs = [root.clone()];
    let out_dir = td.path().join("out");
    fs::create_dir(&out_dir).unwrap();

    let ref_path = out_dir.join("t1.zip");
    let (reference, ref_sum) = compress_path_ok(&inputs, &ref_path, &opts_t(DEFLATE, 1));
    let entries = verify_archive(&reference, &expected_entries(&root), "to_path threads=1");
    check_edge_entries(&entries, &expect);
    external_validate(&ref_path, entries.len(), "edge tree");

    for t in THREADS {
        // compress() for a subset (time budget), compress_to_path for all.
        if [1, 8, 0].contains(&t) {
            let (mem, sum) = compress_ok(&inputs, &opts_t(DEFLATE, t));
            assert!(
                mem == reference,
                "compress() threads={t} differs from compress_to_path() threads=1: {}",
                first_diff(&mem, &reference)
            );
            same_summary(&sum, &ref_sum, &format!("compress() threads={t}"));
        }
        if t != 1 {
            let out = out_dir.join(format!("t{t}.zip"));
            let (bytes, sum) = compress_path_ok(&inputs, &out, &opts_t(DEFLATE, t));
            assert!(
                bytes == reference,
                "compress_to_path threads={t} differs from threads=1: {}",
                first_diff(&bytes, &reference)
            );
            same_summary(&sum, &ref_sum, &format!("to_path threads={t}"));
        }
    }
    let mut want: Vec<String> = THREADS.iter().map(|t| format!("t{t}.zip")).collect();
    want.sort();
    assert_eq!(list_dir(&out_dir), want, "temp files left behind");
}

/// Other levels and the Store method are thread-count independent too.
#[test]
fn levels_and_store_identical_across_threads() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("lv");
    write_file(&root.join("big.txt"), &periodic(6 * MIB + 3, 4_321, 1));
    let mut mixed = compressible_bytes(MIB + 5);
    mixed.extend(random_bytes(3 * MIB, 2));
    write_file(&root.join("mixed.bin"), &mixed);
    write_file(&root.join("rand_small.bin"), &random_bytes(200_000, 3));
    write_file(&root.join("text_small.txt"), &compressible_bytes(200_000));
    let expected = expected_entries(&root);
    for method in [
        Method::Store,
        Method::Deflate { level: 0 },
        Method::Deflate { level: 1 },
        Method::Deflate { level: 9 },
    ] {
        let (a, _) = compress_ok(std::slice::from_ref(&root), &opts_t(method, 1));
        let entries = verify_archive(&a, &expected, &format!("{method:?} threads=1"));
        let bad = size_rule_problems(&entries);
        assert!(bad.is_empty(), "{method:?}: {bad:?}");
        if method == Method::Store {
            for e in entries.iter().filter(|e| !e.is_dir) {
                assert_eq!(e.method, CompressionMethod::Stored, "{}", e.name);
            }
        }
        let (b, _) = compress_ok(std::slice::from_ref(&root), &opts_t(method, 7));
        assert!(
            a == b,
            "{method:?}: threads=7 differs from threads=1: {}",
            first_diff(&a, &b)
        );
    }
}

#[test]
fn many_files_and_large_ones_with_8_threads() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("lots");
    for i in 0..3000usize {
        let data = match i % 4 {
            0 => compressible_bytes(i % 700),
            1 => random_bytes(i % 1500, i as u64),
            2 => Vec::new(),
            _ => format!("file number {i}\n").into_bytes(),
        };
        write_file(&root.join(format!("d{:02}/f{i:04}.dat", i % 37)), &data);
    }
    write_file(&root.join("d05/big_a.txt"), &periodic(9 * MIB, 3_333, 1));
    let mut m = compressible_bytes(MIB);
    m.extend(random_bytes(3 * MIB + 1, 2));
    write_file(&root.join("d17/big_b.bin"), &m);
    write_file(&root.join("d30/big_c.bin"), &random_bytes(5 * MIB, 3));
    write_file(
        &root.join("d30/big_d.txt"),
        &compressible_bytes(7 * MIB - 1),
    );

    let out = td.path().join("lots.zip");
    let (bytes, summary) = compress_path_ok(std::slice::from_ref(&root), &out, &opts_t(DEFLATE, 8));
    let entries = verify_archive(&bytes, &expected_entries(&root), "3000 files threads=8");
    assert_eq!(summary.files, 3004);
    let bad = size_rule_problems(&entries);
    assert!(bad.is_empty(), "{bad:?}");
    external_validate(
        &out,
        entries.len(),
        "many_files_and_large_ones_with_8_threads",
    );

    let (mem, _) = compress_ok(std::slice::from_ref(&root), &opts_t(DEFLATE, 3));
    assert!(
        mem == bytes,
        "threads=3 vs threads=8: {}",
        first_diff(&mem, &bytes)
    );
}

#[test]
fn progress_with_8_threads_is_ordered_and_on_calling_thread() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("prog");
    for i in 0..200usize {
        write_file(
            &root.join(format!("s{}/f{i:03}.txt", i % 7)),
            &compressible_bytes(1000 + 37 * i),
        );
    }
    write_file(
        &root.join("s3/zz_big.txt"),
        &periodic(12 * MIB + 1, 10_007, 4),
    );
    write_file(&root.join("s5/zz_rand.bin"), &random_bytes(5 * MIB, 5));

    let caller = std::thread::current().id();
    let mut events: Vec<(Progress, std::thread::ThreadId)> = Vec::new();
    let mut cb = |p: &Progress| events.push((p.clone(), std::thread::current().id()));
    let cancel = AtomicBool::new(false);
    let mut cur = Cursor::new(Vec::new());
    let summary = zipcore::compress(
        std::slice::from_ref(&root),
        &mut cur,
        &opts_t(DEFLATE, 8),
        &mut cb,
        &cancel,
    )
    .unwrap();
    let bytes = cur.into_inner();
    let entries = verify_archive(&bytes, &expected_entries(&root), "progress run");

    assert!(!events.is_empty(), "no progress events");
    let foreign = events.iter().filter(|(_, id)| *id != caller).count();
    assert_eq!(
        foreign,
        0,
        "{foreign} of {} progress callbacks ran on a worker thread",
        events.len()
    );
    let first = &events[0].0;
    for w in events.windows(2) {
        let (a, b) = (&w[0].0, &w[1].0);
        assert!(
            b.files_done >= a.files_done,
            "files_done backwards: {a:?} -> {b:?}"
        );
        assert!(
            b.bytes_done >= a.bytes_done,
            "bytes_done backwards: {a:?} -> {b:?}"
        );
    }
    for (p, _) in &events {
        assert_eq!(
            p.files_total, first.files_total,
            "files_total changed: {p:?}"
        );
        assert_eq!(
            p.bytes_total, first.bytes_total,
            "bytes_total changed: {p:?}"
        );
        assert!(
            p.files_done <= p.files_total && p.bytes_done <= p.bytes_total,
            "{p:?}"
        );
    }
    let last = &events.last().unwrap().0;
    assert_eq!(last.files_done, last.files_total, "final: {last:?}");
    assert_eq!(last.bytes_done, last.bytes_total, "final: {last:?}");
    assert_eq!(last.bytes_total, summary.bytes_in);
    assert!(last.files_total >= summary.files);

    // Entries are written in archive order, so `current` never moves backwards.
    let pos: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.name.as_str(), i))
        .collect();
    let mut prev = 0usize;
    for (p, _) in &events {
        if p.current.is_empty() {
            continue;
        }
        let i = *pos
            .get(p.current.as_str())
            .unwrap_or_else(|| panic!("Progress.current {:?} is not an entry", p.current));
        assert!(
            i >= prev,
            "progress went back to {:?} after entry #{prev} ({:?})",
            p.current,
            entries[prev].name
        );
        prev = i;
    }
    // Intermediate progress while the chunked file is written.
    let mid = events
        .iter()
        .filter(|(p, _)| p.current == "prog/s3/zz_big.txt")
        .map(|(p, _)| p.bytes_done)
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        mid.len() >= 2,
        "expected several progress reports while a 12 MiB file streams, got {mid:?}"
    );
}

/// Several inputs (files and dirs) at once, unchanged across thread counts.
#[test]
fn multiple_inputs_identical_across_threads() {
    let td = tempfile::tempdir().unwrap();
    let a = mixed_tree(&td.path().join("x"));
    let b = td.path().join("y/solo_big.txt");
    write_file(&b, &periodic(4 * MIB + 7, 999, 1));
    let c = td.path().join("z/notes.txt");
    write_file(&c, b"notes");
    let inputs = vec![a.clone(), b.clone(), c.clone()];
    let mut expected: BTreeMap<String, Node> = expected_entries(&a);
    expected.extend(expected_entries(&b));
    expected.extend(expected_entries(&c));
    let (r, _) = compress_ok(&inputs, &opts_t(DEFLATE, 1));
    verify_archive(&r, &expected, "multi inputs");
    for t in [2, 8, 0] {
        let (x, _) = compress_ok(&inputs, &opts_t(DEFLATE, t));
        assert!(x == r, "threads={t}: {}", first_diff(&x, &r));
    }
}
