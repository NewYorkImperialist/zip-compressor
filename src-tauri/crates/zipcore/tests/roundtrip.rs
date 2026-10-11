//! Archive layout, content round trips and compression methods.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use common::*;
use zip::CompressionMethod;
use zipcore::{Method, SymlinkPolicy};

const DEFLATE: Method = Method::Deflate { level: 6 };

#[test]
fn mixed_tree_round_trips_exactly() {
    let td = tempfile::tempdir().unwrap();
    let root = mixed_tree(td.path());
    for method in [Method::Store, DEFLATE] {
        let (zip, summary) = compress_ok(std::slice::from_ref(&root), &opts(method));
        let entries = read_entries(&zip);
        assert_safe_names(&entries);
        assert_nodes_eq(&archive_nodes(&entries), &expected_entries(&root));

        // Explicit directory entries, including the empty one and the root.
        let n = names(&entries);
        for d in [
            "root/",
            "root/empty_dir/",
            "root/deep/",
            "root/deep/x/",
            "root/deep/x/y/",
        ] {
            assert!(n.contains(&d.to_string()), "missing dir entry {d}: {n:?}");
        }

        let files = entries
            .iter()
            .filter(|e| !e.is_dir && !e.is_symlink)
            .count() as u64;
        let dirs = entries.iter().filter(|e| e.is_dir).count() as u64;
        assert_eq!(summary.files, files, "Summary.files");
        assert_eq!(summary.dirs, dirs, "Summary.dirs");
        assert_eq!(summary.symlinks, 0);
        let bytes_in: u64 = entries.iter().filter(|e| !e.is_dir).map(|e| e.size).sum();
        assert_eq!(summary.bytes_in, bytes_in, "Summary.bytes_in");
        assert_eq!(summary.bytes_out, zip.len() as u64, "Summary.bytes_out");

        // Recorded sizes and CRCs agree with the data (the reader also checked CRCs).
        for e in &entries {
            assert_eq!(e.size, e.data.len() as u64, "size of {}", e.name);
        }
    }
}

#[test]
fn parents_precede_children_and_siblings_are_sorted() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("root");
    // Created in non-sorted order on purpose.
    for name in [
        "zeta.txt",
        "alpha.txt",
        "Mid.txt",
        "beta/2.txt",
        "beta/1.txt",
        "aaa/x.txt",
    ] {
        write_file(&root.join(name), name.as_bytes());
    }
    let (zip, _) = compress_ok(std::slice::from_ref(&root), &opts(DEFLATE));
    let n = names(&read_entries(&zip));
    let pos: HashMap<&str, usize> = n.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();

    for (i, name) in n.iter().enumerate() {
        let trimmed = name.trim_end_matches('/');
        if let Some(idx) = trimmed.rfind('/') {
            let parent = format!("{}/", &trimmed[..idx]);
            let p = *pos
                .get(parent.as_str())
                .unwrap_or_else(|| panic!("parent {parent} of {name} has no entry: {n:?}"));
            assert!(p < i, "parent {parent} comes after child {name}: {n:?}");
        }
    }
    // Siblings appear in sorted order of their final component.
    let mut by_parent: HashMap<String, Vec<String>> = HashMap::new();
    for name in &n {
        let trimmed = name.trim_end_matches('/');
        let (parent, leaf) = match trimmed.rfind('/') {
            Some(i) => (trimmed[..i].to_string(), trimmed[i + 1..].to_string()),
            None => (String::new(), trimmed.to_string()),
        };
        by_parent.entry(parent).or_default().push(leaf);
    }
    for (parent, kids) in by_parent {
        let mut sorted = kids.clone();
        sorted.sort();
        assert_eq!(kids, sorted, "children of {parent:?} not sorted");
    }
}

#[test]
fn deterministic_names_and_bytes() {
    let td = tempfile::tempdir().unwrap();
    let root = mixed_tree(td.path());
    let t = filetime::FileTime::from_unix_time(1_600_000_000, 0);
    for p in walk_all(&root) {
        filetime::set_symlink_file_times(&p, t, t).unwrap();
    }
    let (a, _) = compress_ok(std::slice::from_ref(&root), &opts(DEFLATE));
    let (b, _) = compress_ok(std::slice::from_ref(&root), &opts(DEFLATE));
    assert_eq!(names(&read_entries(&a)), names(&read_entries(&b)));
    assert!(
        a == b,
        "two runs over an unchanged tree produced different bytes"
    );
}

fn walk_all(root: &std::path::Path) -> Vec<PathBuf> {
    let mut v = vec![root.to_path_buf()];
    let mut i = 0;
    while i < v.len() {
        let p = v[i].clone();
        if fs::symlink_metadata(&p).unwrap().is_dir() {
            for e in fs::read_dir(&p).unwrap() {
                v.push(e.unwrap().path());
            }
        }
        i += 1;
    }
    v
}

#[test]
fn multiple_inputs_use_their_own_names() {
    let td = tempfile::tempdir().unwrap();
    let photos = td.path().join("a/photos");
    write_file(&photos.join("p1.jpg"), &random_bytes(1000, 1));
    let notes = td.path().join("b/notes.txt");
    write_file(&notes, b"hello");
    let (zip, summary) = compress_ok(&[photos.clone(), notes.clone()], &opts(DEFLATE));
    let entries = read_entries(&zip);
    assert_safe_names(&entries);
    let mut n = names(&entries);
    n.sort();
    assert_eq!(n, vec!["notes.txt", "photos/", "photos/p1.jpg"]);
    assert_eq!(find(&entries, "notes.txt").data, b"hello");
    assert_eq!(summary.files, 2);
}

#[test]
fn single_file_input_and_trailing_slash_input() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("only.txt");
    write_file(&f, b"x");
    let (zip, _) = compress_ok(&[f], &opts(DEFLATE));
    assert_eq!(names(&read_entries(&zip)), vec!["only.txt"]);

    let d = td.path().join("dir");
    write_file(&d.join("in.txt"), b"y");
    let mut with_slash = d.as_os_str().to_owned();
    with_slash.push(std::path::MAIN_SEPARATOR_STR);
    let (zip, _) = compress_ok(&[PathBuf::from(with_slash)], &opts(DEFLATE));
    let entries = read_entries(&zip);
    assert_safe_names(&entries);
    let mut n = names(&entries);
    n.sort();
    assert_eq!(n, vec!["dir/", "dir/in.txt"]);
}

#[test]
fn empty_directory_input() {
    let td = tempfile::tempdir().unwrap();
    let d = td.path().join("nothing");
    fs::create_dir(&d).unwrap();
    let (zip, summary) = compress_ok(&[d], &opts(DEFLATE));
    let entries = read_entries(&zip);
    assert_eq!(names(&entries), vec!["nothing/"]);
    assert!(entries[0].is_dir);
    assert_eq!(summary.files, 0);
    assert_eq!(summary.bytes_in, 0);
}

#[test]
fn duplicate_top_level_names_are_rejected() {
    let td = tempfile::tempdir().unwrap();
    let a = td.path().join("a/same");
    let b = td.path().join("b/same");
    write_file(&a.join("1.txt"), b"1");
    write_file(&b.join("2.txt"), b"2");
    let r = compress_mem(&[a.clone(), b], &opts(DEFLATE));
    assert!(r.is_err(), "two inputs both named `same` must be an error");

    // A file and a directory with the same name collide too.
    let c = td.path().join("c/same");
    write_file(&c, b"file");
    assert!(compress_mem(&[a.clone(), c], &opts(DEFLATE)).is_err());

    // The same input twice is also a duplicate.
    assert!(compress_mem(&[a.clone(), a], &opts(DEFLATE)).is_err());
}

#[test]
fn missing_input_is_not_found() {
    let td = tempfile::tempdir().unwrap();
    let real = td.path().join("real.txt");
    write_file(&real, b"x");
    let missing = td.path().join("does-not-exist");
    let err = compress_mem(&[real, missing.clone()], &opts(DEFLATE)).unwrap_err();
    assert!(
        matches!(err, zipcore::Error::NotFound(ref p) if p == &missing),
        "got {err:?}"
    );
}

#[test]
fn store_method_produces_stored_entries() {
    let td = tempfile::tempdir().unwrap();
    let root = mixed_tree(td.path());
    let (zip, _) = compress_ok(&[root], &opts(Method::Store));
    for e in read_entries(&zip).iter().filter(|e| !e.is_dir) {
        assert_eq!(e.method, CompressionMethod::Stored, "{}", e.name);
        assert_eq!(e.compressed_size, e.size, "{}", e.name);
    }
}

#[test]
fn deflate_compresses_compressible_data() {
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("text.txt");
    write_file(&f, &compressible_bytes(200_000));
    for level in 1..=9u8 {
        let (zip, _) = compress_ok(std::slice::from_ref(&f), &opts(Method::Deflate { level }));
        let e = &read_entries(&zip)[0];
        assert_eq!(e.method, CompressionMethod::Deflated, "level {level}");
        assert!(
            e.compressed_size * 10 < e.size,
            "level {level}: {} -> {} is not much of a compression",
            e.size,
            e.compressed_size
        );
        assert_eq!(e.data, compressible_bytes(200_000));
    }
}

#[test]
fn deflate_level_zero_and_out_of_range_do_not_panic() {
    let td = tempfile::tempdir().unwrap();
    let root = mixed_tree(td.path());
    for level in [0u8, 10, 42, 255] {
        let res = std::panic::catch_unwind(|| {
            compress_mem(
                std::slice::from_ref(&root),
                &opts(Method::Deflate { level }),
            )
        });
        let res = res.unwrap_or_else(|_| panic!("Deflate {{ level: {level} }} panicked"));
        match res {
            Ok((zip, _)) => {
                let entries = read_entries(&zip);
                assert_nodes_eq(&archive_nodes(&entries), &expected_entries(&root));
            }
            Err(e) => eprintln!("NOTE: level {level} returned an error (acceptable): {e}"),
        }
    }
}

#[test]
fn incompressible_data_never_grows() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("rnd");
    write_file(&root.join("big.rnd"), &random_bytes(1 << 20, 99));
    write_file(&root.join("small.rnd"), &random_bytes(100, 3));
    write_file(&root.join("tiny.rnd"), &random_bytes(1, 4));
    write_file(&root.join("empty"), b"");
    write_file(&root.join("text.txt"), &compressible_bytes(5000));
    for level in [1u8, 6, 9] {
        let (zip, _) = compress_ok(
            std::slice::from_ref(&root),
            &opts(Method::Deflate { level }),
        );
        let entries = read_entries(&zip);
        for e in &entries {
            assert!(
                e.compressed_size <= e.size,
                "level {level}: entry {} grew: {} -> {} ({:?})",
                e.name,
                e.size,
                e.compressed_size,
                e.method
            );
        }
        assert_eq!(
            find(&entries, "rnd/text.txt").method,
            CompressionMethod::Deflated
        );
        assert_nodes_eq(&archive_nodes(&entries), &expected_entries(&root));
    }
}

#[test]
fn large_file_streams_and_round_trips() {
    // ~24 MiB, half random and half compressible, so both deflate paths get work.
    let td = tempfile::tempdir().unwrap();
    let f = td.path().join("large.bin");
    let mut data = random_bytes(12 << 20, 1234);
    data.extend(compressible_bytes(12 << 20));
    write_file(&f, &data);

    let mut events = Vec::new();
    let mut progress = |p: &zipcore::Progress| events.push(p.clone());
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let mut cur = std::io::Cursor::new(Vec::new());
    let summary = zipcore::compress(
        &[f],
        &mut cur,
        &opts(Method::Deflate { level: 1 }),
        &mut progress,
        &cancel,
    )
    .expect("compress");
    let zip = cur.into_inner();
    let entries = read_entries(&zip);
    assert_eq!(entries.len(), 1);
    assert!(entries[0].data == data, "large file contents differ");
    assert_eq!(summary.bytes_in, data.len() as u64);

    // "periodically while large files stream": more than one distinct
    // bytes_done value must be reported while this single file is written.
    let mut distinct: Vec<u64> = events
        .iter()
        .filter(|p| p.current == "large.bin")
        .map(|p| p.bytes_done)
        .collect();
    distinct.dedup();
    assert!(
        distinct
            .iter()
            .filter(|&&b| b > 0 && b < data.len() as u64)
            .count()
            >= 1,
        "no intermediate progress while streaming a 24 MiB file: {distinct:?}"
    );
}

#[test]
fn junk_excluded_by_default_and_kept_when_disabled() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("proj");
    let junk = [
        ".DS_Store",
        "sub/.DS_Store",
        "__MACOSX/._x",
        "__MACOSX/inner/y.txt",
        "._foo",
        "sub/._bar.txt",
        "Thumbs.db",
        "sub/desktop.ini",
        "desktop.ini",
    ];
    let keep = [
        "foo",
        "keep.txt",
        "a._b",
        "Thumbs.db.txt",
        "my.DS_Store.txt",
        "_foo",
        "sub/real.txt",
        "MACOSX/z.txt",
    ];
    for p in junk.iter().chain(keep.iter()) {
        write_file(&root.join(p), p.as_bytes());
    }

    let (zip, _) = compress_ok(
        std::slice::from_ref(&root),
        &opts_full(DEFLATE, SymlinkPolicy::Store, true),
    );
    let n = names(&read_entries(&zip));
    for j in junk {
        let full = format!("proj/{j}");
        assert!(!n.contains(&full), "junk {full} was archived: {n:?}");
    }
    assert!(
        !n.iter().any(|x| x.starts_with("proj/__MACOSX")),
        "__MACOSX dir archived: {n:?}"
    );
    for k in keep {
        let full = format!("proj/{k}");
        assert!(n.contains(&full), "non-junk {full} was dropped: {n:?}");
    }

    let (zip, _) = compress_ok(
        std::slice::from_ref(&root),
        &opts_full(DEFLATE, SymlinkPolicy::Store, false),
    );
    let entries = read_entries(&zip);
    assert_nodes_eq(&archive_nodes(&entries), &expected_entries(&root));
}

#[test]
fn unicode_names_round_trip_in_zip_crate() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("ünï");
    for p in [
        "café.txt",
        "日本語/ファイル.txt",
        "emoji 🎉.txt",
        "Ελληνικά/κείμενο.md",
        "한국어.txt",
    ] {
        write_file(&root.join(p), p.as_bytes());
    }
    let (zip, _) = compress_ok(std::slice::from_ref(&root), &opts(DEFLATE));
    let entries = read_entries(&zip);
    assert_safe_names(&entries);
    // Compare against what the filesystem reports (handles any normalization).
    assert_nodes_eq(&archive_nodes(&entries), &expected_entries(&root));
}

/// A streaming unzipper sees exactly the entries listed in the central
/// directory: no hidden local entries, no stale bytes between entries.
#[test]
fn local_headers_match_central_directory() {
    let td = tempfile::tempdir().unwrap();
    let root = mixed_tree(td.path());
    // Small incompressible files: Deflate loses on these, so an implementation
    // that tries deflate first has to back out and store them instead.
    for i in 0..5u64 {
        write_file(
            &root.join(format!("rnd/{i}.bin")),
            &random_bytes(300 + i as usize * 1000, i),
        );
    }
    // Returns a description of the problem instead of panicking, so every
    // writer path gets checked and reported.
    let check = |zip: &[u8], how: String| -> Option<String> {
        let central = names(&read_entries(zip));
        let streamed = std::panic::catch_unwind(|| stream_entries(zip));
        let streamed = match streamed {
            Ok(s) => s,
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .cloned()
                    .unwrap_or_else(|| "panic".into());
                return Some(format!("{how}: {msg}"));
            }
        };
        let s_names: Vec<String> = streamed
            .iter()
            .map(|(n, _)| String::from_utf8_lossy(n).into_owned())
            .collect();
        (s_names != central)
            .then(|| format!("{how}: local headers {s_names:?} != central directory {central:?}"))
    };
    // A lone, just-under-1 MiB incompressible file with a one-letter name: the
    // abandoned deflate attempt overshoots the stored size by more than the
    // tiny central directory, so a rewinding writer leaves slack at the end.
    let lone = td.path().join("z");
    write_file(&lone, &random_bytes(1_000_000, 77));
    let mut problems = Vec::new();
    for method in [
        DEFLATE,
        Method::Deflate { level: 1 },
        Method::Deflate { level: 9 },
    ] {
        for (label, input) in [("tree", &root), ("lone file", &lone)] {
            let (zip, _) = compress_ok(std::slice::from_ref(input), &opts(method));
            problems.extend(check(&zip, format!("compress() {label} {method:?}")));

            let out = td.path().join("streamcheck.zip");
            let cancel = std::sync::atomic::AtomicBool::new(false);
            zipcore::compress_to_path(
                std::slice::from_ref(input),
                &out,
                &opts(method),
                &mut |_| {},
                &cancel,
            )
            .unwrap();
            problems.extend(check(
                &fs::read(&out).unwrap(),
                format!("compress_to_path() {label} {method:?}"),
            ));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
