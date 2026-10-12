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
//! smaller, as deflate *stored* blocks (5 bytes of framing per 65535 bytes).
//! The chunk size is exactly 16 stored blocks, so a stored chunk costs
//! 80 bytes of framing per 1,048,560 bytes (< 1/13000) and a whole chunked
//! entry is never larger than `size + size/13000 + 64`.
//!
//! Chunking depends only on the file size, never on the thread count, so the
//! output is deterministic.

use std::io;

use flate2::{Compress, Compression, FlushCompress, Status};

/// Largest stored block payload.
pub(crate) const STORED_BLOCK_MAX: usize = 65_535;
/// Chunk size for big files: 16 full stored blocks (1 MiB - 16 bytes).
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

/// A fresh raw-deflate compressor. Deliberately not reused across inputs:
/// with the zlib-rs backend, `Compress::reset()` does not always reproduce a
/// fresh state's output (observed: +-1 byte on ~0.1% of small files), which
/// would make archives depend on which worker compressed what. Allocation is
/// cheap next to compression.
fn with_compressor<R>(level: u32, f: impl FnOnce(&mut Compress) -> R) -> R {
    f(&mut Compress::new(Compression::new(level), false))
}

fn deflate_err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("deflate failed: {e}"))
}

/// Initial output capacity: comfortably above deflate's worst-case expansion.
fn out_capacity(len: usize) -> usize {
    len + len / 8 + 128
}

/// Raw-deflate `input` (optionally primed with `dict`) and end with `Finish`
/// (`last`) or a sync flush.
pub(crate) fn deflate(input: &[u8], dict: &[u8], level: u32, last: bool) -> io::Result<Vec<u8>> {
    with_compressor(level, |c| {
        if !dict.is_empty() {
            c.set_dictionary(dict).map_err(deflate_err)?;
        }
        let flush = if last {
            FlushCompress::Finish
        } else {
            FlushCompress::Sync
        };
        let mut out = Vec::with_capacity(out_capacity(input.len()));
        let base = c.total_in();
        loop {
            let consumed = (c.total_in() - base) as usize;
            let status = c
                .compress_vec(&input[consumed..], &mut out, flush)
                .map_err(deflate_err)?;
            let consumed = (c.total_in() - base) as usize;
            let done = match status {
                Status::StreamEnd => true,
                // A flush is complete once it returns with output space left.
                Status::Ok | Status::BufError => {
                    !last && consumed == input.len() && out.len() < out.capacity()
                }
            };
            if done {
                return Ok(out);
            }
            out.reserve(out.capacity() / 2 + 1024);
        }
    })
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

/// Encode one chunk of a deflated big file: its deflate output if that is
/// smaller than the stored-block encoding, otherwise stored blocks.
pub(crate) fn encode_chunk(
    input: &[u8],
    dict: &[u8],
    level: u32,
    last: bool,
) -> io::Result<Vec<u8>> {
    let d = deflate(input, dict, level, last)?;
    if d.len() < stored_blocks_len(input.len()) {
        return Ok(d);
    }
    let mut out = Vec::new();
    stored_blocks(input, last, &mut out);
    Ok(out)
}

/// The probe rule for big files: deflate is used for the whole file if its
/// first chunk deflates to at least 0.5% less than its size.
pub(crate) fn probe_says_deflate(raw_len: usize, deflated_len: usize) -> bool {
    raw_len > 0 && (deflated_len as u64) * 200 <= (raw_len as u64) * 199
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
    use flate2::{Decompress, FlushDecompress};

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
    fn chunked(data: &[u8], level: u32) -> Vec<u8> {
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
            out.extend(encode_chunk(&data[s..e], dict, level, k == n - 1).unwrap());
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
        for level in [1, 6, 9] {
            let z = chunked(&data, level);
            assert!(z.len() < data.len());
            assert!(inflate(&z, data.len()) == data, "level {level} round trip");
        }
    }

    #[test]
    fn dictionary_priming_helps() {
        // A chunk that repeats (incompressible) bytes from the end of the
        // previous one only compresses if primed with the dictionary.
        let dict = noise(DICT, 3);
        let input = dict[1000..21_000].to_vec();
        let primed = encode_chunk(&input, &dict, 6, true).unwrap();
        let cold = encode_chunk(&input, &[], 6, true).unwrap();
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
        let z = chunked(&data, 6);
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
    fn probe_rule() {
        assert!(probe_says_deflate(1000, 995));
        assert!(!probe_says_deflate(1000, 996));
        assert!(!probe_says_deflate(0, 0));
    }
}
