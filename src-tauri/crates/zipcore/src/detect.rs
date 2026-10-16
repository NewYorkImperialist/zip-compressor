//! Detection of data that deflate can't usefully shrink, for
//! [`crate::CompressOptions::store_incompressible`].
//!
//! # The estimate
//! A region of `len` bytes is judged from [`SAMPLE`]-byte samples, each
//! deflated on its own at level [`LEVEL`]: its first and last [`SAMPLE`]
//! bytes (headers and trailers, such as a ZIP's central directory or a
//! video's index, are often the only compressible part of a file), and `k`
//! samples from the middle, one centered in each of `k` equal strata. The
//! head and tail samples stand for themselves; each middle sample stands for
//! its stratum, so the estimated saving of deflating the whole region is
//!
//! ```text
//! saving(head) + saving(tail) + sum(saving(sample_i) * stratum_i / SAMPLE)
//! ```
//!
//! and the region looks incompressible when that is under [`SAVING_PPM`] of
//! `len`. Samples are taken in order (head, tail, middle) and the check stops
//! as soon as the samples taken already prove the saving is large enough
//! (each remaining one can lower it by at most [`SAMPLE_OVERHEAD`] bytes per
//! [`SAMPLE`] it stands for), so on compressible data it usually costs one
//! level-2 pass over 32 KiB.
//!
//! Level 2 (zlib-rs `deflate_fast`) codes with dynamic Huffman tables like
//! levels 6-9 and lands within ~0.05% of level 6 on media, archives and
//! photos; level 1 (`deflate_quick`) only has the fixed table, so it sees no
//! gain at all in data such as JAR files that level 6 shrinks by 10%.
//! Order-0 entropy alone misses the same JAR gains (they come from matches),
//! so it is not used.
//!
//! # Where it is used
//! - A file under [`crate::codec::BIG_FILE`] of at least [`MIN_UNIT`] bytes
//!   ([`unit_incompressible`]): Stored if it looks incompressible. Smaller
//!   files are deflated as usual (cheap anyway) and kept only if smaller.
//! - A big file: Stored whole if its first chunk and its last [`CHUNK`]
//!   bytes ([`unit_incompressible`] each) and the part in between (one
//!   sample per [`CHUNK`], 4 to [`MAX_SPREAD`] of them) all look
//!   incompressible ([`rest_incompressible`]). Otherwise it is deflated, and
//!   each chunk that looks incompressible on its own is written as stored
//!   blocks without a deflate attempt.
//!
//! Every decision depends only on the bytes of the file (and its size), never
//! on the method, level or thread count, so archives stay deterministic and
//! all methods store exactly the same data.
//!
//! # Measured
//! Estimated against level-6 sizes, unit by unit, on real data (2026-10-16:
//! macOS wallpapers, system videos and PNGs, Minecraft library JARs, two app
//! bundles, Rust sources, Silesia), what this stores would have deflated by
//! 0.005% (JARs) to 0.045% (app bundles, whose asset catalogs hide
//! compressible images between samples) of each set's level-6 size; text,
//! source and Silesia are never stored. With 32 KiB samples (8 per MiB)
//! that was 3-5x more accurate than 16 KiB (16 per MiB) or 8 KiB samples.

use std::io;

use crate::codec::CHUNK;
use crate::zlib::{self, Strategy};

/// Bytes per sample.
pub(crate) const SAMPLE: usize = 32 << 10;
/// Smallest unit that is checked: below this, deflating it whole is cheap
/// and the samples would be a large part of it anyway.
pub(crate) const MIN_UNIT: u64 = 16 * SAMPLE as u64;
/// Most middle samples for the part of a big file between its first chunk
/// and its last [`CHUNK`] bytes.
pub(crate) const MAX_SPREAD: usize = 64;
/// Deflate level of the trial.
pub(crate) const LEVEL: u32 = 2;
/// Regions whose estimated saving is below this many parts per million of
/// their size look incompressible (2500 = 0.25%).
pub(crate) const SAVING_PPM: u64 = 2500;
/// Bound on what one sample's deflate adds to its size (block framing), for
/// the early stop; deflate falls back to stored blocks, 5 bytes per 64 KiB.
const SAMPLE_OVERHEAD: i128 = 64;

/// Middle samples for a unit of `len` bytes: samples cover about a quarter
/// of it (head and tail included), 4 to 10 samples in all. A chunk gets 6.
pub(crate) fn unit_middle(len: u64) -> usize {
    len.div_ceil(4 * SAMPLE as u64).clamp(4, 10) as usize - 2
}

/// Whether the `len` bytes that `read` serves look incompressible, judged
/// from the head and tail samples plus `middle` samples in between.
/// `read(offset, buf)` fills `buf` with the bytes at `offset`.
///
/// `len` must be at least `(middle + 2) * SAMPLE`.
fn looks_incompressible(
    len: u64,
    middle: usize,
    read: &mut dyn FnMut(u64, &mut [u8]) -> io::Result<()>,
) -> io::Result<bool> {
    let s = SAMPLE as u64;
    debug_assert!(len >= (middle as u64 + 2) * s);
    // (offset, bytes it stands for).
    let mut plan = Vec::with_capacity(middle + 2);
    plan.push((0, s));
    plan.push((len - s, s));
    let mid = len - 2 * s;
    let k = middle as u64;
    for i in 0..k {
        let start = s + mid * i / k;
        let end = s + mid * (i + 1) / k;
        plan.push((start + (end - start - s) / 2, end - start));
    }
    // Savings in bytes x SAMPLE, so every term stays an integer.
    let goal = i128::from(len) * i128::from(SAVING_PPM) * SAMPLE as i128 / 1_000_000;
    let mut remaining: i128 = plan.iter().map(|&(_, w)| i128::from(w)).sum();
    let mut saved: i128 = 0;
    let mut buf = vec![0u8; SAMPLE];
    for (offset, weight) in plan {
        read(offset, &mut buf)?;
        let d = zlib::deflate(&buf, &[], LEVEL, Strategy::Default, true)?;
        let w = i128::from(weight);
        saved += (SAMPLE as i128 - d.len() as i128) * w;
        remaining -= w;
        if saved - SAMPLE_OVERHEAD * remaining >= goal {
            return Ok(false);
        }
    }
    Ok(saved < goal)
}

/// Whether the in-memory unit `data` (a whole small file or one chunk)
/// looks incompressible. Units under [`MIN_UNIT`] never do.
pub(crate) fn unit_incompressible(data: &[u8]) -> io::Result<bool> {
    let len = data.len() as u64;
    if len < MIN_UNIT {
        return Ok(false);
    }
    looks_incompressible(len, unit_middle(len), &mut |off, buf| {
        let off = off as usize;
        buf.copy_from_slice(&data[off..off + buf.len()]);
        Ok(())
    })
}

/// Whether the rest of a big file of `size` bytes (at least 2 [`CHUNK`]s)
/// looks incompressible, given that its first chunk does: its last
/// [`CHUNK`] bytes, then the part in between. `read(offset, buf)` reads from
/// the file.
pub(crate) fn rest_incompressible(
    size: u64,
    read: &mut dyn FnMut(u64, &mut [u8]) -> io::Result<()>,
) -> io::Result<bool> {
    let chunk = CHUNK as u64;
    debug_assert!(size >= 2 * chunk);
    let tail_start = size - chunk;
    let tail = looks_incompressible(chunk, unit_middle(chunk), &mut |off, buf| {
        read(tail_start + off, buf)
    })?;
    if !tail {
        return Ok(false);
    }
    let between = tail_start - chunk;
    let middle = (between / chunk).clamp(4, MAX_SPREAD as u64) as usize;
    if between < (middle as u64 + 2) * SAMPLE as u64 {
        return Ok(true); // too little in between to sample
    }
    looks_incompressible(between, middle, &mut |off, buf| read(chunk + off, buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    fn text(len: usize) -> Vec<u8> {
        b"The quick brown fox jumps over the lazy dog; 0123456789.\n"
            .iter()
            .copied()
            .cycle()
            .take(len)
            .collect()
    }

    fn reader(d: &[u8]) -> impl FnMut(u64, &mut [u8]) -> io::Result<()> + '_ {
        move |off, buf| {
            let off = off as usize;
            buf.copy_from_slice(&d[off..off + buf.len()]);
            Ok(())
        }
    }

    #[test]
    fn sample_counts() {
        assert_eq!(unit_middle(MIN_UNIT), 2);
        assert_eq!(unit_middle(CHUNK as u64), 6);
        assert_eq!(unit_middle(4 << 20), 8);
        // Every unit is big enough for its samples.
        for len in [MIN_UNIT, MIN_UNIT + 1, CHUNK as u64, (4 << 20) - 1] {
            assert!(len >= (unit_middle(len) as u64 + 2) * SAMPLE as u64);
        }
    }

    #[test]
    fn noise_is_incompressible_text_is_not() {
        assert!(unit_incompressible(&noise(MIN_UNIT as usize, 1)).unwrap());
        assert!(unit_incompressible(&noise(CHUNK, 2)).unwrap());
        assert!(unit_incompressible(&noise(3 << 20, 3)).unwrap());
        assert!(!unit_incompressible(&text(MIN_UNIT as usize)).unwrap());
        assert!(!unit_incompressible(&text(CHUNK)).unwrap());
        // Too small to check.
        assert!(!unit_incompressible(&noise(MIN_UNIT as usize - 1, 4)).unwrap());
        assert!(!unit_incompressible(&[]).unwrap());
    }

    #[test]
    fn compressible_head_or_tail_counts() {
        // 1 MiB of noise with 64 KiB of text at either end: the text alone
        // saves far more than 0.25% of the unit.
        for at_end in [false, true] {
            let mut d = noise(CHUNK, 5);
            let t = text(64 << 10);
            if at_end {
                let n = d.len();
                d[n - t.len()..].copy_from_slice(&t);
            } else {
                d[..t.len()].copy_from_slice(&t);
            }
            assert!(!unit_incompressible(&d).unwrap(), "at_end {at_end}");
        }
        // A small compressible header (2 KiB, like EXIF in a JPEG) does not
        // make 3 MiB of noise worth deflating.
        let mut d = noise(3 << 20, 6);
        d[..2048].copy_from_slice(&text(2048));
        assert!(unit_incompressible(&d).unwrap());
    }

    #[test]
    fn early_stop() {
        let calls = std::cell::Cell::new(0);
        let count = |d: &[u8]| {
            calls.set(0);
            let len = d.len() as u64;
            let mut r = reader(d);
            let res = looks_incompressible(len, unit_middle(len), &mut |off, buf| {
                calls.set(calls.get() + 1);
                r(off, buf)
            })
            .unwrap();
            (res, calls.get())
        };
        // Compressible data stops after the first sample.
        assert_eq!(count(&text(3 << 20)), (false, 1));
        // Noise needs every sample.
        assert_eq!(count(&noise(3 << 20, 7)), (true, 10));
    }

    #[test]
    fn rest_of_big_file() {
        let n = noise(6 * CHUNK + 1234, 8);
        let len = n.len() as u64;
        assert!(rest_incompressible(len, &mut reader(&n)).unwrap());
        // A compressible tail (like a ZIP central directory or a video index).
        let mut t = n.clone();
        t[n.len() - 200_000..].copy_from_slice(&text(200_000));
        assert!(!rest_incompressible(len, &mut reader(&t)).unwrap());
        // A compressible stretch in the middle.
        let mut m = n.clone();
        m[2 * CHUNK..4 * CHUNK].copy_from_slice(&text(2 * CHUNK));
        assert!(!rest_incompressible(len, &mut reader(&m)).unwrap());
        // Exactly two chunks: only the tail is checked.
        let two = noise(2 * CHUNK, 9);
        assert!(rest_incompressible(two.len() as u64, &mut reader(&two)).unwrap());
    }
}
