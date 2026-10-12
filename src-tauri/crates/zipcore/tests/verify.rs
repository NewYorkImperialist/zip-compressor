//! `test_archive`: archives from our writer and from other tools verify
//! cleanly; damage is reported per entry; unreadable files are
//! `InvalidArchive`; hostile inputs never panic, hang or blow up memory.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::*;
use zipcore::{CompressOptions, Error, Method, Preset, Problem, Progress, TestReport};

fn check(path: &Path) -> zipcore::Result<TestReport> {
    let cancel = AtomicBool::new(false);
    zipcore::test_archive(path, &mut |_| {}, &cancel)
}

fn check_bytes(dir: &Path, bytes: &[u8]) -> zipcore::Result<TestReport> {
    let p = dir.join("t.zip");
    fs::write(&p, bytes).unwrap();
    check(&p)
}

fn ok_report(path: &Path) -> TestReport {
    let r = check(path).unwrap_or_else(|e| panic!("test_archive({path:?}) failed: {e:?}"));
    assert!(
        r.is_ok(),
        "{path:?}: unexpected problems: {:#?}",
        r.problems
    );
    r
}

fn problems_for<'a>(r: &'a TestReport, entry: &str) -> Vec<&'a Problem> {
    r.problems
        .iter()
        .filter(|p| p.entry.as_deref() == Some(entry))
        .collect()
}

/// Assert that `entry` has a problem whose message contains one of `needles`.
fn assert_problem(r: &TestReport, entry: &str, needles: &[&str]) {
    let ps = problems_for(r, entry);
    assert!(
        ps.iter()
            .any(|p| needles.iter().any(|n| p.message.contains(n))),
        "expected a problem on {entry:?} mentioning {needles:?}; got {:#?}",
        r.problems
    );
}

fn assert_invalid(r: zipcore::Result<TestReport>, what: &str) {
    match r {
        Err(Error::InvalidArchive(_)) => {}
        other => panic!("{what}: expected InvalidArchive, got {other:?}"),
    }
}

/// Files of several kinds, including one big enough to be chunked (> 4 MiB).
/// Returns the tree root and the total file bytes.
fn sample_tree(parent: &Path, big: bool) -> (PathBuf, u64) {
    let root = parent.join("tree");
    let mut files: Vec<(&str, Vec<u8>)> = vec![
        ("text.txt", compressible_bytes(40_000)),
        ("random.bin", random_bytes(70_000, 7)),
        ("empty.txt", Vec::new()),
        ("sub/deeper/small.txt", b"hello\n".to_vec()),
    ];
    if big {
        // Incompressible, so Zopfli (preset max) skips it and stays fast.
        files.push(("big.bin", random_bytes(9 * MIB + 123, 11)));
    }
    fs::create_dir_all(root.join("emptydir")).unwrap();
    let mut total = 0;
    for (name, data) in &files {
        write_file(&root.join(name), data);
        total += data.len() as u64;
    }
    (root, total)
}

#[test]
fn every_preset_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let (root, total) = sample_tree(dir.path(), true);
    for preset in Preset::ALL {
        let out = dir.path().join(format!("{}.zip", preset.name()));
        compress_path_ok(std::slice::from_ref(&root), &out, &opts(preset.method()));
        let r = ok_report(&out);
        // tree/, emptydir/, sub/, sub/deeper/, five files.
        assert_eq!(r.entries, 9, "{preset:?}");
        assert_eq!(r.bytes_checked, total, "{preset:?}: bytes_checked");
    }
}

#[test]
fn store_and_levels_verify() {
    let dir = tempfile::tempdir().unwrap();
    let (root, total) = sample_tree(dir.path(), false);
    for method in [
        Method::Store,
        Method::Deflate { level: 0 },
        Method::Deflate { level: 9 },
    ] {
        let out = dir.path().join("m.zip");
        let _ = fs::remove_file(&out);
        compress_path_ok(std::slice::from_ref(&root), &out, &opts(method));
        assert_eq!(ok_report(&out).bytes_checked, total, "{method:?}");
    }
}

#[test]
fn progress_and_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let (root, total) = sample_tree(dir.path(), true);
    let out = dir.path().join("p.zip");
    compress_path_ok(&[root], &out, &opts(Method::Deflate { level: 1 }));

    let cancel = AtomicBool::new(false);
    let mut seen: Vec<Progress> = Vec::new();
    let r = zipcore::test_archive(&out, &mut |p| seen.push(p.clone()), &cancel).unwrap();
    assert!(r.is_ok());
    assert!(seen.len() >= 9 + 2, "{} progress calls", seen.len());
    let last = seen.last().unwrap();
    assert_eq!((last.files_done, last.files_total), (9, 9));
    assert_eq!((last.bytes_done, last.bytes_total), (total, total));
    assert!(seen
        .windows(2)
        .all(|w| w[0].bytes_done <= w[1].bytes_done && w[0].files_done <= w[1].files_done));
    // Gaps between reports stay around 4 MiB (+ one 64 KiB buffer).
    let mut prev = 0;
    for p in &seen {
        assert!(p.bytes_done - prev <= 4 * MIB as u64 + 64 * 1024, "gap");
        prev = p.bytes_done;
    }

    // Cancel before starting, and from inside the first progress report.
    cancel.store(true, Ordering::SeqCst);
    assert!(matches!(
        zipcore::test_archive(&out, &mut |_| {}, &cancel),
        Err(Error::Cancelled)
    ));
    let cancel = AtomicBool::new(false);
    let r = zipcore::test_archive(&out, &mut |_| cancel.store(true, Ordering::SeqCst), &cancel);
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
}

// ---------------------------------------------------------------------------
// Hand-built archives.
// ---------------------------------------------------------------------------

fn deflate(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Desc {
    None,
    Sig32,
    NoSig32,
    Sig64,
}

#[derive(Clone)]
struct Raw {
    name: Vec<u8>,
    method: u16,
    flags: u16,
    /// Bytes stored as the entry's data.
    payload: Vec<u8>,
    crc: u32,
    usize: u64,
    desc: Desc,
    /// Central record uses ZIP64 for sizes and offset.
    zip64: bool,
    /// Override the local header offset written in the central directory.
    offset: Option<u64>,
}

impl Raw {
    fn stored(name: &str, data: &[u8]) -> Raw {
        Raw {
            name: name.as_bytes().to_vec(),
            method: 0,
            flags: 0,
            payload: data.to_vec(),
            crc: crc32fast::hash(data),
            usize: data.len() as u64,
            desc: Desc::None,
            zip64: false,
            offset: None,
        }
    }
    fn deflated(name: &str, data: &[u8]) -> Raw {
        Raw {
            method: 8,
            payload: deflate(data),
            ..Raw::stored(name, data)
        }
    }
    fn desc(mut self, d: Desc) -> Raw {
        self.desc = d;
        self.flags |= 8;
        self
    }
}

fn p16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn p32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn p64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// Assemble an archive; `zip64_end` forces a ZIP64 end record + locator.
fn build(entries: &[Raw], zip64_end: bool, comment: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut offsets = Vec::new();
    for e in entries {
        offsets.push(out.len() as u64);
        let csize = e.payload.len() as u64;
        let bit3 = e.desc != Desc::None;
        p32(&mut out, 0x0403_4b50);
        p16(&mut out, 20);
        p16(&mut out, e.flags);
        p16(&mut out, e.method);
        p32(&mut out, 0);
        let (crc, c, u) = if bit3 {
            (0, 0, 0)
        } else {
            (e.crc, csize, e.usize)
        };
        p32(&mut out, crc);
        p32(&mut out, c as u32);
        p32(&mut out, u as u32);
        p16(&mut out, e.name.len() as u16);
        p16(&mut out, 0);
        out.extend_from_slice(&e.name);
        out.extend_from_slice(&e.payload);
        match e.desc {
            Desc::None => {}
            Desc::Sig32 | Desc::NoSig32 => {
                if e.desc == Desc::Sig32 {
                    p32(&mut out, 0x0807_4b50);
                }
                p32(&mut out, e.crc);
                p32(&mut out, csize as u32);
                p32(&mut out, e.usize as u32);
            }
            Desc::Sig64 => {
                p32(&mut out, 0x0807_4b50);
                p32(&mut out, e.crc);
                p64(&mut out, csize);
                p64(&mut out, e.usize);
            }
        }
    }
    let cd_start = out.len() as u64;
    for (e, off) in entries.iter().zip(&offsets) {
        let off = e.offset.unwrap_or(*off);
        let csize = e.payload.len() as u64;
        let mut extra = Vec::new();
        if e.zip64 {
            p16(&mut extra, 1);
            p16(&mut extra, 24);
            p64(&mut extra, e.usize);
            p64(&mut extra, csize);
            p64(&mut extra, off);
        }
        p32(&mut out, 0x0201_4b50);
        p16(&mut out, 0x031e);
        p16(&mut out, if e.zip64 { 45 } else { 20 });
        p16(&mut out, e.flags);
        p16(&mut out, e.method);
        p32(&mut out, 0);
        p32(&mut out, e.crc);
        let sat = |v: u64| if e.zip64 { u32::MAX } else { v as u32 };
        p32(&mut out, sat(csize));
        p32(&mut out, sat(e.usize));
        p16(&mut out, e.name.len() as u16);
        p16(&mut out, extra.len() as u16);
        p16(&mut out, 0);
        p16(&mut out, 0);
        p16(&mut out, 0);
        p32(&mut out, 0);
        p32(&mut out, sat(off));
        out.extend_from_slice(&e.name);
        out.extend_from_slice(&extra);
    }
    let cd_size = out.len() as u64 - cd_start;
    let n = entries.len() as u64;
    if zip64_end {
        let rec = out.len() as u64;
        p32(&mut out, 0x0606_4b50);
        p64(&mut out, 44);
        p16(&mut out, 45);
        p16(&mut out, 45);
        p32(&mut out, 0);
        p32(&mut out, 0);
        p64(&mut out, n);
        p64(&mut out, n);
        p64(&mut out, cd_size);
        p64(&mut out, cd_start);
        p32(&mut out, 0x0706_4b50);
        p32(&mut out, 0);
        p64(&mut out, rec);
        p32(&mut out, 1);
    }
    p32(&mut out, 0x0605_4b50);
    p16(&mut out, 0);
    p16(&mut out, 0);
    let n16 = if zip64_end { 0xFFFF } else { n as u16 };
    p16(&mut out, n16);
    p16(&mut out, n16);
    p32(&mut out, if zip64_end { u32::MAX } else { cd_size as u32 });
    p32(&mut out, if zip64_end { u32::MAX } else { cd_start as u32 });
    p16(&mut out, comment.len() as u16);
    out.extend_from_slice(comment);
    out
}

fn sample_raw() -> Vec<Raw> {
    vec![
        Raw::stored("dir/", b""),
        Raw::stored("dir/stored.txt", &compressible_bytes(3000)),
        Raw::deflated("dir/deflated.txt", &compressible_bytes(50_000)),
        Raw::deflated("desc-sig32.txt", b"descriptor with signature").desc(Desc::Sig32),
        Raw::deflated("desc-nosig.txt", b"descriptor without signature").desc(Desc::NoSig32),
        Raw::stored("desc-sig64.bin", &random_bytes(999, 3)).desc(Desc::Sig64),
        Raw::deflated("empty.txt", b""),
    ]
}

#[test]
fn hand_built_variants_verify() {
    let dir = tempfile::tempdir().unwrap();
    let raw = sample_raw();
    let sizes: u64 = raw.iter().map(|e| e.usize).sum();

    let r = check_bytes(dir.path(), &build(&raw, false, b"")).unwrap();
    assert!(r.is_ok(), "{:#?}", r.problems);
    assert_eq!((r.entries, r.bytes_checked), (7, sizes));

    // ZIP64 everywhere: central extras with all three fields, ZIP64 end records.
    let z: Vec<Raw> = raw
        .iter()
        .cloned()
        .map(|mut e| {
            e.zip64 = true;
            e
        })
        .collect();
    let r = check_bytes(dir.path(), &build(&z, true, b"")).unwrap();
    assert!(r.is_ok(), "zip64: {:#?}", r.problems);
    assert_eq!(r.bytes_checked, sizes);

    // Maximum-length comment that contains a fake end-record signature.
    let mut comment = vec![b'c'; 65_535];
    comment[100..104].copy_from_slice(&0x0605_4b50u32.to_le_bytes());
    comment[60_000..60_004].copy_from_slice(&0x0605_4b50u32.to_le_bytes());
    let r = check_bytes(dir.path(), &build(&raw, false, &comment)).unwrap();
    assert!(r.is_ok(), "comment: {:#?}", r.problems);

    // Empty archive: just an end record.
    let r = check_bytes(dir.path(), &build(&[], false, b"")).unwrap();
    assert!(r.is_ok());
    assert_eq!((r.entries, r.bytes_checked), (0, 0));
}

#[test]
fn prepended_and_appended_data_are_noted() {
    let dir = tempfile::tempdir().unwrap();
    let mut sfx = b"#!/bin/sh\necho self-extractor stub\nexit 0\n".to_vec();
    sfx.extend(build(&sample_raw(), false, b""));
    let r = check_bytes(dir.path(), &sfx).unwrap();
    assert_eq!(r.problems.len(), 1, "{:#?}", r.problems);
    assert_eq!(r.problems[0].entry, None);
    assert!(r.problems[0].message.contains("extra bytes before"));
    assert_eq!(r.bytes_checked, sample_raw().iter().map(|e| e.usize).sum());

    let mut tail = build(&sample_raw(), false, b"");
    tail.extend_from_slice(b"trailing junk");
    let r = check_bytes(dir.path(), &tail).unwrap();
    assert_eq!(r.problems.len(), 1, "{:#?}", r.problems);
    assert!(r.problems[0].message.contains("after the end"));
}

#[test]
fn bad_descriptor_and_header_mismatches() {
    let dir = tempfile::tempdir().unwrap();
    // Descriptor CRC wrong.
    let mut a = build(
        &[Raw::deflated("d.txt", b"data data data").desc(Desc::Sig32)],
        false,
        b"",
    );
    let pos = find(&a, &0x0807_4b50u32.to_le_bytes()).unwrap();
    a[pos + 4] ^= 0xFF;
    let r = check_bytes(dir.path(), &a).unwrap();
    assert_problem(&r, "d.txt", &["data descriptor"]);

    // Local header name differs from the central directory.
    let mut b = build(&[Raw::stored("name.txt", b"x")], false, b"");
    b[30] = b'N';
    let r = check_bytes(dir.path(), &b).unwrap();
    assert_problem(&r, "name.txt", &["local header name"]);

    // Local header size differs (no data descriptor).
    let mut c = build(&[Raw::stored("s.txt", b"abc")], false, b"");
    c[22] = 9;
    let r = check_bytes(dir.path(), &c).unwrap();
    assert_problem(&r, "s.txt", &["local header size"]);

    // Directory with data.
    let r = check_bytes(
        dir.path(),
        &build(&[Raw::stored("d/", b"oops")], false, b""),
    )
    .unwrap();
    assert_problem(&r, "d/", &["directory entry has 4 bytes"]);

    // Stored entry whose sizes disagree.
    let mut e = Raw::stored("st", b"12345");
    e.usize = 4;
    let r = check_bytes(dir.path(), &build(&[e], false, b"")).unwrap();
    assert_problem(&r, "st", &["size mismatch"]);
}

#[test]
fn unverifiable_entries_are_problems() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = Vec::new();
    for m in [9u16, 12, 14, 93, 95, 99] {
        let mut e = Raw::stored(&format!("m{m}"), b"opaque payload");
        e.method = m;
        entries.push(e);
    }
    let mut enc = Raw::stored("enc", b"ciphertext!!");
    enc.flags = 1;
    entries.push(enc);
    entries.push(Raw::stored("plain", b"fine"));
    let r = check_bytes(dir.path(), &build(&entries, false, b"")).unwrap();
    for m in [9u16, 12, 14, 93, 95] {
        assert_problem(&r, &format!("m{m}"), &["can't verify"]);
    }
    assert_problem(&r, "m99", &["AES-encrypted"]);
    assert_problem(&r, "enc", &["encrypted"]);
    assert!(problems_for(&r, "plain").is_empty());
    assert_eq!(r.problems.len(), 7);
    assert_eq!(r.bytes_checked, 4, "only the plain entry is decompressed");
}

// ---------------------------------------------------------------------------
// Corruption.
// ---------------------------------------------------------------------------

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Data offset of `name` in one of our archives (local header has no extra
/// beyond what the header says).
fn data_offset(bytes: &[u8], name: &str) -> usize {
    let mut i = 0;
    loop {
        let h = &bytes[i..];
        assert_eq!(&h[..4], b"PK\x03\x04", "entry {name} not found");
        let n = u16::from_le_bytes([h[26], h[27]]) as usize;
        let x = u16::from_le_bytes([h[28], h[29]]) as usize;
        let c = u32::from_le_bytes(h[18..22].try_into().unwrap()) as usize;
        if &h[30..30 + n] == name.as_bytes() {
            return i + 30 + n + x;
        }
        i += 30 + n + x + c;
    }
}

#[test]
fn corruption_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (root, _) = sample_tree(dir.path(), false);
    let src = dir.path().join("src.zip");
    let (good, _) = compress_path_ok(
        std::slice::from_ref(&root),
        &src,
        &opts(Method::Deflate { level: 6 }),
    );

    // Flip a byte in the middle of deflate data.
    let mut b = good.clone();
    let off = data_offset(&b, "tree/text.txt");
    b[off + 100] ^= 0x55;
    let r = check_bytes(dir.path(), &b).unwrap();
    assert_problem(
        &r,
        "tree/text.txt",
        &[
            "bad CRC",
            "invalid compressed data",
            "size mismatch",
            "ends before",
            "more than",
        ],
    );
    assert!(r
        .problems
        .iter()
        .all(|p| p.entry.as_deref() == Some("tree/text.txt")));

    // Flip a byte in a stored entry.
    let stored_zip = dir.path().join("stored.zip");
    let (good_s, _) = compress_path_ok(&[root], &stored_zip, &opts(Method::Store));
    let mut s = good_s.clone();
    let off = data_offset(&s, "tree/random.bin");
    s[off + 10] ^= 1;
    let r = check_bytes(dir.path(), &s).unwrap();
    assert_problem(&r, "tree/random.bin", &["bad CRC"]);
    assert_eq!(r.problems.len(), 1);

    // Change the CRC in the central directory.
    let mut c = good.clone();
    let cd = find(&c, b"PK\x01\x02").unwrap();
    let mut i = cd;
    loop {
        let n = u16::from_le_bytes([c[i + 28], c[i + 29]]) as usize;
        let x = u16::from_le_bytes([c[i + 30], c[i + 31]]) as usize;
        let k = u16::from_le_bytes([c[i + 32], c[i + 33]]) as usize;
        if &c[i + 46..i + 46 + n] == b"tree/sub/deeper/small.txt" {
            c[i + 16] ^= 0xFF;
            break;
        }
        i += 46 + n + x + k;
    }
    let r = check_bytes(dir.path(), &c).unwrap();
    assert_problem(&r, "tree/sub/deeper/small.txt", &["bad CRC"]);
    assert_problem(&r, "tree/sub/deeper/small.txt", &["local header CRC"]);

    // Truncation anywhere loses the end record.
    for cut in [1usize, 10, 21, 22, 100, good.len() / 2, good.len() - 30] {
        let r = check_bytes(dir.path(), &good[..good.len() - cut]);
        assert_invalid(r, &format!("truncated by {cut}"));
    }
    assert_invalid(
        check_bytes(dir.path(), &random_bytes(100_000, 5)),
        "garbage",
    );
    assert_invalid(check_bytes(dir.path(), b""), "empty");
    assert_invalid(check_bytes(dir.path(), b"PK\x05\x06"), "tiny");
    assert_invalid(
        check_bytes(dir.path(), &good[..good.len() / 2]),
        "first half",
    );
    assert!(matches!(
        check(&dir.path().join("missing.zip")),
        Err(Error::Io { .. })
    ));
}

#[test]
fn structural_damage() {
    let dir = tempfile::tempdir().unwrap();
    let good = build(&sample_raw(), false, b"");
    let eocd = good.len() - 22;

    // More entries than the central directory can hold.
    let mut a = good.clone();
    a[eocd + 8..eocd + 12].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    assert_invalid(check_bytes(dir.path(), &a), "entry count");

    // Multi-disk.
    let mut b = good.clone();
    b[eocd + 4] = 1;
    b[eocd + 6] = 1;
    match check_bytes(dir.path(), &b) {
        Err(Error::InvalidArchive(m)) => assert!(m.contains("split archives"), "{m}"),
        r => panic!("split: {r:?}"),
    }

    // Central directory offset beyond the file.
    let mut c = good.clone();
    c[eocd + 16..eocd + 20].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    assert_invalid(check_bytes(dir.path(), &c), "cd offset");

    // An entry whose local header offset points outside the file.
    let mut raw = sample_raw();
    raw[2].offset = Some(0x7000_0000);
    let r = check_bytes(dir.path(), &build(&raw, false, b"")).unwrap();
    assert_problem(&r, "dir/deflated.txt", &["outside"]);

    // An entry whose compressed size runs past the file.
    // The second central record ("dir/" before it has no extra field).
    let mut d = good.clone();
    let second = find(&d, b"PK\x01\x02").unwrap() + 46 + 4;
    d[second + 20..second + 24].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
    let r = check_bytes(dir.path(), &d).unwrap();
    assert_problem(&r, "dir/stored.txt", &["extends past the end of the file"]);

    // Damaged central directory record signature.
    let mut e = good.clone();
    e[second] = b'X';
    assert_invalid(check_bytes(dir.path(), &e), "cd signature");
}

#[test]
fn zip_bomb_is_stopped() {
    let dir = tempfile::tempdir().unwrap();
    // 64 MiB of zeros deflates to ~64 KB; claim it is 1000 bytes.
    let zeros = vec![0u8; 64 * MIB];
    let mut bomb = Raw::deflated("bomb.bin", &zeros);
    bomb.usize = 1000;
    let path = dir.path().join("bomb.zip");
    fs::write(&path, build(&[bomb], false, b"")).unwrap();
    let started = Instant::now();
    let r = with_timeout(60, "bomb", move || check(&path).unwrap());
    assert_problem(&r, "bomb.bin", &["more than the 1000 bytes"]);
    assert!(r.bytes_checked <= 1000 + 64 * 1024, "{}", r.bytes_checked);
    assert!(started.elapsed() < Duration::from_secs(30));

    // Overlapping entries (one local header shared by many central records):
    // each later one is reported and not decompressed.
    let one = Raw::deflated("a", &vec![b'x'; 2 * MIB]);
    let mut entries = vec![one.clone()];
    for i in 0..50 {
        let mut e = one.clone();
        e.name = format!("a{i}").into_bytes();
        e.offset = Some(0);
        entries.push(e);
    }
    let bytes = build(&entries, false, b"");
    let r = check_bytes(dir.path(), &bytes).unwrap();
    assert_eq!(
        r.bytes_checked,
        2 * MIB as u64,
        "only the first is inflated"
    );
    assert_problem(&r, "a7", &["overlaps entry \"a\""]);
    assert!(problems_for(&r, "a").is_empty());
}

// ---------------------------------------------------------------------------
// Fuzz-style robustness.
// ---------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn mutate(base: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut b = base.to_vec();
    let len = b.len();
    // Bias towards the end, where the directory and end records are.
    let pos = |rng: &mut Rng| {
        if rng.below(2) == 0 {
            len - 1 - rng.below(len.min(300))
        } else {
            rng.below(len)
        }
    };
    match rng.below(6) {
        0 => {
            for _ in 0..1 + rng.below(4) {
                let p = pos(rng);
                b[p] ^= 1 << rng.below(8);
            }
        }
        1 => {
            let p = pos(rng);
            b[p] = rng.next() as u8;
        }
        2 => b.truncate(rng.below(len)),
        3 => {
            // Plant 0xFF runs (saturated sizes, ZIP64 markers).
            let p = pos(rng);
            let n = (1 + rng.below(8)).min(len - p);
            b[p..p + n].fill(0xFF);
        }
        4 => {
            // Random 32-bit value somewhere (a size or offset).
            let p = pos(rng).min(len.saturating_sub(4));
            let v = match rng.below(3) {
                0 => rng.next() as u32,
                1 => rng.below(len) as u32,
                _ => 0,
            };
            b[p..p + 4].copy_from_slice(&v.to_le_bytes());
        }
        _ => {
            let a = rng.below(len);
            let n = rng.below(64).min(len - a);
            b.drain(a..a + n);
        }
    }
    b
}

#[test]
fn fuzzed_archives_never_panic_or_hang() {
    let dir = tempfile::tempdir().unwrap();
    let (root, _) = sample_tree(dir.path(), false);
    let ours = dir.path().join("ours.zip");
    let (ours, _) = compress_path_ok(&[root], &ours, &opts(Method::Deflate { level: 6 }));
    let mut z64: Vec<Raw> = sample_raw();
    for e in &mut z64 {
        e.zip64 = true;
    }
    let bases = [
        build(&sample_raw(), false, b"comment"),
        build(&z64, true, b""),
        ours,
    ];
    let work = dir.path().join("fuzz.zip");
    let (mut oks, mut errs) = (0u32, 0u32);
    let counts = with_timeout(300, "fuzz", move || {
        let mut rng = Rng(0x5EED_1234_ABCD_0001);
        for i in 0..4000 {
            let base = &bases[i % bases.len()];
            let mut b = mutate(base, &mut rng);
            if rng.below(4) == 0 {
                b = mutate(&b, &mut rng);
            }
            fs::write(&work, &b).unwrap();
            match check(&work) {
                Ok(_) => oks += 1,
                Err(Error::InvalidArchive(_)) => errs += 1,
                Err(e) => panic!("iteration {i}: unexpected error {e:?}"),
            }
        }
        (oks, errs)
    });
    eprintln!("fuzz: {} reports, {} invalid", counts.0, counts.1);
    assert!(counts.0 > 0 && counts.1 > 0);
}

// ---------------------------------------------------------------------------
// Archives made by other tools.
// ---------------------------------------------------------------------------

/// A small tree for external tools: `src/{a.txt, b.bin, sub/c.txt}`.
fn tool_tree(parent: &Path) -> PathBuf {
    let src = parent.join("src");
    write_file(&src.join("a.txt"), &compressible_bytes(300_000));
    write_file(&src.join("b.bin"), &random_bytes(50_000, 9));
    write_file(&src.join("sub/c.txt"), b"tiny\n");
    src
}

const TOOL_TREE_BYTES: u64 = 300_000 + 50_000 + 5;

/// Run `cmd`, writing its stdout through a pipe (so it is unseekable) to `out`.
fn run_piped_to(cmd: &mut Command, out: &Path) {
    let o = run_ok(cmd.stdout(Stdio::piped()));
    fs::write(out, o.stdout).unwrap();
}

#[test]
fn info_zip_archives_verify() {
    let Some(zip) = tool(&["zip"], "info_zip_archives_verify") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = tool_tree(dir.path());

    let plain = dir.path().join("plain.zip");
    run_ok(
        Command::new(&zip)
            .current_dir(&src)
            .arg("-qr")
            .arg(&plain)
            .arg("."),
    );
    let r = ok_report(&plain);
    assert_eq!(r.bytes_checked, TOOL_TREE_BYTES);

    // Streaming to a pipe: data descriptors.
    let streamed = dir.path().join("streamed.zip");
    run_piped_to(
        Command::new(&zip).current_dir(&src).args(["-qr", "-", "."]),
        &streamed,
    );
    let bytes = fs::read(&streamed).unwrap();
    assert!(
        u16::from_le_bytes([bytes[6], bytes[7]]) & 8 != 0,
        "expected a data descriptor"
    );
    assert_eq!(ok_report(&streamed).bytes_checked, TOOL_TREE_BYTES);

    // Stored only.
    let stored = dir.path().join("stored.zip");
    run_ok(
        Command::new(&zip)
            .current_dir(&src)
            .arg("-qr0")
            .arg(&stored)
            .arg("."),
    );
    assert_eq!(ok_report(&stored).bytes_checked, TOOL_TREE_BYTES);

    // Encrypted (traditional PKWARE).
    let enc = dir.path().join("enc.zip");
    run_ok(
        Command::new(&zip)
            .current_dir(&src)
            .args(["-q", "-P", "secret"])
            .arg(&enc)
            .arg("a.txt"),
    );
    let r = check(&enc).unwrap();
    assert_problem(&r, "a.txt", &["encrypted"]);
}

#[test]
fn python_zipfile_archives_verify() {
    let Some(py) = python("python_zipfile_archives_verify") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("py.zip");
    let script = r#"
import zipfile
with zipfile.ZipFile(sys.argv[1], 'w') as z:
    z.writestr('stored.txt', 'x' * 5000, zipfile.ZIP_STORED)
    z.writestr('deflated.txt', 'y' * 50000, zipfile.ZIP_DEFLATED)
    z.writestr('dir/', '')
    with z.open('streamed.bin', 'w') as f:
        f.write(bytes(range(256)) * 300)
    z.comment = b'an archive comment'
"#;
    run_python(&py, script, &[&out]);
    let r = ok_report(&out);
    assert_eq!(r.bytes_checked, 5000 + 50000 + 256 * 300);

    // Written to an unseekable stream: data descriptors.
    let piped = dir.path().join("piped.zip");
    let script = r#"
import sys, zipfile
z = zipfile.ZipFile(sys.stdout.buffer, 'w')
z.writestr('a.txt', 'q' * 5000, zipfile.ZIP_DEFLATED)
z.writestr('s.txt', 'stored', zipfile.ZIP_STORED)
with z.open('b.bin', 'w', force_zip64=True) as f:
    f.write(b'r' * 70000)
z.close()
"#;
    run_piped_to(Command::new(&py).arg("-I").arg("-c").arg(script), &piped);
    assert_eq!(ok_report(&piped).bytes_checked, 5000 + 6 + 70000);

    // LZMA and BZIP2 entries can't be verified.
    let other = dir.path().join("other.zip");
    let script = r#"
import zipfile
with zipfile.ZipFile(sys.argv[1], 'w') as z:
    z.writestr('l.txt', 'z' * 1000, zipfile.ZIP_LZMA)
    z.writestr('b.txt', 'z' * 1000, zipfile.ZIP_BZIP2)
    z.writestr('ok.txt', 'z' * 1000, zipfile.ZIP_DEFLATED)
"#;
    run_python(&py, script, &[&other]);
    let r = check(&other).unwrap();
    assert_problem(&r, "l.txt", &["LZMA", "can't verify"]);
    assert_problem(&r, "b.txt", &["BZIP2", "can't verify"]);
    assert!(problems_for(&r, "ok.txt").is_empty());
    assert_eq!(r.bytes_checked, 1000);
}

#[cfg(target_os = "macos")]
#[test]
fn ditto_archives_verify() {
    let Some(ditto) = tool(&["ditto"], "ditto_archives_verify") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = tool_tree(dir.path());
    let out = dir.path().join("ditto.zip");
    run_ok(Command::new(ditto).args(["-c", "-k"]).arg(&src).arg(&out));
    assert_eq!(ok_report(&out).bytes_checked, TOOL_TREE_BYTES);
}

#[test]
fn jar_archives_verify() {
    let Some(jar) = tool(&["jar"], "jar_archives_verify") else {
        return;
    };
    // macOS ships a /usr/bin/jar stub that fails without a JDK.
    if !Command::new(&jar)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("NOTE: skipping jar_archives_verify: jar does not run");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = tool_tree(dir.path());
    let out = dir.path().join("t.jar");
    run_ok(
        Command::new(jar)
            .arg("cf")
            .arg(&out)
            .arg("-C")
            .arg(&src)
            .arg("."),
    );
    let r = ok_report(&out);
    assert!(r.bytes_checked >= TOOL_TREE_BYTES, "plus the manifest");
}

// ---------------------------------------------------------------------------
// ZIP64 (slow; run with --ignored).
// ---------------------------------------------------------------------------

/// A 4.5 GiB entry of zeros (sparse input, deflated so the archive stays
/// small) needs ZIP64 sizes in both headers.
#[test]
#[ignore]
fn zip64_big_entry_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("z");
    fs::create_dir(&root).unwrap();
    let size = 4 * 1024 * MIB as u64 + 512 * MIB as u64;
    fs::File::create(root.join("big.bin"))
        .unwrap()
        .set_len(size)
        .unwrap();
    write_file(&root.join("after.txt"), b"after the big one\n");
    let out = dir.path().join("big.zip");
    let opts = CompressOptions {
        method: Method::Deflate { level: 1 },
        ..CompressOptions::default()
    };
    let cancel = AtomicBool::new(false);
    zipcore::compress_to_path(&[root], &out, &opts, &mut |_| {}, &cancel).unwrap();
    let started = Instant::now();
    let mut reports = 0u32;
    let r = zipcore::test_archive(&out, &mut |_| reports += 1, &cancel).unwrap();
    eprintln!(
        "verified {} bytes in {:?} ({} progress reports, archive {} bytes)",
        r.bytes_checked,
        started.elapsed(),
        reports,
        fs::metadata(&out).unwrap().len()
    );
    assert!(r.is_ok(), "{:#?}", r.problems);
    assert_eq!(r.bytes_checked, size + 18);
    assert!(reports as u64 >= size / (4 * MIB as u64));
}
