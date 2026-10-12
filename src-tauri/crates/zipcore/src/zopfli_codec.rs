//! Zopfli-encoded deflate for one unit (a whole small file or one chunk of a
//! big file), via the `zopfli` crate.
//!
//! # Priming a chunk with the previous 32 KiB
//! The crate has no public "compress this range with that window" call (C
//! Zopfli's `ZopfliDeflatePart`), but its streaming [`DeflateEncoder`] does
//! exactly that internally: each `write` compresses the *previous* write as a
//! separate part, with up to 32 KiB of earlier input as the LZ77 window. So a
//! chunk is encoded by writing the dictionary first and then the chunk; the
//! output is `[blocks for the dictionary][blocks for the chunk]`, the chunk's
//! blocks back-referencing into the dictionary. [`realign`] then walks the
//! stream block by block (a small deflate parser that decodes the Huffman
//! codes but not the data), drops the dictionary's blocks, and re-emits the
//! chunk's blocks starting at a byte boundary: Huffman blocks are copied bit
//! for bit, stored blocks get their padding recomputed, BFINAL is set as the
//! caller asks, and a non-final chunk ends with an empty stored block so the
//! concatenation of all chunks is one valid stream. Cost: compressing 32 KiB
//! of dictionary that is thrown away (~3% of a 1 MiB chunk's time).
//!
//! # Cancellation
//! A Zopfli call cannot be interrupted, so the input is fed in pieces (see
//! [`piece_len`]); each piece is one part (C Zopfli's "master block", which is
//! 1 MB there) and `interrupted` is polled between pieces. Pieces only bound
//! block splitting -- the LZ77 window carries across them.
//!
//! Every result is checked by inflating it (cheap next to Zopfli) before it is
//! used; a mismatch is reported as an error rather than written.

use std::io::{self, Write};
use std::num::NonZeroU64;

use zopfli::{BlockType, DeflateEncoder, Options};

use crate::codec::Poll;
use crate::zlib;

/// Largest piece fed to one Zopfli call.
const PIECE_MAX: usize = 128 << 10;
/// Piece size for data that compresses to under [`DENSE_RATIO`] at level 9.
/// Such data (long repeats, e.g. silesia's `nci`) makes Zopfli's match finder
/// up to ~8x slower per byte, so pieces are smaller to keep cancellation fast.
const PIECE_DENSE: usize = 32 << 10;
/// "Dense" data: level-9 output below 1/8 of the input.
const DENSE_RATIO: usize = 8;
/// Smallest piece, whatever the iteration count.
const PIECE_MIN: usize = 16 << 10;
/// Iteration count the piece sizes above are tuned for (Zopfli's default).
const BASE_ITERATIONS: u64 = 15;
/// Units shorter than this are encoded as a single block (no block
/// splitting). On small files the crate's block splitter costs ~100 ms per
/// call whatever the size and, measured on the `tiny` and `source` corpora,
/// its splits make files under 4 KiB slightly *larger* in total; above that
/// they pay off.
const SPLIT_MIN: usize = 4 << 10;
/// Largest iteration count honoured; more is clamped (time grows linearly
/// with iterations and gains past a few hundred are negligible).
pub(crate) const MAX_ITERATIONS: u16 = 1000;

/// Zopfli scratch memory for one call on a piece of `piece` bytes (longest
/// match cache, cost arrays, LZ77 stores, hash chains), measured at about
/// 3 MiB + 40 bytes per input byte; this is a conservative bound.
pub(crate) const fn scratch_bytes(piece: usize) -> u64 {
    (4 << 20) + 48 * piece as u64
}

/// Upper bound of [`scratch_bytes`] over every piece size used.
pub(crate) const SCRATCH_MAX: u64 = scratch_bytes(PIECE_MAX);

/// Piece size for `iterations` and data whose level-9 deflate is
/// `level9_len` bytes for `len` input bytes. Depends only on the data and the
/// options, so the output is deterministic.
pub(crate) fn piece_len(len: usize, level9_len: usize, iterations: u16) -> usize {
    let base = if level9_len * DENSE_RATIO < len {
        PIECE_DENSE
    } else {
        PIECE_MAX
    };
    let it = u64::from(iterations.max(1));
    let scaled = (base as u64 * BASE_ITERATIONS / it.max(BASE_ITERATIONS)) as usize;
    scaled.clamp(PIECE_MIN, PIECE_MAX)
}

/// Zopfli-encode `input` (non-empty), primed with `dict` (at most 32 KiB),
/// ending with BFINAL if `last`, else on a byte boundary without BFINAL.
/// Polls `poll.interrupted` between pieces (`Ok(None)` if it says stop) and
/// reports each finished piece's length to `poll.advanced` (the lengths add
/// up to `input.len()` on success).
pub(crate) fn encode(
    input: &[u8],
    dict: &[u8],
    iterations: u16,
    piece: usize,
    last: bool,
    poll: &Poll<'_>,
) -> io::Result<Option<Vec<u8>>> {
    if input.is_empty() {
        return Err(io::Error::other("zopfli: empty input"));
    }
    let dict = &dict[dict.len().saturating_sub(DICT_MAX)..];
    let options = Options {
        iteration_count: NonZeroU64::new(u64::from(iterations.clamp(1, MAX_ITERATIONS)))
            .expect("clamped to >= 1"),
        maximum_block_splits: if input.len() < SPLIT_MIN {
            0
        } else {
            Options::default().maximum_block_splits
        },
        ..Options::default()
    };
    let mut enc = DeflateEncoder::new(options, BlockType::Dynamic, Vec::new());
    // Each write compresses the previous one; the dictionary write only sets
    // up the window (and is compressed, then discarded, by the next write).
    if !dict.is_empty() {
        enc.write_all(dict)?;
    }
    // Length of the piece the next write (or `finish`) compresses.
    let mut pending = 0u64;
    for p in input.chunks(piece.max(1)) {
        if (poll.interrupted)() {
            return Ok(None);
        }
        enc.write_all(p)?;
        if pending > 0 {
            (poll.advanced)(pending);
        }
        pending = p.len() as u64;
    }
    if (poll.interrupted)() {
        return Ok(None);
    }
    let raw = enc.finish()?;
    (poll.advanced)(pending);
    let out = if dict.is_empty() && last {
        raw
    } else {
        realign(&raw, dict.len() as u64, last)?
    };
    if !zlib::inflates_to(&out, dict, input, last) {
        return Err(io::Error::other(
            "internal error: zopfli output failed verification",
        ));
    }
    Ok(Some(out))
}

const DICT_MAX: usize = 32 << 10;

// ---------------------------------------------------------------------------
// Deflate block walker and re-emitter.
// ---------------------------------------------------------------------------

fn bad(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("zopfli realign: {what}"),
    )
}

/// LSB-first bit reader.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl BitReader<'_> {
    fn bit(&mut self) -> io::Result<u32> {
        let byte = *self
            .data
            .get(self.pos >> 3)
            .ok_or_else(|| bad("truncated"))?;
        let b = (byte >> (self.pos & 7)) & 1;
        self.pos += 1;
        Ok(u32::from(b))
    }

    fn bits(&mut self, n: u32) -> io::Result<u32> {
        let mut v = 0;
        for i in 0..n {
            v |= self.bit()? << i;
        }
        Ok(v)
    }

    fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }
}

/// Canonical Huffman decoding table (puff style).
struct Huffman {
    count: [u16; 16],
    symbol: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> io::Result<Self> {
        let mut count = [0u16; 16];
        for &l in lengths {
            count[l as usize] += 1;
        }
        let mut offs = [0u16; 16];
        for l in 1..15 {
            offs[l + 1] = offs[l] + count[l];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (s, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[offs[l as usize] as usize] = s as u16;
                offs[l as usize] += 1;
            }
        }
        // Over-subscribed codes cannot come from a valid encoder.
        let mut left: i32 = 1;
        for &c in &count[1..] {
            left = (left << 1) - i32::from(c);
            if left < 0 {
                return Err(bad("over-subscribed code"));
            }
        }
        Ok(Huffman { count, symbol })
    }

    fn decode(&self, r: &mut BitReader<'_>) -> io::Result<u16> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= r.bit()? as i32;
            let count = i32::from(self.count[len]);
            if code - first < count {
                return Ok(self.symbol[(index + code - first) as usize]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(bad("invalid Huffman code"))
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Payload at byte offset `data`, `len` bytes.
    Stored { data: usize, len: usize },
    /// Fixed or dynamic Huffman block.
    Huffman,
}

#[derive(Debug, Clone, Copy)]
struct Block {
    /// Bit position of the BFINAL bit.
    start: usize,
    /// Bit position just past the block (after end-of-block / payload).
    end: usize,
    kind: Kind,
    /// Decoded bytes the block produces.
    out: u64,
}

/// Skip the symbols of a Huffman block, counting output bytes.
fn skip_symbols(r: &mut BitReader<'_>, lit: &Huffman, dist: &Huffman) -> io::Result<u64> {
    let mut out = 0u64;
    loop {
        let sym = lit.decode(r)?;
        match sym {
            0..=255 => out += 1,
            256 => return Ok(out),
            257..=285 => {
                let i = usize::from(sym - 257);
                out += u64::from(LEN_BASE[i]) + u64::from(r.bits(u32::from(LEN_EXTRA[i]))?);
                let d = usize::from(dist.decode(r)?);
                let extra = *DIST_EXTRA.get(d).ok_or_else(|| bad("bad distance code"))?;
                r.bits(u32::from(extra))?;
            }
            _ => return Err(bad("bad literal/length code")),
        }
    }
}

fn fixed_tables() -> io::Result<(Huffman, Huffman)> {
    let mut l = [0u8; 288];
    l[..144].fill(8);
    l[144..256].fill(9);
    l[256..280].fill(7);
    l[280..].fill(8);
    Ok((Huffman::new(&l)?, Huffman::new(&[5u8; 30])?))
}

fn dynamic_tables(r: &mut BitReader<'_>) -> io::Result<(Huffman, Huffman)> {
    let nlen = r.bits(5)? as usize + 257;
    let ndist = r.bits(5)? as usize + 1;
    let ncode = r.bits(4)? as usize + 4;
    if nlen > 286 || ndist > 30 {
        return Err(bad("bad code counts"));
    }
    let mut cl = [0u8; 19];
    for &i in &CL_ORDER[..ncode] {
        cl[i] = r.bits(3)? as u8;
    }
    let clh = Huffman::new(&cl)?;
    let mut lengths = vec![0u8; nlen + ndist];
    let mut i = 0;
    while i < nlen + ndist {
        let sym = clh.decode(r)?;
        let (val, rep) = match sym {
            0..=15 => (sym as u8, 1),
            16 => {
                let prev = *lengths[..i]
                    .last()
                    .ok_or_else(|| bad("repeat with no length"))?;
                (prev, 3 + r.bits(2)? as usize)
            }
            17 => (0, 3 + r.bits(3)? as usize),
            18 => (0, 11 + r.bits(7)? as usize),
            _ => return Err(bad("bad code length code")),
        };
        if i + rep > lengths.len() {
            return Err(bad("code lengths overflow"));
        }
        lengths[i..i + rep].fill(val);
        i += rep;
    }
    Ok((
        Huffman::new(&lengths[..nlen])?,
        Huffman::new(&lengths[nlen..])?,
    ))
}

/// Walk every block of the raw deflate stream `data` up to the final one.
fn walk(data: &[u8]) -> io::Result<Vec<Block>> {
    let mut r = BitReader { data, pos: 0 };
    let fixed = fixed_tables()?;
    let mut blocks = Vec::new();
    loop {
        let start = r.pos;
        let fin = r.bit()?;
        let btype = r.bits(2)?;
        let (kind, out) = match btype {
            0 => {
                r.align();
                let len = r.bits(16)? as usize;
                let nlen = r.bits(16)? as usize;
                if len != !nlen & 0xFFFF {
                    return Err(bad("stored length check"));
                }
                let at = r.pos >> 3;
                if at + len > data.len() {
                    return Err(bad("truncated stored block"));
                }
                r.pos += 8 * len;
                (Kind::Stored { data: at, len }, len as u64)
            }
            1 => (Kind::Huffman, skip_symbols(&mut r, &fixed.0, &fixed.1)?),
            2 => {
                let (l, d) = dynamic_tables(&mut r)?;
                (Kind::Huffman, skip_symbols(&mut r, &l, &d)?)
            }
            _ => return Err(bad("reserved block type")),
        };
        blocks.push(Block {
            start,
            end: r.pos,
            kind,
            out,
        });
        if fin == 1 {
            return Ok(blocks);
        }
    }
}

/// LSB-first bit writer.
struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl BitWriter {
    fn put(&mut self, v: u64, bits: u32) {
        debug_assert!(bits <= 32);
        self.acc |= v << self.n;
        self.n += bits;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    fn align(&mut self) {
        if self.n > 0 {
            self.out.push(self.acc as u8);
            self.acc = 0;
            self.n = 0;
        }
    }

    /// Copy bits `[from, to)` of `src`.
    fn copy(&mut self, src: &[u8], mut from: usize, to: usize) {
        while from < to {
            let take = (to - from).min(24) as u32;
            // Gather `take` bits starting at `from` (spanning up to 4 bytes).
            let byte = from >> 3;
            let mut w = 0u64;
            for (i, &b) in src[byte..].iter().take(5).enumerate() {
                w |= u64::from(b) << (8 * i);
            }
            let v = (w >> (from & 7)) & ((1u64 << take) - 1);
            self.put(v, take);
            from += take as usize;
        }
    }
}

/// Re-emit the blocks of `data` that follow the first `skip_out` decoded
/// bytes, starting byte-aligned; BFINAL on the last block if `last`, else an
/// empty stored block at the end instead (byte-aligned, no BFINAL).
fn realign(data: &[u8], skip_out: u64, last: bool) -> io::Result<Vec<u8>> {
    let blocks = walk(data)?;
    let mut done = 0u64;
    let mut first = 0;
    while done < skip_out {
        let b = blocks
            .get(first)
            .ok_or_else(|| bad("dictionary not found"))?;
        done += b.out;
        first += 1;
    }
    if done != skip_out {
        return Err(bad("a block straddles the dictionary boundary"));
    }
    let keep = &blocks[first..];
    if keep.is_empty() {
        return Err(bad("no blocks after the dictionary"));
    }
    let mut w = BitWriter {
        out: Vec::with_capacity(data.len() + 16),
        acc: 0,
        n: 0,
    };
    for (i, b) in keep.iter().enumerate() {
        let fin = last && i + 1 == keep.len();
        w.put(u64::from(fin), 1);
        match b.kind {
            Kind::Stored { data: at, len } => {
                w.put(0, 2);
                w.align();
                w.put(len as u64, 16);
                w.put(!len as u64 & 0xFFFF, 16);
                w.out.extend_from_slice(&data[at..at + len]);
            }
            Kind::Huffman => w.copy(data, b.start + 1, b.end),
        }
    }
    if !last {
        // Empty stored block: byte-aligns without BFINAL (a sync flush).
        w.put(0, 3);
        w.align();
        w.out.extend_from_slice(&[0, 0, 0xFF, 0xFF]);
    } else {
        w.align();
    }
    Ok(w.out)
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
                x as u8
            })
            .collect()
    }

    fn text(len: usize, seed: u64) -> Vec<u8> {
        let words = [
            "the ", "quick ", "brown ", "fox ", "jumps ", "over ", "lazy ", "dog.\n", "and ",
            "zopfli ", "deflate ", "window ",
        ];
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut v = Vec::new();
        while v.len() < len {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            v.extend_from_slice(words[(x % words.len() as u64) as usize].as_bytes());
        }
        v.truncate(len);
        v
    }

    const NONE: Poll<'static> = Poll::NONE;

    #[test]
    fn walker_handles_all_block_types() {
        // zlib output with stored (incompressible), fixed (tiny) and dynamic
        // blocks, sync-flushed in between, re-emitted from an odd split.
        let mut s = Vec::new();
        let parts: [Vec<u8>; 3] = [text(5000, 1), noise(70_000, 2), b"ab".to_vec()];
        for (i, p) in parts.iter().enumerate() {
            s.extend(zlib::deflate(p, &[], 6, zlib::Strategy::Default, i == 2).unwrap());
        }
        let blocks = walk(&s).unwrap();
        assert!(blocks.iter().any(|b| matches!(b.kind, Kind::Stored { .. })));
        let total: u64 = blocks.iter().map(|b| b.out).sum();
        assert_eq!(total, 75_002);
        let all: Vec<u8> = parts.concat();
        // Skip the first part; the rest is re-emitted byte-aligned.
        for last in [true, false] {
            let r = realign(&s, 5000, last).unwrap();
            assert!(zlib::inflates_to(&r, &all[..5000], &all[5000..], last));
        }
    }

    /// Chunks primed with the previous bytes, some containing incompressible
    /// runs (stored blocks inside Zopfli output), concatenate to one stream;
    /// progress adds up to the input length.
    #[test]
    fn primed_chunks_round_trip_and_concatenate() {
        let mut data = text(8_000, 3);
        data.extend(noise(3_000, 4));
        data.extend(text(4_000, 5));
        let chunk = 4_001;
        let mut stream = Vec::new();
        let n = data.len().div_ceil(chunk);
        let done = std::cell::Cell::new(0u64);
        let advanced = |b| done.set(done.get() + b);
        let poll = Poll {
            interrupted: &|| false,
            advanced: &advanced,
        };
        for k in 0..n {
            let s = k * chunk;
            let e = (s + chunk).min(data.len());
            // A short dictionary keeps this fast in debug builds.
            let dict = &data[s.saturating_sub(2_000)..s];
            let last = k + 1 == n;
            let z = encode(&data[s..e], dict, 1, 1 << 10, last, &poll)
                .unwrap()
                .unwrap();
            stream.extend(z);
        }
        assert!(zlib::inflates_to(&stream, &[], &data, true));
        assert!(stream.len() < data.len() * 2 / 3);
        assert_eq!(done.get(), data.len() as u64);
    }

    #[test]
    fn priming_helps() {
        let dict = noise(8_000, 3);
        let input = dict[1000..7_000].to_vec();
        let primed = encode(&input, &dict, 1, PIECE_MAX, true, &NONE)
            .unwrap()
            .unwrap();
        assert!(primed.len() < 300, "primed: {}", primed.len());
    }

    #[test]
    fn beats_level9_on_text() {
        let data = text(8_000, 9);
        let z = encode(&data, &[], 2, PIECE_MAX, true, &NONE)
            .unwrap()
            .unwrap();
        let d9 = zlib::deflate(&data, &[], 9, zlib::Strategy::Default, true).unwrap();
        assert!(z.len() < d9.len(), "zopfli {} vs 9 {}", z.len(), d9.len());
    }

    /// The real thing: default iterations on a few hundred KiB.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "slow in debug builds")]
    fn beats_level9_on_text_default_iterations() {
        let data = text(300_000, 9);
        let z = encode(&data, &[], 15, PIECE_MAX, true, &NONE)
            .unwrap()
            .unwrap();
        let d9 = zlib::deflate(&data, &[], 9, zlib::Strategy::Default, true).unwrap();
        assert!(z.len() < d9.len(), "zopfli {} vs 9 {}", z.len(), d9.len());
    }

    #[test]
    fn interruption_stops_between_pieces() {
        let data = text(20_000, 1);
        let calls = std::cell::Cell::new(0);
        let stop = || {
            calls.set(calls.get() + 1);
            calls.get() > 2
        };
        let poll = Poll {
            interrupted: &stop,
            advanced: &|_| {},
        };
        assert!(encode(&data, &[], 1, 2 << 10, true, &poll)
            .unwrap()
            .is_none());
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn deterministic() {
        let data = text(8_000, 2);
        let run = |d: &[u8]| encode(&d[2_000..], &d[..2_000], 2, 2 << 10, false, &NONE).unwrap();
        let a = run(&data);
        let b = std::thread::spawn(move || run(&data)).join().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn piece_sizes() {
        assert_eq!(piece_len(1 << 20, 400_000, 15), PIECE_MAX);
        assert_eq!(piece_len(1 << 20, 100_000, 15), PIECE_DENSE);
        assert_eq!(piece_len(1 << 20, 400_000, 1), PIECE_MAX);
        assert_eq!(piece_len(1 << 20, 400_000, 30), PIECE_MAX / 2);
        assert_eq!(piece_len(1 << 20, 400_000, 1000), PIECE_MIN);
        assert!(scratch_bytes(PIECE_MIN) <= SCRATCH_MAX);
    }
}
