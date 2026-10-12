//! Raw deflate and inflate through zlib-rs, with the encoder settings flate2
//! does not expose (the strategy).
//!
//! A fresh stream is created for every call: with zlib-rs, a reset stream
//! does not always reproduce a fresh stream's output (observed: +-1 byte on
//! ~0.1% of small files), which would make archives depend on which worker
//! compressed what. Allocation is cheap next to compression.

use std::io;

use zlib_rs::c_api::z_stream;
use zlib_rs::deflate::{self, DeflateConfig, DeflateStream, Method};
use zlib_rs::{DeflateFlush, ReturnCode};

pub(crate) use zlib_rs::deflate::Strategy;

/// The deflate window (and maximum preset dictionary), as window bits.
const WINDOW_BITS: i32 = 15;
/// zlib's default memLevel (what flate2 and Info-ZIP use).
const MEM_LEVEL: i32 = 8;

fn deflate_err(what: &str, rc: ReturnCode) -> io::Error {
    io::Error::other(format!("deflate failed ({what}): {rc:?}"))
}

/// Initial output capacity: comfortably above deflate's worst-case expansion.
fn out_capacity(len: usize) -> usize {
    len + len / 8 + 128
}

/// An initialized raw-deflate `z_stream`, ended on drop.
struct Stream(z_stream);

impl Stream {
    fn new(level: u32, strategy: Strategy) -> io::Result<Self> {
        let mut s = Stream(z_stream::default());
        let config = DeflateConfig {
            level: level.min(9) as i32,
            method: Method::Deflated,
            window_bits: -WINDOW_BITS,
            mem_level: MEM_LEVEL,
            strategy,
        };
        match deflate::init(&mut s.0, config) {
            ReturnCode::Ok => Ok(s),
            rc => Err(deflate_err("init", rc)),
        }
    }

    fn get(&mut self) -> io::Result<&mut DeflateStream<'static>> {
        // SAFETY: `self.0` was initialized by `deflate::init` (checked in
        // `new`) and is only ended in `drop`; `from_stream_mut` re-checks
        // that the state and allocator are set.
        unsafe { DeflateStream::from_stream_mut(&mut self.0) }
            .ok_or_else(|| deflate_err("stream", ReturnCode::StreamError))
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Ok(s) = self.get() {
            let _ = deflate::end(s);
        }
    }
}

/// Raw-deflate `input`, primed with `dict` (at most 32 KiB is used), ending
/// with `Finish` if `last`, else with a sync flush (an empty stored block,
/// so the output ends byte-aligned without BFINAL).
pub(crate) fn deflate(
    input: &[u8],
    dict: &[u8],
    level: u32,
    strategy: Strategy,
    last: bool,
) -> io::Result<Vec<u8>> {
    let len = u32::try_from(input.len()).map_err(|_| io::Error::other("deflate input too big"))?;
    let mut s = Stream::new(level, strategy)?;
    if !dict.is_empty() {
        let dict = &dict[dict.len().saturating_sub(1 << WINDOW_BITS)..];
        match deflate::set_dictionary(s.get()?, dict) {
            ReturnCode::Ok => {}
            rc => return Err(deflate_err("set_dictionary", rc)),
        }
    }
    let flush = if last {
        DeflateFlush::Finish
    } else {
        DeflateFlush::SyncFlush
    };
    let mut out: Vec<u8> = Vec::with_capacity(out_capacity(input.len()));
    s.0.next_in = input.as_ptr().cast_mut();
    s.0.avail_in = len;
    loop {
        let spare = out.capacity() - out.len();
        let avail = spare.min(u32::MAX as usize);
        // SAFETY: `next_out` points at `avail` writable bytes of `out`'s spare
        // capacity; zlib-rs writes at most `avail_out` bytes there.
        s.0.next_out = unsafe { out.as_mut_ptr().add(out.len()) };
        s.0.avail_out = avail as u32;
        let before = s.0.total_out;
        let rc = deflate::deflate(s.get()?, flush);
        let produced = (s.0.total_out - before) as usize;
        debug_assert!(produced <= avail);
        // SAFETY: zlib-rs initialized exactly `produced` bytes past `len`.
        unsafe { out.set_len(out.len() + produced) };
        let room_left = s.0.avail_out > 0;
        match rc {
            ReturnCode::StreamEnd => break,
            // A flush is complete once it returns with output space left.
            ReturnCode::Ok | ReturnCode::BufError if !last && s.0.avail_in == 0 && room_left => {
                break
            }
            ReturnCode::Ok => {}
            ReturnCode::BufError if !room_left => {}
            rc => return Err(deflate_err("deflate", rc)),
        }
        out.reserve(out.capacity() / 2 + 1024);
    }
    // The stream must not be used past this point; `next_in` borrowed `input`.
    s.0.next_in = std::ptr::null_mut();
    s.0.avail_in = 0;
    Ok(out)
}

/// Inflate the raw-deflate `data` (primed with `dict`), which must decode to
/// exactly `expected`: a final stream if `last`, else one ending on a byte
/// boundary without BFINAL. Used to double-check encoder output that went
/// through our own bit-level rewriting (see `zopfli_codec`).
pub(crate) fn inflates_to(data: &[u8], dict: &[u8], expected: &[u8], last: bool) -> bool {
    use zlib_rs::{Inflate, InflateFlush, Status};
    let mut inf = Inflate::new(false, WINDOW_BITS as u8);
    if !dict.is_empty() && inf.set_dictionary(dict).is_err() {
        return false;
    }
    // One spare byte so that trailing output beyond `expected` is detected.
    let mut out = vec![0u8; expected.len() + 1];
    let flush = if last {
        InflateFlush::Finish
    } else {
        InflateFlush::SyncFlush
    };
    let mut in_pos = 0usize;
    let mut out_pos = 0usize;
    loop {
        let before_in = inf.total_in();
        let before_out = inf.total_out();
        let st = inf.decompress(&data[in_pos..], &mut out[out_pos..], flush);
        in_pos += (inf.total_in() - before_in) as usize;
        out_pos += (inf.total_out() - before_out) as usize;
        let progressed = inf.total_in() != before_in || inf.total_out() != before_out;
        match st {
            Ok(Status::StreamEnd) => {
                return last && in_pos == data.len() && out[..out_pos] == *expected
            }
            Ok(_) if in_pos == data.len() && !last => {
                return out_pos == expected.len() && out[..out_pos] == *expected
            }
            Ok(_) if progressed && out_pos < out.len() => {}
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(len: usize) -> Vec<u8> {
        let mut x = 7u64;
        let words = [
            "alpha ", "beta ", "gamma ", "delta\n", "epsilon ", "zeta, ", "eta ", "theta. ",
        ];
        let mut v = Vec::new();
        while v.len() < len {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            v.extend_from_slice(words[(x >> 60) as usize % words.len()].as_bytes());
        }
        v.truncate(len);
        v
    }

    /// Our wrapper at the default strategy produces exactly what flate2 (same
    /// zlib-rs engine) produces, for every level, with and without a
    /// dictionary and for both flush modes.
    #[test]
    fn matches_flate2_byte_for_byte() {
        use flate2::{Compress, Compression, FlushCompress};
        let data = text(40_000);
        let dict = text(40_000);
        for level in 1..=9u32 {
            for (d, last) in [(&[][..], true), (&dict[8_000..], false), (&dict[..], true)] {
                let ours = deflate(&data, d, level, Strategy::Default, last).unwrap();
                let mut c = Compress::new(Compression::new(level), false);
                if !d.is_empty() {
                    c.set_dictionary(&d[d.len().saturating_sub(32 << 10)..])
                        .unwrap();
                }
                let mut theirs = Vec::with_capacity(data.len() + 1024);
                let flush = if last {
                    FlushCompress::Finish
                } else {
                    FlushCompress::Sync
                };
                c.compress_vec(&data, &mut theirs, flush).unwrap();
                assert_eq!(ours, theirs, "level {level} last {last} dict {}", d.len());
                assert!(inflates_to(
                    &ours,
                    &d[d.len().saturating_sub(32 << 10)..],
                    &data,
                    last
                ));
            }
        }
    }

    #[test]
    fn filtered_and_edge_inputs_round_trip() {
        let data = text(100_000);
        for s in [Strategy::Default, Strategy::Filtered, Strategy::Rle] {
            for input in [&data[..], &[][..], &b"a"[..]] {
                for last in [true, false] {
                    let z = deflate(input, &data[..1000], 9, s, last).unwrap();
                    assert!(inflates_to(&z, &data[..1000], input, last));
                    assert!(!inflates_to(&z, &data[..1000], &data[..1], last) || input == b"a");
                }
            }
        }
    }

    #[test]
    fn inflates_to_rejects_wrong_data() {
        let data = text(50_000);
        let z = deflate(&data, &[], 6, Strategy::Default, true).unwrap();
        assert!(inflates_to(&z, &[], &data, true));
        assert!(!inflates_to(&z, &[], &data[..49_999], true));
        let mut longer = data.clone();
        longer.push(b'x');
        assert!(!inflates_to(&z, &[], &longer, true));
        let mut other = data.clone();
        other[100] ^= 1;
        assert!(!inflates_to(&z, &[], &other, true));
        assert!(!inflates_to(&z[..z.len() - 3], &[], &data, true));
        // A sync-flushed stream is not a final one, and vice versa.
        let s = deflate(&data, &[], 6, Strategy::Default, false).unwrap();
        assert!(inflates_to(&s, &[], &data, false));
        assert!(!inflates_to(&s, &[], &data, true));
    }
}
