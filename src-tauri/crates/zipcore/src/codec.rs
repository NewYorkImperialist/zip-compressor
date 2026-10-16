//! Data encoding: whole-buffer deflate for small files, and pigz-style
//! chunked deflate for big ones.
//!
//! # Chunked deflate
//! A big file is cut into fixed [`CHUNK`]-byte pieces (the last one shorter).
//! Each piece is compressed independently as raw deflate, primed with the
//! previous [`DICT`] bytes of input as the preset dictionary so matches can
//! still reach back across the boundary. Non-final chunks end with a sync
//! flush (an empty stored block), which byte-aligns them without setting
//! BFINAL; the final chunk ends with `Finish`. The concatenation of the
//! chunk outputs is therefore one valid deflate stream.
//!
//! Each chunk is emitted either as its deflate output or, when that is not
//! smaller (or [`crate::detect`] finds the chunk incompressible, so it is not
//! tried), as deflate *stored* blocks (5 bytes of framing per 65535 bytes).
//! The chunk size is exactly 16 stored blocks, so a stored chunk costs
//! 80 bytes of framing per 1,048,560 bytes (< 1/13000) and a whole chunked
//! entry is never larger than `size + size/13000 + 64`.
//!
//! Chunking depends only on the file size, never on the thread count, so the
//! output is deterministic.
//!
//! # Codecs
//! A unit (a whole small file or one chunk) is encoded by its [`Codec`]:
//! levels 1-9 are one zlib-rs pass; Best adds a level-6 attempt (and, for
//! data like numeric tables, a level-9 `FILTERED` one) for small whole files
//! and keeps the smallest; Zopfli additionally runs the Zopfli
//! optimizer ([`zopfli_codec`]). Every choice is made per unit from that unit's bytes
//! alone, and priming is input-based, so per-chunk choices are independent
//! and the concatenation stays one valid stream.

use std::io;

use crate::zlib::{self, Strategy};
use crate::zopfli_codec;

/// Largest stored block payload.
pub(crate) const STORED_BLOCK_MAX: usize = 65_535;
/// Chunk size for big files: 16 full stored blocks (1 MiB - 16 bytes).
/// The same for every method (Zopfli stays cancellable through its own
/// smaller pieces, see [`zopfli_codec`]).
pub(crate) const CHUNK: usize = 16 * STORED_BLOCK_MAX;
/// Files at least this big are chunked and streamed; smaller ones are
/// compressed whole, in memory.
pub(crate) const BIG_FILE: u64 = 4 << 20;
/// Preset-dictionary length (the deflate window).
pub(crate) const DICT: usize = 32 << 10;

/// Number of chunks for a big file of `size` bytes (at least 1).
pub(crate) fn chunk_count(size: u64) -> u64 {
    size.div_ceil(CHUNK as u64).max(1)
}

/// Byte range `[start, end)` of chunk `k` in a file of `size` bytes.
pub(crate) fn chunk_range(size: u64, k: u64) -> (u64, u64) {
    let start = k * CHUNK as u64;
    (start, (start + CHUNK as u64).min(size))
}

/// Raw-deflate `input` (optionally primed with `dict`) at `level` with the
/// default strategy, and end with `Finish` (`last`) or a sync flush.
pub(crate) fn deflate(input: &[u8], dict: &[u8], level: u32, last: bool) -> io::Result<Vec<u8>> {
    zlib::deflate(input, dict, level, Strategy::Default, last)
}

/// How units are encoded (the compressing methods, after clamping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Codec {
    /// One zlib-rs pass at this level (1-9).
    Deflate(u32),
    /// [`crate::Method::DeflateBest`]: level 9, plus level 6 and (see
    /// [`tries_filtered`]) level 9 with the `FILTERED` strategy for whole
    /// files under [`BEST_SMALL`]; the smallest wins (ties: plain level 9,
    /// then level 6).
    Best,
    /// Zopfli, plus everything [`Codec::Best`] tries; the smallest wins
    /// (ties: the zlib-rs output).
    Zopfli { iterations: u16 },
}

/// Whole files below this size get the extra attempts in Best (and Zopfli)
/// mode: zlib-rs level 6 often beats level 9 on small text files, while on
/// big inputs the extra passes cost a lot for ~nothing (`FILTERED` on
/// Silesia's chunks: -0.09% for twice the time).
pub(crate) const BEST_SMALL: usize = 1 << 20;

/// Whether Best tries level 9 `FILTERED` on a small whole file that level 9
/// shrinks from `raw` to `level9` bytes: only for a ratio of 35-75%.
///
/// `FILTERED` favors literals over short matches, which pays off on data
/// like numeric tables (CSV, logs; -2% to -3% there) that land in that band.
/// Measured on 2,600 source files, the files outside it (prose and code
/// compress further, noise and media less) gained 0.04% for 60% more time;
/// inside it, a tree of 20,000 small files kept 99.97% of the gain.
pub(crate) fn tries_filtered(raw: usize, level9: usize) -> bool {
    let (raw, l9) = (raw as u64, level9 as u64);
    raw > 0 && l9 * 100 >= raw * 35 && l9 * 100 <= raw * 75
}

/// Hooks a slow unit calls as it goes.
pub(crate) struct Poll<'a> {
    /// Polled between expensive steps; true stops the unit (`Ok(None)`).
    pub interrupted: &'a dyn Fn() -> bool,
    /// Told the number of input bytes Zopfli has finished, piece by piece.
    pub advanced: &'a dyn Fn(u64),
}

#[cfg(test)]
impl Poll<'static> {
    /// Never interrupted, progress ignored.
    pub const NONE: Poll<'static> = Poll {
        interrupted: &|| false,
        advanced: &|_| {},
    };
}

/// `deflated` is at least 0.1% smaller than `raw`.
fn saves_a_tenth_percent(raw: usize, deflated: usize) -> bool {
    raw > 0 && (deflated as u64) * 1000 <= (raw as u64) * 999
}

impl Codec {
    /// The single zlib-rs level used for the first attempt.
    pub fn base_level(self) -> u32 {
        match self {
            Codec::Deflate(l) => l,
            Codec::Best | Codec::Zopfli { .. } => 9,
        }
    }
}

/// The smallest deflate encoding of one unit (a whole small file, or a chunk
/// primed with `dict`) that `codec` finds; not compared with storing.
///
/// For Best and Zopfli, units on which the first attempt (level 9) saves
/// less than 0.1% are taken as is (incompressible data: nothing else will do
/// meaningfully better, and Zopfli would take minutes for nothing).
/// Returns `Ok(None)` if interrupted.
pub(crate) fn encode_unit(
    input: &[u8],
    dict: &[u8],
    codec: Codec,
    last: bool,
    poll: &Poll<'_>,
) -> io::Result<Option<Vec<u8>>> {
    let base = deflate(input, dict, codec.base_level(), last)?;
    if matches!(codec, Codec::Deflate(_))
        || input.is_empty()
        || !saves_a_tenth_percent(input.len(), base.len())
    {
        return Ok(Some(base));
    }
    let base_len = base.len();
    let mut best = base;
    let whole_small_file = dict.is_empty() && last && input.len() < BEST_SMALL;
    if whole_small_file {
        if (poll.interrupted)() {
            return Ok(None);
        }
        let d = deflate(input, dict, 6, last)?;
        if d.len() < best.len() {
            best = d;
        }
        if tries_filtered(input.len(), base_len) {
            if (poll.interrupted)() {
                return Ok(None);
            }
            let d = zlib::deflate(input, dict, 9, Strategy::Filtered, last)?;
            if d.len() < best.len() {
                best = d;
            }
        }
    }
    if let Codec::Zopfli { iterations } = codec {
        let piece = zopfli_codec::piece_len(input.len(), base_len, iterations);
        match zopfli_codec::encode(input, dict, iterations, piece, last, poll)? {
            None => return Ok(None),
            Some(z) if z.len() < best.len() => best = z,
            Some(_) => {}
        }
    }
    Ok(Some(best))
}

/// Length of `len` bytes encoded as stored blocks (an empty input still
/// needs one empty block).
pub(crate) fn stored_blocks_len(len: usize) -> usize {
    len + 5 * len.div_ceil(STORED_BLOCK_MAX).max(1)
}

/// Encode `data` as deflate stored blocks starting at a byte boundary; the
/// last block has BFINAL set if `last`.
pub(crate) fn stored_blocks(data: &[u8], last: bool, out: &mut Vec<u8>) {
    out.reserve(stored_blocks_len(data.len()));
    let mut pieces = data.chunks(STORED_BLOCK_MAX).peekable();
    if pieces.peek().is_none() {
        out.extend_from_slice(&[u8::from(last), 0, 0, 0xFF, 0xFF]);
        return;
    }
    while let Some(p) = pieces.next() {
        let is_last = last && pieces.peek().is_none();
        let n = p.len() as u16;
        out.push(u8::from(is_last));
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&(!n).to_le_bytes());
        out.extend_from_slice(p);
    }
}

/// Encode one chunk of a deflated big file: its [`encode_unit`] output if
/// that is smaller than the stored-block encoding, otherwise stored blocks.
pub(crate) fn encode_chunk(
    input: &[u8],
    dict: &[u8],
    codec: Codec,
    last: bool,
    poll: &Poll<'_>,
) -> io::Result<Option<Vec<u8>>> {
    let Some(d) = encode_unit(input, dict, codec, last, poll)? else {
        return Ok(None);
    };
    if d.len() < stored_blocks_len(input.len()) {
        return Ok(Some(d));
    }
    Ok(Some(stored_chunk(input, last)))
}

/// One chunk of a deflated big file as stored blocks, without trying to
/// deflate it (for chunks [`crate::detect`] finds incompressible).
pub(crate) fn stored_chunk(input: &[u8], last: bool) -> Vec<u8> {
    let mut out = Vec::new();
    stored_blocks(input, last, &mut out);
    out
}

/// CRC-32 of a sequence of chunk CRCs, combined in order.
#[derive(Default)]
pub(crate) struct CrcCombiner(crc32fast::Hasher);

impl CrcCombiner {
    pub fn push(&mut self, crc: u32, len: u64) {
        self.0
            .combine(&crc32fast::Hasher::new_with_initial_len(crc, len));
    }
    pub fn finish(self) -> u32 {
        self.0.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Decompress, FlushDecompress, Status};

    fn enc(input: &[u8], dict: &[u8], codec: Codec, last: bool) -> Vec<u8> {
        encode_chunk(input, dict, codec, last, &Poll::NONE)
            .unwrap()
            .unwrap()
    }

    fn inflate(data: &[u8], cap: usize) -> Vec<u8> {
        let mut d = Decompress::new(false);
        let mut out = Vec::with_capacity(cap + 16);
        let st = d
            .decompress_vec(data, &mut out, FlushDecompress::Finish)
            .unwrap();
        assert_eq!(st, Status::StreamEnd, "deflate stream did not end");
        assert_eq!(d.total_in() as usize, data.len(), "trailing bytes");
        out
    }

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn text(len: usize) -> Vec<u8> {
        b"Lorem ipsum dolor sit amet, consectetur adipiscing elit 0123456789\n"
            .iter()
            .copied()
            .cycle()
            .take(len)
            .collect()
    }

    /// Chunk-encode `data` the way the writer does and return the stream.
    fn chunked(data: &[u8], codec: Codec) -> Vec<u8> {
        let size = data.len() as u64;
        let n = chunk_count(size);
        let mut out = Vec::new();
        for k in 0..n {
            let (s, e) = chunk_range(size, k);
            let (s, e) = (s as usize, e as usize);
            let dict = if k == 0 {
                &data[..0]
            } else {
                &data[s - DICT..s]
            };
            out.extend(enc(&data[s..e], dict, codec, k == n - 1));
        }
        out
    }

    #[test]
    fn chunk_boundaries() {
        assert_eq!(CHUNK, 1_048_560);
        assert_eq!(chunk_count(0), 1);
        assert_eq!(chunk_count(1), 1);
        assert_eq!(chunk_count(CHUNK as u64), 1);
        assert_eq!(chunk_count(CHUNK as u64 + 1), 2);
        assert_eq!(chunk_count(BIG_FILE), 5);
        let size = 3 * CHUNK as u64 + 7;
        assert_eq!(chunk_range(size, 0), (0, CHUNK as u64));
        assert_eq!(chunk_range(size, 3), (3 * CHUNK as u64, size));
        let total: u64 = (0..chunk_count(size))
            .map(|k| {
                let (s, e) = chunk_range(size, k);
                e - s
            })
            .sum();
        assert_eq!(total, size);
    }

    #[test]
    fn chunked_stream_is_one_valid_deflate_stream() {
        // Mixed: compressible, incompressible, compressible, partial tail.
        let mut data = text(CHUNK + 1234);
        data.extend(noise(2 * CHUNK, 7));
        data.extend(text(CHUNK / 2));
        for codec in [Codec::Deflate(1), Codec::Deflate(6), Codec::Best] {
            let z = chunked(&data, codec);
            assert!(z.len() < data.len());
            assert!(inflate(&z, data.len()) == data, "{codec:?} round trip");
        }
    }

    #[test]
    fn dictionary_priming_helps() {
        // A chunk that repeats (incompressible) bytes from the end of the
        // previous one only compresses if primed with the dictionary.
        let dict = noise(DICT, 3);
        let input = dict[1000..21_000].to_vec();
        let primed = enc(&input, &dict, Codec::Deflate(6), true);
        let cold = enc(&input, &[], Codec::Deflate(6), true);
        assert!(primed.len() < 1000, "primed: {}", primed.len());
        assert!(cold.len() >= input.len(), "cold: {}", cold.len());
        let mut d = Decompress::new(false);
        d.set_dictionary(&dict).unwrap();
        let mut out = Vec::with_capacity(input.len() + 16);
        d.decompress_vec(&primed, &mut out, FlushDecompress::Finish)
            .unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn incompressible_chunks_fall_back_to_stored_blocks() {
        let data = noise(3 * CHUNK + 100, 11);
        let z = chunked(&data, Codec::Deflate(6));
        assert!(inflate(&z, data.len()) == data);
        let n = data.len();
        assert!(z.len() <= n + n / 13000 + 64, "{} > bound for {n}", z.len());
        // Exactly 16 blocks per full chunk.
        assert_eq!(z.len(), n + 3 * 80 + 5);
    }

    #[test]
    fn stored_blocks_encoding() {
        let mut out = Vec::new();
        stored_blocks(&[], true, &mut out);
        assert_eq!(out, [1, 0, 0, 0xFF, 0xFF]);
        assert_eq!(inflate(&out, 0), b"");
        let data = noise(STORED_BLOCK_MAX + 3, 5);
        let mut out = Vec::new();
        stored_blocks(&data, true, &mut out);
        assert_eq!(out.len(), stored_blocks_len(data.len()));
        assert_eq!(out[0], 0);
        assert_eq!(&out[1..5], &[0xFF, 0xFF, 0, 0]);
        assert_eq!(out[5 + STORED_BLOCK_MAX], 1, "BFINAL on the last block");
        assert!(inflate(&out, data.len()) == data);
    }

    #[test]
    fn deflate_is_deterministic_across_calls_and_threads() {
        let a = text(100_000);
        let b = noise(50_000, 9);
        let first = deflate(&a, &[], 6, true).unwrap();
        let _ = deflate(&b, &a[..DICT], 6, false).unwrap();
        let again = deflate(&a, &[], 6, true).unwrap();
        let other = std::thread::spawn(move || deflate(&text(100_000), &[], 6, true).unwrap())
            .join()
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(first, other);
    }

    #[test]
    fn crc_combine_matches_whole() {
        let data = noise(3 * CHUNK + 17, 21);
        let mut c = CrcCombiner::default();
        for part in data.chunks(CHUNK) {
            c.push(crc32fast::hash(part), part.len() as u64);
        }
        assert_eq!(c.finish(), crc32fast::hash(&data));
        let mut c = CrcCombiner::default();
        c.push(crc32fast::hash(b""), 0);
        c.push(crc32fast::hash(b"abc"), 3);
        assert_eq!(c.finish(), crc32fast::hash(b"abc"));
        assert_eq!(CrcCombiner::default().finish(), 0);
    }

    #[test]
    fn best_tries_more_settings_on_small_whole_files_only() {
        let data = text(200_000);
        let d9 = deflate(&data, &[], 9, true).unwrap();
        let d6 = deflate(&data, &[], 6, true).unwrap();
        let best = enc(&data, &[], Codec::Best, true);
        assert!(
            !tries_filtered(data.len(), d9.len()),
            "text is outside the band"
        );
        assert_eq!(best.len(), d9.len().min(d6.len()));
        if d6.len() >= d9.len() {
            assert_eq!(best, d9, "ties go to level 9");
        }
        assert!(inflate(&best, data.len()) == data);
        // Small files of every kind: never larger than any single setting.
        for (i, small) in [text(300), text(5000), noise(2000, 7), {
            let mut m = text(3000);
            m.extend(noise(500, 3));
            m.extend(text(3000));
            m
        }]
        .iter()
        .enumerate()
        {
            let b = enc(small, &[], Codec::Best, true);
            let d9 = deflate(small, &[], 9, true).unwrap();
            let mut tried = vec![deflate(small, &[], 6, true).unwrap()];
            if tries_filtered(small.len(), d9.len()) {
                tried.push(zlib::deflate(small, &[], 9, Strategy::Filtered, true).unwrap());
            }
            tried.push(d9);
            for d in tried {
                assert!(b.len() <= d.len(), "case {i}: {} > {}", b.len(), d.len());
            }
            assert!(inflate(&b, small.len()) == *small, "case {i}");
        }
        // Chunks (primed, or not final) and big units are plain level 9.
        let mut s = enc(&data[..100_000], &[], Codec::Best, false);
        assert_eq!(s, enc(&data[..100_000], &[], Codec::Deflate(9), false));
        let tail = enc(
            &data[100_000..],
            &data[100_000 - DICT..100_000],
            Codec::Best,
            true,
        );
        assert_eq!(
            tail,
            enc(
                &data[100_000..],
                &data[100_000 - DICT..100_000],
                Codec::Deflate(9),
                true
            )
        );
        s.extend(tail);
        assert!(inflate(&s, data.len()) == data);
        let big = text(BEST_SMALL);
        assert_eq!(
            enc(&big, &[], Codec::Best, true),
            enc(&big, &[], Codec::Deflate(9), true)
        );
    }

    /// Rows of random integers, like a numeric CSV export.
    fn csv(rows: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        let mut out = Vec::new();
        for _ in 0..rows {
            for col in 0..8 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let digits = 1 + (x % 7) as u32;
                out.extend_from_slice(
                    (x >> 8)
                        .rem_euclid(10u64.pow(digits))
                        .to_string()
                        .as_bytes(),
                );
                out.push(if col == 7 { b'\n' } else { b',' });
            }
        }
        out
    }

    #[test]
    fn filtered_band() {
        assert!(!tries_filtered(0, 0));
        assert!(!tries_filtered(100, 34));
        assert!(tries_filtered(100, 35));
        assert!(tries_filtered(100, 75));
        assert!(!tries_filtered(100, 76));
        assert!(!tries_filtered(100, 105), "stored fallback");
    }

    #[test]
    fn best_uses_filtered_on_numeric_tables() {
        let data = csv(100, 9);
        let d9 = deflate(&data, &[], 9, true).unwrap();
        assert!(
            tries_filtered(data.len(), d9.len()),
            "{} -> {}",
            data.len(),
            d9.len()
        );
        let f9 = zlib::deflate(&data, &[], 9, Strategy::Filtered, true).unwrap();
        assert!(
            f9.len() < d9.len(),
            "FILTERED {} vs level 9 {}",
            f9.len(),
            d9.len()
        );
        let best = enc(&data, &[], Codec::Best, true);
        assert!(best.len() <= f9.len());
        assert!(inflate(&best, data.len()) == data);
    }

    #[test]
    fn zopfli_codec_never_loses_to_best_and_skips_noise() {
        let mut data = text(12_000);
        data.extend(noise(3000, 1));
        let z = enc(&data, &[], Codec::Zopfli { iterations: 1 }, true);
        let b = enc(&data, &[], Codec::Best, true);
        assert!(z.len() <= b.len());
        assert!(inflate(&z, data.len()) == data);
        // Noise: level 9 saves nothing, so the result is exactly level 9's
        // stored fallback, computed without Zopfli.
        let n = noise(100_000, 2);
        let calls = std::cell::Cell::new(0);
        let count = || {
            calls.set(calls.get() + 1);
            false
        };
        let poll = Poll {
            interrupted: &count,
            advanced: &|_| panic!("zopfli ran"),
        };
        let out = encode_chunk(&n, &[], Codec::Zopfli { iterations: 15 }, true, &poll)
            .unwrap()
            .unwrap();
        assert_eq!(calls.get(), 0);
        assert_eq!(out.len(), stored_blocks_len(n.len()));
    }

    #[test]
    fn interrupted_unit_returns_none() {
        let data = text(50_000);
        let yes = Poll {
            interrupted: &|| true,
            advanced: &|_| {},
        };
        for c in [Codec::Best, Codec::Zopfli { iterations: 1 }] {
            assert!(encode_chunk(&data, &[], c, true, &yes).unwrap().is_none());
        }
        // A single-pass level never polls.
        assert!(encode_chunk(&data, &[], Codec::Deflate(6), true, &yes)
            .unwrap()
            .is_some());
    }
}
