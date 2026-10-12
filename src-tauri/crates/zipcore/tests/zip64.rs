//! ZIP64: a file larger than 4 GiB, an entry offset beyond 4 GiB, and more
//! than 65,535 entries.
//!
//! Ignored by default because they are slow. Run them with:
//!
//! ```sh
//! cd src-tauri
//! cargo test -p zipcore --release --test zip64 -- --ignored --nocapture
//! ```
//!
//! The big input is sparse (created with `File::set_len`), so it needs little
//! disk for the input. The deflate test's output is a few MB of deflated
//! zeros; the store test writes a ~4.1 GiB archive.

mod common;

use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::Path;
use std::sync::atomic::AtomicBool;

use common::*;
use zipcore::{Method, Progress};

const SMALL: &[u8] = b"small file after the big one\n";

/// `dir/big.bin` (sparse, `size` bytes) followed in archive order by `dir/small.txt`.
fn big_then_small(parent: &Path, size: u64) -> std::path::PathBuf {
    let dir = parent.join("z64");
    fs::create_dir(&dir).unwrap();
    File::create(dir.join("big.bin"))
        .unwrap()
        .set_len(size)
        .unwrap();
    fs::write(dir.join("small.txt"), SMALL).unwrap();
    dir
}

fn check_big_archive(out: &Path, size: u64, expect_small_offset_over_4g: bool) {
    let mut ar = zip::ZipArchive::new(BufReader::new(File::open(out).unwrap())).unwrap();
    assert_eq!(ar.len(), 3, "z64/, z64/big.bin, z64/small.txt");
    {
        let mut f = ar.by_name("z64/big.bin").unwrap();
        assert_eq!(f.size(), size, "uncompressed size in central directory");
        assert_eq!(f.crc32(), crc32_of_zeros(size), "recorded CRC-32");
        // Reading to EOF makes the zip crate verify the CRC against the data.
        let n = io::copy(&mut f, &mut io::sink()).expect("read back (CRC check)");
        assert_eq!(n, size);
    }
    {
        let mut f = ar.by_name("z64/small.txt").unwrap();
        let mut d = Vec::new();
        f.read_to_end(&mut d).unwrap();
        assert_eq!(d, SMALL);
    }
    drop(ar);

    let raw = parse_raw(&mut BufReader::new(File::open(out).unwrap()));
    let probs = layout_problems(&raw);
    assert!(
        probs.is_empty(),
        "layout problems:\n  {}",
        probs.join("\n  ")
    );
    let (big_cd, big_local) = &raw
        .entries
        .iter()
        .find(|(c, _)| c.name == b"z64/big.bin")
        .unwrap();
    assert_eq!(big_cd.usize, size);
    assert_eq!(big_local.usize, size, "local header size (ZIP64 extra)");
    assert!(
        extra_field(&big_local.extra, 0x0001).is_some(),
        "local header of a > 4 GiB entry has no ZIP64 extra"
    );
    assert!(
        big_cd.version_needed >= 45,
        "version needed {} < 4.5",
        big_cd.version_needed
    );
    let (small_cd, _) = raw
        .entries
        .iter()
        .find(|(c, _)| c.name == b"z64/small.txt")
        .unwrap();
    if expect_small_offset_over_4g {
        assert!(
            small_cd.offset > u32::MAX as u64,
            "offset {}",
            small_cd.offset
        );
        assert!(
            extra_field(&small_cd.extra, 0x0001).is_some(),
            "entry at offset > 4 GiB has no ZIP64 extra in the CD"
        );
    }

    if let Some(unzip) = tool(&["unzip"], "zip64 unzip -t") {
        run_ok(std::process::Command::new(unzip).arg("-tq").arg(out));
    }
    if let Some(py) = python("zip64 python") {
        let o = run_python(
            &py,
            "import zipfile\nz = zipfile.ZipFile(sys.argv[1])\ni = z.getinfo('z64/small.txt')\nprint(len(z.infolist()), i.header_offset, z.read(i))\n",
            &[out],
        );
        assert!(o.starts_with("3 "), "python: {o}");
        assert!(o.contains("small file after the big one"), "python: {o}");
    }
}

#[test]
#[ignore = "slow: streams a sparse 4.5 GiB file; run with --release -- --ignored"]
fn zip64_large_sparse_file_round_trips() {
    const SIZE: u64 = 4 * 1024 * 1024 * 1024 + 512 * 1024 * 1024;
    let td = tempfile::tempdir().unwrap();
    let big = td.path().join("big.bin");
    File::create(&big).unwrap().set_len(SIZE).unwrap();
    let out = td.path().join("big.zip");
    let cancel = AtomicBool::new(false);
    let mut cb = |_: &Progress| {};
    let summary = zipcore::compress_to_path(
        std::slice::from_ref(&big),
        &out,
        &opts(Method::Deflate { level: 1 }),
        &mut cb,
        &cancel,
    )
    .expect("compress 4.5 GiB");
    assert_eq!(summary.bytes_in, SIZE);
    assert_eq!(summary.bytes_out, fs::metadata(&out).unwrap().len());

    let mut ar = zip::ZipArchive::new(BufReader::new(File::open(&out).unwrap())).unwrap();
    assert_eq!(ar.len(), 1);
    let mut f = ar.by_index(0).unwrap();
    assert_eq!(f.name().unwrap(), "big.bin");
    assert_eq!(f.size(), SIZE, "uncompressed size in central directory");
    assert_eq!(f.crc32(), crc32_of_zeros(SIZE), "recorded CRC-32");
    let n = io::copy(&mut f, &mut io::sink()).expect("read back (CRC check)");
    assert_eq!(n, SIZE);
    drop(f);

    if let Some(unzip) = tool(&["unzip"], "zip64 unzip -t") {
        run_ok(std::process::Command::new(unzip).arg("-tq").arg(&out));
    }
}

/// Deflated > 4 GiB entry with 1 and 8 threads (chunked path), followed by a
/// small entry; both thread counts must give identical bytes.
#[test]
#[ignore = "slow: deflates a sparse 4.5 GiB file twice; run with --release -- --ignored"]
fn zip64_deflated_big_then_small_threads_identical() {
    const SIZE: u64 = 4 * 1024 * 1024 * 1024 + 512 * 1024 * 1024;
    let td = tempfile::tempdir().unwrap();
    let dir = big_then_small(td.path(), SIZE);
    let mut outs = Vec::new();
    for threads in [8usize, 1] {
        let out = td.path().join(format!("t{threads}.zip"));
        let cancel = AtomicBool::new(false);
        let summary = zipcore::compress_to_path(
            std::slice::from_ref(&dir),
            &out,
            &opts_t(Method::Deflate { level: 1 }, threads),
            &mut |_| {},
            &cancel,
        )
        .expect("compress 4.5 GiB");
        assert_eq!(summary.bytes_in, SIZE + SMALL.len() as u64);
        assert_eq!(summary.bytes_out, fs::metadata(&out).unwrap().len());
        check_big_archive(&out, SIZE, false);
        outs.push(fs::read(&out).unwrap());
    }
    assert!(
        outs[0] == outs[1],
        "threads=8 and threads=1 archives differ"
    );
}

/// Stored > 4 GiB entry, so the following entry's local header offset is
/// beyond 4 GiB and needs the ZIP64 offset field. Writes ~4.1 GiB to disk.
#[test]
#[ignore = "slow and needs ~4.2 GiB free disk; run with --release -- --ignored"]
fn zip64_offset_beyond_4gib_with_8_threads() {
    const SIZE: u64 = 4 * 1024 * 1024 * 1024 + 64 * 1024 * 1024;
    let td = tempfile::tempdir().unwrap();
    let dir = big_then_small(td.path(), SIZE);
    let out = td.path().join("stored.zip");
    let cancel = AtomicBool::new(false);
    let summary = zipcore::compress_to_path(
        std::slice::from_ref(&dir),
        &out,
        &opts_t(Method::Store, 8),
        &mut |_| {},
        &cancel,
    )
    .expect("store 4.1 GiB");
    assert_eq!(summary.bytes_out, fs::metadata(&out).unwrap().len());
    check_big_archive(&out, SIZE, true);
}

/// More than 65,535 entries needs the ZIP64 end of central directory.
#[test]
#[ignore = "slow: creates 70,000 files; run with --release -- --ignored"]
fn zip64_more_than_65535_entries() {
    const N: usize = 70_000;
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("lots");
    for d in 0..70 {
        fs::create_dir_all(root.join(format!("d{d:02}"))).unwrap();
    }
    for i in 0..N {
        File::create(root.join(format!("d{:02}/e{i:05}", i % 70))).unwrap();
    }
    let total = N + 70 + 1;
    let out = td.path().join("lots.zip");
    let cancel = AtomicBool::new(false);
    let summary = zipcore::compress_to_path(
        std::slice::from_ref(&root),
        &out,
        &opts_t(Method::Deflate { level: 6 }, 8),
        &mut |_| {},
        &cancel,
    )
    .expect("compress 70k files");
    assert_eq!(summary.files, N as u64);
    let bytes = fs::read(&out).unwrap();
    let raw = parse_raw_bytes(&bytes);
    assert!(raw.has_zip64_eocd, "no ZIP64 EOCD for {total} entries");
    assert_eq!(raw.total_entries, total as u64);
    assert_eq!(
        raw.eocd_entries16, 0xFFFF,
        "classic EOCD count must be the 0xFFFF sentinel"
    );
    let probs = layout_problems(&raw);
    assert!(
        probs.is_empty(),
        "layout problems:\n  {}",
        probs[..probs.len().min(20)].join("\n  ")
    );

    let ar = zip::ZipArchive::new(std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(ar.len(), total, "zip crate entry count");
    if let Some(py) = python("zip64_more_than_65535_entries") {
        let o = run_python(
            &py,
            "import zipfile\nz = zipfile.ZipFile(sys.argv[1])\nprint(len(z.infolist()), z.testzip())\n",
            &[&out],
        );
        assert_eq!(o.trim(), format!("{total} None"));
    }
    if let Some(unzip) = tool(&["unzip"], "zip64 70k unzip -t") {
        run_ok(std::process::Command::new(unzip).arg("-tq").arg(&out));
    }
}

/// Independent table-driven CRC-32 (IEEE) over `n` zero bytes.
fn crc32_of_zeros(n: u64) -> u32 {
    let mut table = [0u32; 256];
    for (i, t) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *t = c;
    }
    let mut crc = !0u32;
    for _ in 0..n {
        crc = table[(crc & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}
