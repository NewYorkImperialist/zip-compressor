//! Phase 3: the named presets (Fast / Balanced / Best / Max).
//!
//! - Every preset round-trips a mixed tree and every entry is Stored or
//!   Deflate (method 0 / 8), extractable by unzip, python, bsdtar and ditto.
//! - Best (`Method::DeflateBest`) tries levels 6 and 9 (and level 9
//!   `FILTERED`) per file under 1 MiB,
//!   so per small entry it is never larger than an independent zlib-rs at
//!   levels 6 and 9; `Deflate { level: 9 }` is a single level-9 pass.
//! - Max (Zopfli) is never larger per small entry than Best and is smaller in
//!   total than Balanced on text.
//! - Archives are byte-identical for any thread count.
//!
//! Zopfli is slow in debug builds; the heaviest Max cases are release-only
//! (`#[cfg_attr(debug_assertions, ignore)]`). Run them with
//! `cargo test -p zipcore --release`.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::*;
use proptest::prelude::*;
use zip::CompressionMethod;
use zipcore::{Method, Preset};

// ---------------------------------------------------------------------------
// API
// ---------------------------------------------------------------------------

#[test]
fn preset_api_contract() {
    assert_eq!(Preset::default(), Preset::Balanced);
    assert_eq!(
        Preset::ALL,
        [Preset::Fast, Preset::Balanced, Preset::Best, Preset::Max]
    );
    let names: Vec<&str> = Preset::ALL.iter().map(|p| p.name()).collect();
    assert_eq!(names, ["fast", "balanced", "best", "max"]);
    let mut uniq = names.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(
        uniq.len(),
        names.len(),
        "preset names not unique: {names:?}"
    );
    assert_eq!(Preset::Fast.method(), Method::Deflate { level: 1 });
    assert_eq!(Preset::Balanced.method(), Method::Deflate { level: 6 });
    assert_eq!(Preset::Best.method(), Method::DeflateBest);
    assert_eq!(Preset::Max.method(), Method::Zopfli { iterations: 15 });
    // The default method is the default preset's.
    assert_eq!(Method::default(), Preset::default().method());
    assert_eq!(
        zipcore::CompressOptions::default().method,
        Preset::Balanced.method()
    );
}

// ---------------------------------------------------------------------------
// Mixed-tree round trip for every preset
// ---------------------------------------------------------------------------

/// Text, source, binary, random, empty, unicode names (chosen without
/// Unicode-normalization variants so APFS/ditto keep them byte-identical),
/// optionally a 5 MiB compressible (chunked) file.
fn preset_tree(parent: &Path, big: bool) -> PathBuf {
    let root = parent.join("tree");
    write_file(&root.join("docs/readme.txt"), &text_corpus(40_000, 1));
    write_file(&root.join("docs/short.txt"), b"hello\n");
    write_file(&root.join("src/main.rs"), &source_like(30_000, 2));
    write_file(&root.join("src/lib.rs"), &source_like(7_777, 3));
    let bin: Vec<u8> = (0..20_000u32)
        .flat_map(|i| (i.wrapping_mul(2_654_435_761) >> 13).to_le_bytes())
        .collect();
    write_file(&root.join("bin/table.bin"), &bin);
    let all: Vec<u8> = (0..=255u8).cycle().take(256 * 33).collect();
    write_file(&root.join("bin/all_bytes"), &all);
    write_file(&root.join("bin/random.dat"), &random_bytes(50_001, 11));
    write_file(&root.join("bin/one_byte"), b"x");
    write_file(&root.join("empty.bin"), b"");
    fs::create_dir_all(root.join("empty_dir")).unwrap();
    write_file(&root.join("日本語/ファイル.txt"), &text_corpus(3_000, 4));
    write_file(&root.join("emoji 🎉/straße Ωmega.txt"), b"unicode ok\n");
    if big {
        let mut d = text_corpus(5 * MIB - 3, 5);
        d.extend_from_slice(b"end");
        write_file(&root.join("big/text_5m.txt"), &d);
    }
    root
}

fn check_preset_round_trip(preset: Preset, big: bool) {
    let td = tempfile::tempdir().unwrap();
    let src = td.path().join("src");
    let root = preset_tree(&src, big);
    let expected = expected_entries(&root);
    let work = td.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let zip_path = work.join(format!("{}.zip", preset.name()));
    let how = format!("preset {}", preset.name());
    let (zip, summary) = compress_path_ok(
        std::slice::from_ref(&root),
        &zip_path,
        &opts_t(preset.method(), 0),
    );
    let entries = verify_archive(&zip, &expected, &how);
    assert_methods_store_or_deflate(&zip, &how);
    let bad = size_rule_problems(&entries);
    assert!(bad.is_empty(), "{how}: size rule: {bad:?}");
    assert_eq!(summary.bytes_out, zip.len() as u64);
    // Compressible content must actually be deflated.
    for name in ["tree/docs/readme.txt", "tree/src/main.rs"] {
        let e = find(&entries, name);
        assert_eq!(e.method, CompressionMethod::Deflated, "{how}: {name}");
        assert!(
            e.compressed_size < e.size / 2,
            "{how}: {name} barely compressed ({} -> {})",
            e.size,
            e.compressed_size
        );
    }
    if big {
        let e = find(&entries, "tree/big/text_5m.txt");
        assert_eq!(e.method, CompressionMethod::Deflated, "{how}: big file");
        assert!(e.compressed_size < e.size / 2, "{how}: big file");
    }
    assert_eq!(
        find(&entries, "tree/bin/random.dat").method,
        CompressionMethod::Stored,
        "{how}: random.dat"
    );
    external_validate(&zip_path, expected.len(), &how);
    assert_external_extracts(&zip_path, &src, &work, &how);
}

#[test]
fn fast_round_trips_mixed_tree() {
    check_preset_round_trip(Preset::Fast, true);
}

#[test]
fn balanced_round_trips_mixed_tree() {
    check_preset_round_trip(Preset::Balanced, true);
}

#[test]
fn best_round_trips_mixed_tree() {
    check_preset_round_trip(Preset::Best, true);
}

/// Max without the 5 MiB file (debug-friendly).
#[test]
fn max_round_trips_mixed_tree_small() {
    check_preset_round_trip(Preset::Max, false);
}

/// Max including the 5 MiB chunked file. Release-only (slow in debug).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "zopfli on 5 MiB is slow in debug; run with --release"
)]
fn max_round_trips_mixed_tree_with_big_file() {
    check_preset_round_trip(Preset::Max, true);
}

// ---------------------------------------------------------------------------
// Size ordering
// ---------------------------------------------------------------------------

/// A tree of small text / source files.
fn text_tree(parent: &Path, files: usize, each: usize) -> PathBuf {
    let root = parent.join("text");
    for i in 0..files {
        let len = each + (i * 977) % (each / 2 + 1);
        let data = if i % 3 == 2 {
            source_like(len, i as u64)
        } else {
            text_corpus(len, i as u64 + 100)
        };
        write_file(&root.join(format!("d{}/f{i:03}.txt", i % 4)), &data);
    }
    write_file(&root.join("tiny.txt"), b"a");
    write_file(&root.join("rnd.bin"), &random_bytes(5_000, 3));
    root
}

fn compress_preset(root: &Path, preset: Preset, threads: usize) -> (Vec<u8>, Vec<Entry>) {
    let (zip, _) = compress_ok(
        std::slice::from_ref(&root.to_path_buf()),
        &opts_t(preset.method(), threads),
    );
    let entries = verify_archive(&zip, &expected_entries(root), preset.name());
    assert_methods_store_or_deflate(&zip, preset.name());
    let bad = size_rule_problems(&entries);
    assert!(bad.is_empty(), "{}: size rule: {bad:?}", preset.name());
    (zip, entries)
}

/// Per small entry: Best <= independent min of zlib-rs levels 6 and 9 (and
/// never above Stored); Max <= Best. In total: Max <= Best <= independent
/// level 9, and Max < Balanced.
#[test]
fn size_ordering_on_text_tree() {
    let td = tempfile::tempdir().unwrap();
    let (files, each) = if cfg!(debug_assertions) {
        (12, 6_000)
    } else {
        (40, 12_000)
    };
    let root = text_tree(td.path(), files, each);
    let (_, best) = compress_preset(&root, Preset::Best, 4);
    let (_, bal) = compress_preset(&root, Preset::Balanced, 4);
    let (_, max) = compress_preset(&root, Preset::Max, 4);
    let best_m = by_name(&best);
    let max_m = by_name(&max);
    let mut indep9_total = 0u64;
    let mut problems = Vec::new();
    for e in best.iter().filter(|e| !e.is_dir) {
        let i9 = indep_deflate_len(&e.data, 9) as u64;
        let imin = i9.min(indep_deflate_len(&e.data, 6) as u64);
        indep9_total += i9.min(e.size);
        let bound = imin.min(e.size);
        if e.compressed_size > bound {
            problems.push(format!(
                "Best {}: {} > independent min(6, 9, stored) {} (L9 {i9})",
                e.name, e.compressed_size, bound
            ));
        }
        let m = &max_m[&e.name];
        if m.compressed_size > e.compressed_size {
            problems.push(format!(
                "Max {}: {} > Best {}",
                e.name, m.compressed_size, e.compressed_size
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    assert_eq!(best_m.len(), max_m.len());
    let (tmax, tbest, tbal) = (
        total_compressed(&max),
        total_compressed(&best),
        total_compressed(&bal),
    );
    eprintln!("totals: max {tmax}, best {tbest}, indep L9 {indep9_total}, balanced {tbal}");
    assert!(tmax <= tbest, "total Max {tmax} > Best {tbest}");
    assert!(
        tbest <= indep9_total,
        "total Best {tbest} > independent L9 {indep9_total}"
    );
    assert!(tmax < tbal, "total Max {tmax} not < Balanced {tbal}");
}

/// DeflateBest per entry <= independent min(level 6, level 9, stored) for
/// files under 1 MiB (level 9 alone above), and Deflate 9 is exactly one
/// level-9 pass (or Stored), across varied inputs (text, source,
/// binary-ish, short, highly repetitive).
#[test]
fn best_never_loses_to_independent_levels_6_to_9() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("b9");
    let mut inputs: Vec<(String, Vec<u8>)> = vec![
        ("text_small".into(), text_corpus(2_000, 1)),
        ("text_mid".into(), text_corpus(90_000, 2)),
        ("src".into(), source_like(60_000, 3)),
        ("pattern".into(), compressible_bytes(200_000)),
        ("zeros".into(), vec![0u8; 300_000]),
        ("short".into(), b"abcabcabcabcabc".to_vec()),
        ("mixed".into(), {
            let mut v = text_corpus(30_000, 4);
            v.extend(random_bytes(20_000, 5));
            v.extend(text_corpus(30_000, 6));
            v
        }),
        (
            "bytes_ramp".into(),
            (0..=255u8).cycle().take(70_000).collect(),
        ),
    ];
    // Just under the 4 MiB whole-file limit (release only: several encoder
    // passes over 4 MiB, twice, are slow in debug).
    if !cfg!(debug_assertions) {
        inputs.push(("near_4m".into(), text_corpus(4 * MIB - 1, 7)));
    }
    for (n, d) in &inputs {
        write_file(&root.join(n), d);
    }
    // (independent level 9, bound for DeflateBest)
    let indep: std::collections::BTreeMap<String, (u64, u64)> = inputs
        .iter()
        .map(|(n, d)| {
            let i9 = indep_deflate_len(d, 9) as u64;
            let bound = if d.len() < MIB {
                i9.min(indep_deflate_len(d, 6) as u64)
            } else {
                i9
            };
            (format!("b9/{n}"), (i9, bound))
        })
        .collect();
    for threads in [1, 8] {
        let (zip, _) = compress_ok(
            std::slice::from_ref(&root),
            &opts_t(Method::DeflateBest, threads),
        );
        let entries = verify_archive(&zip, &expected_entries(&root), "b9");
        let mut problems = Vec::new();
        for e in entries.iter().filter(|e| !e.is_dir) {
            let (_, bound) = indep[&e.name];
            if e.compressed_size > bound.min(e.size) {
                problems.push(format!(
                    "{}: {} > independent min {}",
                    e.name, e.compressed_size, bound
                ));
            }
        }
        let (zip9, _) = compress_ok(
            std::slice::from_ref(&root),
            &opts_t(Method::Deflate { level: 9 }, threads),
        );
        for e in verify_archive(&zip9, &expected_entries(&root), "b9 level 9")
            .iter()
            .filter(|e| !e.is_dir)
        {
            let (i9, _) = indep[&e.name];
            let want = if i9 < e.size { i9 } else { e.size };
            if e.compressed_size != want {
                problems.push(format!(
                    "level 9 {}: {} != single level-9 pass {}",
                    e.name, e.compressed_size, want
                ));
            }
        }
        assert!(
            problems.is_empty(),
            "threads {threads}: {}",
            problems.join("\n")
        );
    }
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

fn assert_deterministic(root: &Path, method: Method, threads: &[usize]) {
    let (reference, _) = compress_ok(
        std::slice::from_ref(&root.to_path_buf()),
        &opts_t(method, 1),
    );
    let entries = verify_archive(&reference, &expected_entries(root), "threads 1");
    assert_methods_store_or_deflate(&reference, "threads 1");
    let bad = size_rule_problems(&entries);
    assert!(bad.is_empty(), "{bad:?}");
    for &t in threads {
        let (zip, _) = compress_ok(
            std::slice::from_ref(&root.to_path_buf()),
            &opts_t(method, t),
        );
        assert!(
            zip == reference,
            "{method:?}: threads {t} archive differs from threads 1 ({} vs {} bytes)",
            zip.len(),
            reference.len()
        );
    }
}

#[test]
fn best_deterministic_across_threads_small() {
    let td = tempfile::tempdir().unwrap();
    let root = text_tree(td.path(), 20, 4_000);
    assert_deterministic(&root, Preset::Best.method(), &[3, 8]);
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "multi-setting Best on 5 MiB, thrice, is slow in debug; run with --release"
)]
fn best_deterministic_across_threads_with_chunked_file() {
    let td = tempfile::tempdir().unwrap();
    let root = text_tree(td.path(), 20, 4_000);
    let mut big = text_corpus(3 * MIB, 8);
    big.extend(random_bytes(MIB, 9));
    big.extend(source_like(MIB + 4321, 10));
    write_file(&root.join("big.txt"), &big);
    assert_deterministic(&root, Preset::Best.method(), &[3, 8]);
}

#[test]
fn max_deterministic_across_threads_small() {
    let td = tempfile::tempdir().unwrap();
    let root = text_tree(td.path(), 6, 1_500);
    assert_deterministic(&root, Preset::Max.method(), &[3, 8]);
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "zopfli on a chunked file is slow in debug; run with --release"
)]
fn max_deterministic_across_threads_with_chunked_file() {
    let td = tempfile::tempdir().unwrap();
    let root = text_tree(td.path(), 30, 8_000);
    let mut big = text_corpus(4 * MIB, 12);
    big.extend(random_bytes(MIB / 2, 13));
    big.extend(text_corpus(MIB + 17, 14));
    write_file(&root.join("big.txt"), &big);
    assert_deterministic(&root, Preset::Max.method(), &[3, 8]);
}

#[test]
fn fast_and_balanced_deterministic_across_threads() {
    let td = tempfile::tempdir().unwrap();
    let root = text_tree(td.path(), 20, 4_000);
    write_file(&root.join("big.txt"), &text_corpus(5 * MIB, 15));
    for p in [Preset::Fast, Preset::Balanced] {
        assert_deterministic(&root, p.method(), &[3, 8]);
    }
}

// ---------------------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------------------

fn content(kind: u8, len: usize, seed: u64) -> Vec<u8> {
    match kind % 5 {
        0 => random_bytes(len, seed),
        1 => text_corpus(len, seed),
        2 => source_like(len, seed),
        3 => vec![(seed & 0xff) as u8; len],
        _ => Vec::new(),
    }
}

fn build_flat_tree(root: &Path, files: &[(u8, u8, usize, u64)]) {
    fs::create_dir_all(root).unwrap();
    for (i, (dir, kind, len, seed)) in files.iter().enumerate() {
        let p = match dir % 4 {
            0 => root.join(format!("f{i}")),
            1 => root.join(format!("a/f{i}.txt")),
            2 => root.join(format!("a/b/ü {i}")),
            _ => root.join(format!("日本/f{i}")),
        };
        write_file(&p, &content(*kind, *len, *seed));
    }
}

fn prop_check(root: &Path, method: Method, threads: usize) -> Result<(), TestCaseError> {
    let (zip, summary) = compress_ok(
        std::slice::from_ref(&root.to_path_buf()),
        &opts_t(method, threads),
    );
    let (zip1, _) = compress_ok(
        std::slice::from_ref(&root.to_path_buf()),
        &opts_t(method, 1),
    );
    prop_assert!(
        zip == zip1,
        "{:?} threads={} differs from threads=1",
        method,
        threads
    );
    let raw = parse_raw_bytes(&zip);
    let layout = layout_problems(&raw);
    prop_assert!(layout.is_empty(), "layout: {:?}", layout);
    for (cd, _) in &raw.entries {
        prop_assert!(cd.method == 0 || cd.method == 8, "method {}", cd.method);
    }
    let entries = read_entries(&zip);
    let rule = size_rule_problems(&entries);
    prop_assert!(rule.is_empty(), "size rule: {:?}", rule);
    assert_nodes_eq(&archive_nodes(&entries), &expected_entries(root));
    prop_assert_eq!(summary.bytes_out, zip.len() as u64);
    Ok(())
}

fn preset_strategy() -> impl Strategy<Value = Preset> {
    prop_oneof![
        Just(Preset::Fast),
        Just(Preset::Balanced),
        Just(Preset::Best)
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn presets_random_trees(
        files in prop::collection::vec((any::<u8>(), any::<u8>(), 0usize..40_000, any::<u64>()), 0..10),
        preset in preset_strategy(),
        threads in 1usize..=8,
    ) {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("t");
        build_flat_tree(&root, &files);
        prop_check(&root, preset.method(), threads)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 4, ..ProptestConfig::default() })]

    #[test]
    fn max_random_tiny_trees(
        files in prop::collection::vec((any::<u8>(), any::<u8>(), 0usize..3_000, any::<u64>()), 0..5),
        iterations in prop_oneof![Just(0u16), Just(1u16), Just(15u16)],
        threads in 2usize..=8,
    ) {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("t");
        build_flat_tree(&root, &files);
        prop_check(&root, Method::Zopfli { iterations }, threads)?;
    }
}

/// Progress for every preset: monotonic, ends at the totals.
#[test]
fn progress_monotonic_for_every_preset() {
    let td = tempfile::tempdir().unwrap();
    let root = text_tree(td.path(), 6, 1_500);
    let total_bytes: u64 = snapshot(&root)
        .values()
        .map(|n| match n {
            Node::File(d) => d.len() as u64,
            _ => 0,
        })
        .sum();
    for p in Preset::ALL {
        let mut seen: Vec<zipcore::Progress> = Vec::new();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let mut cur = std::io::Cursor::new(Vec::new());
        let s = zipcore::compress(
            std::slice::from_ref(&root),
            &mut cur,
            &opts_t(p.method(), 4),
            &mut |x| seen.push(x.clone()),
            &cancel,
        )
        .unwrap();
        let last = seen.last().expect("no progress reported");
        assert_eq!(last.files_done, last.files_total, "{}", p.name());
        assert_eq!(last.bytes_done, last.bytes_total, "{}", p.name());
        assert_eq!(last.bytes_total, total_bytes, "{}", p.name());
        assert_eq!(s.bytes_in, total_bytes, "{}", p.name());
        for w in seen.windows(2) {
            assert!(
                w[1].files_done >= w[0].files_done && w[1].bytes_done >= w[0].bytes_done,
                "{}: progress went backwards: {:?} -> {:?}",
                p.name(),
                w[0],
                w[1]
            );
        }
    }
}
