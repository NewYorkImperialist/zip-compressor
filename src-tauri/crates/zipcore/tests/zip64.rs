//! ZIP64: a file larger than 4 GiB.
//!
//! Ignored by default because it streams 4.5 GiB through deflate. Run it with:
//!
//! ```sh
//! cd src-tauri
//! cargo test -p zipcore --release --test zip64 -- --ignored --nocapture
//! ```
//!
//! The input is sparse (created with `File::set_len`), so it needs little disk
//! for the input; the output is a few MB of deflated zeros (or ~4.5 GiB with
//! `Method::Store`, which this test does not use).

mod common;

use std::fs::{self, File};
use std::io::{self, BufReader};
use std::sync::atomic::AtomicBool;

use common::*;
use zipcore::{Method, Progress};

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
    // Reading to EOF makes the zip crate verify the CRC against the data.
    let n = io::copy(&mut f, &mut io::sink()).expect("read back (CRC check)");
    assert_eq!(n, SIZE);
    drop(f);

    if let Some(unzip) = tool(&["unzip"], "zip64 unzip -t") {
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
