//! Streaming archive writer.
//!
//! # Memory bound
//! File data is streamed through a single reusable `CHUNK`-sized buffer; no file
//! is ever read whole. The pre-scan keeps one small [`Entry`] per archive entry.
//!
//! # "Never larger than stored"
//! The method is decided per file before writing whenever possible:
//! - Files that fit in one `CHUNK` buffer (< 1 MiB) are read whole into that
//!   buffer and their exact deflated size is computed with a byte-counting
//!   encoder; they are deflated only if that is strictly smaller. No rewind,
//!   memory bounded by the buffer.
//! - Larger files are probed: their first `CHUNK` is deflated into a counter
//!   and, if it shrinks by less than ~1%, the file is Stored outright (video,
//!   archives, images). Otherwise the file is streamed through deflate and the
//!   result is checked; if deflate still lost overall (compressible head,
//!   incompressible body), the entry is dropped with `ZipWriter::abort_file`
//!   (which rewinds the output to the entry's local header) and written again
//!   as Stored, re-reading the file. That costs a second read in that rare
//!   case, but no extra memory or temp space.
//!
//! A rewind can leave stale bytes beyond the archive's final end; see
//! [`write_archive`] for how they're kept out of the archive body (the
//! invariant: every byte between the first local header and the central
//! directory belongs to an entry listed in the central directory).

use std::cell::Cell;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use zip::write::{FullFileOptions, SimpleFileOptions};
use zip::{CompressionMethod, ZipWriter};

use crate::timeconv;
use crate::walk::{Entry, Kind};
use crate::{CompressOptions, Error, Method, Progress, Result, Summary};

/// Read/stream buffer size.
const CHUNK: usize = 1 << 20;
/// Report progress at least this often while streaming one file.
const PROGRESS_EVERY: u64 = 4 << 20;
/// Entries at least this large are written with ZIP64 sizes. Below 4 GiB on
/// purpose: deflate can expand incompressible data slightly (~0.03%), and a
/// compressed size that crosses 4 GiB without ZIP64 would be an error.
const LARGE_FILE_THRESHOLD: u64 = 0xF000_0000;
/// Name of the transient entry used to finish a deflated file. It never stays
/// in the output (it's aborted immediately and overwritten). A NUL
/// can never appear in a real archive path, so it can't collide.
const SCRATCH_NAME: &str = "\0zipcore-scratch";

/// Size of the output write buffer. The zip crate issues many tiny writes per
/// entry (header fields, extra fields), so buffering matters for many-file trees.
const OUT_BUF: usize = 256 << 10;

/// Buffered output wrapper that tracks the logical position and the
/// high-water mark.
///
/// It can also
/// - pretend the output ends at a given position (`logical_end`), so that
///   stale bytes left behind by a rewind are invisible to `SeekFrom::End`;
/// - discard everything, so that `ZipWriter`'s drop-time finalization after
///   an error or cancel doesn't do pointless work.
pub(crate) struct Tracker<W: Write> {
    inner: W,
    /// Bytes not yet written to `inner`; they belong right before `pos`.
    pending: Vec<u8>,
    /// Logical position (including `pending`).
    pos: u64,
    max: u64,
    logical_end: Cell<Option<u64>>,
    discard: Cell<bool>,
}

impl<W: Write + Seek> Tracker<W> {
    fn new(mut inner: W) -> io::Result<Self> {
        let pos = inner.stream_position()?;
        Ok(Self {
            inner,
            pending: Vec::with_capacity(OUT_BUF),
            pos,
            max: pos,
            logical_end: Cell::new(None),
            discard: Cell::new(false),
        })
    }

    fn flush_pending(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            let r = self.inner.write_all(&self.pending);
            self.pending.clear();
            r?;
        }
        Ok(())
    }

    /// Flush buffered bytes and return the inner writer, logical end
    /// position and high-water mark.
    fn into_parts(mut self) -> io::Result<(W, u64, u64)> {
        self.flush_pending()?;
        self.inner.flush()?;
        Ok((self.inner, self.pos, self.max))
    }
}

impl<W: Write> Write for Tracker<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.discard.get() {
            return Ok(buf.len());
        }
        if self.pending.len() + buf.len() > OUT_BUF {
            let r = self.inner.write_all(&self.pending);
            self.pending.clear();
            r?;
        }
        if buf.len() >= OUT_BUF {
            self.inner.write_all(buf)?;
        } else {
            self.pending.extend_from_slice(buf);
        }
        self.pos += buf.len() as u64;
        self.max = self.max.max(self.pos);
        if let Some(end) = self.logical_end.get() {
            self.logical_end.set(Some(end.max(self.pos)));
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.discard.get() {
            return Ok(());
        }
        if !self.pending.is_empty() {
            let r = self.inner.write_all(&self.pending);
            self.pending.clear();
            r?;
        }
        self.inner.flush()
    }
}

impl<W: Write + Seek> Seek for Tracker<W> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        if self.discard.get() {
            return Ok(self.pos);
        }
        // Position queries (very frequent) don't need to touch the inner writer.
        match to {
            SeekFrom::Current(0) => return Ok(self.pos),
            SeekFrom::Start(p) if p == self.pos => return Ok(self.pos),
            _ => {}
        }
        let to = match (to, self.logical_end.get()) {
            (SeekFrom::End(off), Some(end)) => SeekFrom::Start(
                end.checked_add_signed(off)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad seek"))?,
            ),
            (to, _) => to,
        };
        self.flush_pending()?;
        self.pos = self.inner.seek(to)?;
        Ok(self.pos)
    }
}

/// Shrinks the output to the given length (e.g. `File::set_len`).
pub(crate) type TruncateFn<W> = dyn Fn(&W, u64) -> io::Result<()>;

pub(crate) struct Written<W> {
    pub inner: W,
    pub summary: Summary,
}

struct Ctx<'a> {
    progress: &'a mut dyn FnMut(&Progress),
    cancel: &'a AtomicBool,
    out_label: &'a Path,
    files_done: u64,
    files_total: u64,
    bytes_done: u64,
    bytes_total: u64,
    last_report: u64,
}

impl Ctx<'_> {
    fn report(&mut self, current: &str) {
        self.last_report = self.bytes_done;
        (self.progress)(&Progress {
            current: current.to_string(),
            files_done: self.files_done,
            files_total: self.files_total,
            bytes_done: self.bytes_done,
            bytes_total: self.bytes_total,
        });
    }
    fn check_cancel(&self) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
    fn out_err(&self, source: io::Error) -> Error {
        Error::Io {
            path: self.out_label.to_path_buf(),
            source,
        }
    }
}

/// Map a zip-crate error: I/O failures are attributed to the output.
fn zip_err(ctx: &Ctx<'_>, e: zip::result::ZipError) -> Error {
    match e {
        zip::result::ZipError::Io(io) => ctx.out_err(io),
        other => Error::Zip(other),
    }
}

/// Effective method: deflate level clamped to 0..=9; level 0 means Store
/// (the deflate backend has no level 0, and deflate-0 is just Store with overhead).
fn effective_method(m: Method) -> Method {
    match m {
        Method::Store => Method::Store,
        Method::Deflate { level } => match level.min(9) {
            0 => Method::Store,
            l => Method::Deflate { level: l },
        },
    }
}

fn base_options(entry: &Entry, large: bool) -> Result<FullFileOptions<'static, 'static>> {
    let mut o = FullFileOptions::default().large_file(large);
    if let Some(mtime) = entry.mtime {
        o = o.last_modified_time(timeconv::dos_datetime(mtime));
        if let Some(payload) = timeconv::ext_timestamp_payload(timeconv::unix_secs(mtime)) {
            o.add_extra_field(timeconv::EXT_TIMESTAMP_ID, payload, false)?;
        }
    }
    if let Some(mode) = entry.mode {
        o = o.unix_permissions(mode);
    }
    Ok(o)
}

/// Byte counter used as the probe's deflate sink.
struct Counter(u64);
impl Write for Counter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Exact raw-deflate size of `data` at `level`, computed with the same encoder
/// and settings the zip crate uses for levels 1..=9 (flate2, zlib-rs backend).
/// Deflate output doesn't depend on how the input is split into writes, so this
/// is the size the entry's data will have.
fn deflated_size(data: &[u8], level: u8) -> u64 {
    let mut enc =
        flate2::write::DeflateEncoder::new(Counter(0), flate2::Compression::new(level.into()));
    // Writing into a counter can't fail.
    let _ = enc.write_all(data);
    enc.finish().map_or(u64::MAX, |c| c.0)
}

/// True if deflating `sample` saves at least ~1%.
fn worth_deflating(sample: &[u8], level: u8) -> bool {
    !sample.is_empty() && deflated_size(sample, level) * 100 < sample.len() as u64 * 99
}

/// Read until `buf` is full or EOF.
fn read_full(src: &mut File, buf: &mut [u8], path: &Path) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                return Err(Error::Io {
                    path: path.to_path_buf(),
                    source: e,
                })
            }
        }
    }
    Ok(n)
}

/// Stream the rest of `src` into the open entry. `first` bytes of `buf` have
/// already been read and are written first. Returns the bytes written.
fn stream_into<W: Write + Seek>(
    zip: &mut ZipWriter<Tracker<W>>,
    src: &mut File,
    buf: &mut [u8],
    first: usize,
    entry: &Entry,
    ctx: &mut Ctx<'_>,
    count: bool,
) -> Result<u64> {
    let mut written = 0u64;
    let mut n = first;
    loop {
        if n > 0 {
            zip.write_all(&buf[..n]).map_err(|e| ctx.out_err(e))?;
            written += n as u64;
            if count {
                ctx.bytes_done += n as u64;
                if ctx.bytes_done.saturating_sub(ctx.last_report) >= PROGRESS_EVERY {
                    ctx.report(&entry.name);
                }
            }
        }
        ctx.check_cancel()?;
        n = match src.read(buf) {
            Ok(0) => return Ok(written),
            Ok(k) => k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => 0,
            Err(e) => {
                return Err(Error::Io {
                    path: entry.path.clone(),
                    source: e,
                })
            }
        };
    }
}

fn tracker_pos<W: Write + Seek>(zip: &ZipWriter<Tracker<W>>) -> u64 {
    zip.get_ref().map_or(0, |t| t.pos)
}

/// Write one regular file.
///
/// - Small files (they fit in one buffer): the exact deflated size is computed
///   in memory first, so the method is decided before anything is written.
/// - Larger files: a probe of the first buffer decides; if it says deflate, the
///   entry is streamed and checked afterwards, and in the rare case it still
///   came out larger than the input it's rewound and rewritten as Stored.
fn write_file<W: Write + Seek>(
    zip: &mut ZipWriter<Tracker<W>>,
    entry: &Entry,
    size: u64,
    method: Method,
    buf: &mut [u8],
    ctx: &mut Ctx<'_>,
) -> Result<()> {
    let open_err = |e| Error::Io {
        path: entry.path.clone(),
        source: e,
    };
    let mut src = File::open(&entry.path).map_err(open_err)?;
    let opts = base_options(entry, size >= LARGE_FILE_THRESHOLD)?;

    // Ask for at most size+1 bytes: enough to see EOF on a file that didn't
    // grow, without paying for a 1 MiB read request on every tiny file.
    let want = (buf.len() as u64).min(size.saturating_add(1)) as usize;
    let first = read_full(&mut src, &mut buf[..want], &entry.path)?;
    // Short read => EOF reached, the whole file is in the buffer.
    let whole_file_in_buf = first < want;
    let stored = opts.clone().compression_method(CompressionMethod::Stored);
    let level = match method {
        Method::Deflate { level } if first > 0 => {
            let use_deflate = if whole_file_in_buf {
                deflated_size(&buf[..first], level) < first as u64
            } else {
                worth_deflating(&buf[..first], level)
            };
            use_deflate.then_some(level)
        }
        _ => None,
    };
    let Some(level) = level else {
        zip.start_file(entry.name.as_str(), stored)
            .map_err(|e| zip_err(ctx, e))?;
        stream_into(zip, &mut src, buf, first, entry, ctx, true)?;
        return Ok(());
    };

    zip.start_file(
        entry.name.as_str(),
        opts.compression_method(CompressionMethod::Deflated)
            .compression_level(Some(i64::from(level))),
    )
    .map_err(|e| zip_err(ctx, e))?;
    if whole_file_in_buf {
        // Size already known to be smaller than stored; nothing more to check
        // (and avoiding the check keeps per-file cost O(1) for many small files).
        stream_into(zip, &mut src, buf, first, entry, ctx, true)?;
        return Ok(());
    }

    let data_start = tracker_pos(zip);
    let uncompressed = stream_into(zip, &mut src, buf, first, entry, ctx, true)?;
    // Finish the entry without starting a real one: open a scratch entry
    // (which finalizes this one's header) and abort it again, which rewinds
    // to exactly where this entry's data ended.
    zip.start_file(
        SCRATCH_NAME,
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
    )
    .map_err(|e| zip_err(ctx, e))?;
    zip.abort_file().map_err(|e| zip_err(ctx, e))?;
    let compressed = tracker_pos(zip) - data_start;
    if compressed <= uncompressed {
        return Ok(());
    }
    // Deflate lost: drop the entry (rewinds to its local header) and store it.
    // The bytes were already counted for progress; don't count them again.
    zip.abort_file().map_err(|e| zip_err(ctx, e))?;
    src.seek(SeekFrom::Start(0)).map_err(open_err)?;
    let first = read_full(&mut src, buf, &entry.path)?;
    zip.start_file(entry.name.as_str(), stored)
        .map_err(|e| zip_err(ctx, e))?;
    stream_into(zip, &mut src, buf, first, entry, ctx, false)?;
    Ok(())
}

/// Write `entries` as a ZIP into `out`.
pub(crate) fn write_archive<W: Write + Seek>(
    entries: &[Entry],
    out: W,
    opts: &CompressOptions,
    progress: &mut dyn FnMut(&Progress),
    cancel: &AtomicBool,
    out_label: &Path,
    truncate: Option<&TruncateFn<W>>,
) -> Result<Written<W>> {
    let started = Instant::now();
    let method = effective_method(opts.method);
    let mut ctx = Ctx {
        progress,
        cancel,
        out_label,
        files_done: 0,
        files_total: entries
            .iter()
            .filter(|e| !matches!(e.kind, Kind::Dir))
            .count() as u64,
        bytes_done: 0,
        bytes_total: entries
            .iter()
            .map(|e| match e.kind {
                Kind::File { size } => size,
                _ => 0,
            })
            .sum(),
        last_report: 0,
    };
    ctx.check_cancel()?;

    let tracker = Tracker::new(out).map_err(|e| ctx.out_err(e))?;
    let start_pos = tracker.pos;
    let mut zip = ZipWriter::new(tracker);
    let result = write_entries(&mut zip, entries, method, &mut ctx);
    let (dirs, files, symlinks) = match result {
        Ok(v) => v,
        Err(e) => {
            // Don't let ZipWriter's Drop write a central directory for a
            // half-finished archive.
            if let Some(t) = zip.get_ref() {
                t.discard.set(true);
            }
            return Err(e);
        }
    };
    // A rewound entry may have left stale bytes beyond the current position.
    // We want the central directory written right here, contiguous with the
    // last entry (streaming unzippers walk local headers up to it):
    // - truncatable output (a file): cut the stale bytes off;
    // - otherwise: hide them from `finish` (the zip crate would otherwise move
    //   the footer to the physical end, leaving a gap before it), then turn
    //   whatever stale bytes still trail the end record into a zero-filled
    //   archive comment. If they wouldn't fit in a comment (> 64 KiB; needs a
    //   multi-hundred-MiB file whose deflate attempt lost) we let the zip crate
    //   relocate the footer instead: still a valid archive, just with a gap.
    let mut comment_slack = false;
    if let Some(t) = zip.get_ref() {
        if t.max > t.pos {
            match truncate {
                Some(truncate) => truncate(&t.inner, t.pos).map_err(|e| ctx.out_err(e))?,
                None if t.max - t.pos <= u64::from(u16::MAX) => {
                    t.logical_end.set(Some(t.pos));
                    comment_slack = true;
                }
                None => {}
            }
        }
    }
    let tracker = zip.finish().map_err(|e| zip_err(&ctx, e))?;
    let (mut inner, mut pos, max) = tracker.into_parts().map_err(|e| ctx.out_err(e))?;
    if comment_slack && max > pos {
        fill_slack_as_comment(&mut inner, pos, max).map_err(|e| ctx.out_err(e))?;
        pos = max;
    }
    inner.flush().map_err(|e| ctx.out_err(e))?;
    Ok(Written {
        inner,
        summary: Summary {
            files,
            dirs,
            symlinks,
            bytes_in: ctx.bytes_done,
            bytes_out: pos - start_pos,
            elapsed: started.elapsed(),
        },
    })
}

/// The archive (written with an empty comment) ends at `end`, but the output
/// holds stale bytes up to `max`. Zero them and declare them the archive
/// comment by patching the end record's comment-length field (its last 2 bytes).
fn fill_slack_as_comment<W: Write + Seek>(w: &mut W, end: u64, max: u64) -> io::Result<()> {
    let len = u16::try_from(max - end)
        .map_err(|_| io::Error::other("archive slack too large for a comment"))?;
    w.seek(SeekFrom::Start(end - 2))?;
    w.write_all(&len.to_le_bytes())?;
    let zeros = [0u8; 4096];
    let mut left = usize::from(len);
    while left > 0 {
        let n = left.min(zeros.len());
        w.write_all(&zeros[..n])?;
        left -= n;
    }
    Ok(())
}

/// Returns (dirs, files, symlinks).
fn write_entries<W: Write + Seek>(
    zip: &mut ZipWriter<Tracker<W>>,
    entries: &[Entry],
    method: Method,
    ctx: &mut Ctx<'_>,
) -> Result<(u64, u64, u64)> {
    let mut buf = vec![0u8; CHUNK];
    let (mut dirs, mut files, mut symlinks) = (0u64, 0u64, 0u64);

    for entry in entries {
        ctx.check_cancel()?;
        match &entry.kind {
            Kind::Dir => {
                zip.add_directory(entry.name.as_str(), base_options(entry, false)?)
                    .map_err(|e| zip_err(ctx, e))?;
                dirs += 1;
            }
            Kind::Symlink { target } => {
                zip.add_symlink(
                    entry.name.as_str(),
                    target.as_str(),
                    base_options(entry, false)?,
                )
                .map_err(|e| zip_err(ctx, e))?;
                symlinks += 1;
                ctx.files_done += 1;
            }
            Kind::File { size } => {
                write_file(zip, entry, *size, method, &mut buf, ctx)?;
                files += 1;
                ctx.files_done += 1;
            }
        }
        ctx.report(&entry.name);
    }

    Ok((dirs, files, symlinks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_clamping() {
        assert_eq!(effective_method(Method::Store), Method::Store);
        assert_eq!(
            effective_method(Method::Deflate { level: 0 }),
            Method::Store
        );
        assert_eq!(
            effective_method(Method::Deflate { level: 6 }),
            Method::Deflate { level: 6 }
        );
        assert_eq!(
            effective_method(Method::Deflate { level: 200 }),
            Method::Deflate { level: 9 }
        );
    }

    #[test]
    fn probe_detects_incompressible() {
        let zeros = vec![0u8; 1 << 16];
        assert!(worth_deflating(&zeros, 6));
        // xorshift noise
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let noise: Vec<u8> = (0..1 << 16)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        assert!(!worth_deflating(&noise, 6));
        assert!(!worth_deflating(&[], 6));
    }

    #[test]
    fn tracker_tracks_high_water_mark() {
        let mut t = Tracker::new(io::Cursor::new(Vec::new())).unwrap();
        t.write_all(b"hello world").unwrap();
        t.seek(SeekFrom::Start(3)).unwrap();
        t.write_all(b"xy").unwrap();
        assert_eq!((t.pos, t.max), (5, 11));
        assert_eq!(t.stream_position().unwrap(), 5);
        t.flush().unwrap();
        t.discard.set(true);
        t.write_all(b"ignored").unwrap();
        assert_eq!(t.inner.get_ref().as_slice(), b"helxy world");
        // Seeking to End with a logical end hides stale bytes.
        t.discard.set(false);
        t.logical_end.set(Some(5));
        assert_eq!(t.seek(SeekFrom::End(0)).unwrap(), 5);
    }
}
