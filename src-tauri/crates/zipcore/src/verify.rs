//! Archive verification behind [`crate::test_archive`] (like `unzip -t`).
//!
//! 1. [`reader::read_directory`] parses the end records and central directory.
//! 2. Every entry's local header is read and its data span located and
//!    bounds-checked against the central directory start.
//! 3. Spans are sorted by offset; an entry whose span overlaps an earlier one
//!    (the trick behind non-recursive zip bombs) is reported and its data is
//!    not decompressed. So every compressed byte is inflated at most once and
//!    total work is bounded by Deflate's maximum ratio times the file size.
//! 4. Each entry is checked in central directory order: local-vs-central
//!    fields, the data (Stored: CRC; Deflate: streaming inflate in 64 KiB
//!    buffers, stopped as soon as it produces more than the declared size),
//!    CRC-32 and sizes against the central directory, and the data descriptor
//!    when flag bit 3 is set.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use zlib_rs::{Inflate, InflateFlush, Status};

use crate::reader::{
    self, u32_at, u64_at, CentralEntry, LocalHeader, ReadError, Source, DESCRIPTOR_SIG,
    FLAG_DESCRIPTOR, FLAG_ENCRYPTED, FLAG_STRONG_ENCRYPTION,
};
use crate::{Error, Problem, Progress, Result, TestReport};

const BUF_LEN: usize = 64 * 1024;
/// Progress is reported at least this often (in uncompressed bytes) within an entry.
const PROGRESS_STEP: u64 = 4 << 20;
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATED: u16 = 8;
const METHOD_AES: u16 = 99;
/// Raw deflate window.
const WINDOW_BITS: u8 = 15;

/// Why checking stopped early.
enum Stop {
    Io(io::Error),
    Cancelled,
}

impl From<io::Error> for Stop {
    fn from(e: io::Error) -> Self {
        Stop::Io(e)
    }
}

/// Outcome of reading one entry's data.
struct DataCheck {
    crc: u32,
    /// Uncompressed bytes produced.
    size: u64,
    /// Set when the data could not be decoded to the end (damage, or more
    /// output than declared); CRC and size are then not meaningful.
    problem: Option<String>,
}

/// Problems with an entry's local header and data span, or the header and the
/// absolute end of its data.
type Located = std::result::Result<(LocalHeader, u64), String>;

struct Checker<'a> {
    src: Source,
    cancel: &'a AtomicBool,
    progress: &'a mut dyn FnMut(&Progress),
    p: Progress,
    next_report: u64,
    inbuf: Vec<u8>,
    outbuf: Vec<u8>,
}

pub(crate) fn test_archive(
    path: &Path,
    progress: &mut dyn FnMut(&Progress),
    cancel: &AtomicBool,
) -> Result<TestReport> {
    let started = Instant::now();
    let io_err = |e: io::Error| Error::Io {
        path: path.to_path_buf(),
        source: e,
    };
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let file = File::open(path).map_err(io_err)?;
    if !file.metadata().map_err(io_err)?.is_file() {
        return Err(Error::InvalidArchive("not a regular file".into()));
    }
    let mut src = Source::new(file).map_err(io_err)?;
    let dir = reader::read_directory(&mut src).map_err(|e| match e {
        ReadError::Io(e) => io_err(e),
        ReadError::Invalid(m) => Error::InvalidArchive(m),
    })?;

    let entries = &dir.entries;
    let mut problems: Vec<Problem> = dir
        .problems
        .iter()
        .map(|m| Problem {
            entry: None,
            message: m.clone(),
        })
        .collect();

    let bytes_total = entries
        .iter()
        .filter(|e| verifiable(e).is_ok())
        .fold(0u64, |acc, e| acc.saturating_add(e.uncompressed));
    let mut ck = Checker {
        src,
        cancel,
        progress,
        p: Progress {
            current: String::new(),
            files_done: 0,
            files_total: entries.len() as u64,
            bytes_done: 0,
            bytes_total,
        },
        next_report: PROGRESS_STEP,
        inbuf: vec![0u8; BUF_LEN],
        outbuf: vec![0u8; BUF_LEN],
    };

    let mut run = || -> std::result::Result<(), Stop> {
        // Locate every entry's data.
        let mut located: Vec<Located> = Vec::with_capacity(entries.len());
        for e in entries {
            ck.check_cancel()?;
            located.push(locate(&mut ck.src, e, dir.cd_start)?);
        }
        let overlaps = find_overlaps(entries, &located);

        for (i, e) in entries.iter().enumerate() {
            ck.check_cancel()?;
            ck.p.current = e.name.clone();
            let mut msgs = e.problems.clone();
            match &located[i] {
                Err(m) => msgs.push(m.clone()),
                Ok((lh, data_end)) => {
                    compare_headers(e, lh, &mut msgs);
                    if let Some(o) = overlaps[i] {
                        msgs.push(format!(
                            "data overlaps entry \"{}\" (possible zip bomb); not checked",
                            entries[o].name
                        ));
                    } else {
                        ck.check_entry(e, lh, &mut msgs)?;
                        if lh.flags & FLAG_DESCRIPTOR != 0 {
                            if let Some(m) =
                                check_descriptor(&mut ck.src, *data_end, dir.cd_start, e, lh)?
                            {
                                msgs.push(m);
                            }
                        }
                    }
                }
            }
            if e.is_dir() && e.uncompressed != 0 {
                msgs.push(format!(
                    "directory entry has {} bytes of data",
                    e.uncompressed
                ));
            }
            problems.extend(msgs.into_iter().map(|message| Problem {
                entry: Some(e.name.clone()),
                message,
            }));
            ck.p.files_done = i as u64 + 1;
            (ck.progress)(&ck.p);
        }
        Ok(())
    };
    match run() {
        Ok(()) => {}
        Err(Stop::Cancelled) => return Err(Error::Cancelled),
        Err(Stop::Io(e)) => return Err(io_err(e)),
    }

    Ok(TestReport {
        entries: entries.len() as u64,
        bytes_checked: ck.p.bytes_done,
        problems,
        elapsed: started.elapsed(),
    })
}

/// `Ok` if this entry's data can be checked, else why not.
fn verifiable(e: &CentralEntry) -> std::result::Result<(), String> {
    if e.method == METHOD_AES {
        return Err("AES-encrypted; can't verify without the password".into());
    }
    if e.flags & (FLAG_ENCRYPTED | FLAG_STRONG_ENCRYPTION) != 0 {
        return Err("encrypted; can't verify without the password".into());
    }
    match e.method {
        METHOD_STORED | METHOD_DEFLATED => Ok(()),
        m => Err(format!(
            "compression method {m} ({}) is not supported; can't verify",
            method_name(m)
        )),
    }
}

fn method_name(m: u16) -> &'static str {
    match m {
        1 => "Shrink",
        2..=5 => "Reduce",
        6 => "Implode",
        9 => "Deflate64",
        10 => "PKWARE DCL Implode",
        12 => "BZIP2",
        14 => "LZMA",
        16 => "IBM z/OS CMPSC",
        18 => "IBM TERSE",
        19 => "IBM LZ77",
        20 | 93 => "Zstandard",
        94 => "MP3",
        95 => "XZ",
        96 => "JPEG",
        97 => "WavPack",
        98 => "PPMd",
        _ => "unknown",
    }
}

/// Read the local header and bounds-check the data span.
fn locate(src: &mut Source, e: &CentralEntry, cd_start: u64) -> io::Result<Located> {
    let lh = match reader::read_local(src, e.local_offset, cd_start)? {
        Ok(h) => h,
        Err(m) => return Ok(Err(m)),
    };
    match lh.data_start.checked_add(e.compressed) {
        Some(end) if end <= cd_start => Ok(Ok((lh, end))),
        _ => {
            let place = if lh.data_start.saturating_add(e.compressed) > src.len {
                "past the end of the file"
            } else {
                "into the central directory"
            };
            Ok(Err(format!(
                "compressed data ({} bytes at offset {}) extends {place}",
                e.compressed, lh.data_start
            )))
        }
    }
}

/// For each entry, the earlier entry (in file order) whose span it overlaps.
fn find_overlaps(entries: &[CentralEntry], located: &[Located]) -> Vec<Option<usize>> {
    let mut spans: Vec<(u64, u64, usize)> = located
        .iter()
        .enumerate()
        .filter_map(|(i, l)| {
            l.as_ref()
                .ok()
                .map(|(_, end)| (entries[i].local_offset, *end, i))
        })
        .collect();
    spans.sort_unstable();
    let mut out = vec![None; located.len()];
    let mut reach: Option<(u64, usize)> = None;
    for (start, end, i) in spans {
        if let Some((max_end, owner)) = reach {
            if start < max_end {
                out[i] = Some(owner);
                continue;
            }
        }
        if reach.is_none_or(|(m, _)| end > m) {
            reach = Some((end, i));
        }
    }
    out
}

fn compare_headers(e: &CentralEntry, lh: &LocalHeader, msgs: &mut Vec<String>) {
    if lh.raw_name != e.raw_name {
        msgs.push(format!(
            "local header name \"{}\" differs from the central directory",
            String::from_utf8_lossy(&lh.raw_name)
        ));
    }
    if lh.method != e.method {
        msgs.push(format!(
            "local header says compression method {}, the central directory {}",
            lh.method, e.method
        ));
    }
    if (lh.flags ^ e.flags) & FLAG_ENCRYPTED != 0 {
        msgs.push("encryption flag differs between local header and central directory".into());
    }
    if lh.flags & FLAG_DESCRIPTOR != 0 {
        return;
    }
    if lh.crc != e.crc {
        msgs.push(format!(
            "local header CRC-32 {:08x} differs from the central directory's {:08x}",
            lh.crc, e.crc
        ));
    }
    match (lh.compressed, lh.uncompressed) {
        (Some(c), Some(u)) => {
            if c != e.compressed {
                msgs.push(format!(
                    "local header compressed size {c} differs from the central directory's {}",
                    e.compressed
                ));
            }
            if u != e.uncompressed {
                msgs.push(format!(
                    "local header size {u} differs from the central directory's {}",
                    e.uncompressed
                ));
            }
        }
        _ => msgs.push("local header ZIP64 extra field is missing or too short".into()),
    }
}

/// Check the data descriptor at `pos`: with or without its signature, 32- or
/// 64-bit sizes; any form that matches the central directory is accepted.
fn check_descriptor(
    src: &mut Source,
    pos: u64,
    limit: u64,
    e: &CentralEntry,
    lh: &LocalHeader,
) -> io::Result<Option<String>> {
    let avail = limit.saturating_sub(pos).min(24) as usize;
    let mut b = [0u8; 24];
    src.read_at(pos, &mut b[..avail])?;
    let b = &b[..avail];
    let has_sig = avail >= 4 && u32_at(b, 0) == DESCRIPTOR_SIG;
    let wide_expected =
        lh.zip64_extra || e.compressed >= 0xFFFF_FFFF || e.uncompressed >= 0xFFFF_FFFF;
    let mut preferred = None;
    for (sig, wide) in [(true, false), (true, true), (false, false), (false, true)] {
        let off = if sig { 4 } else { 0 };
        let need = off + if wide { 20 } else { 12 };
        if avail < need || (sig && !has_sig) {
            continue;
        }
        let crc = u32_at(b, off);
        let (c, u) = if wide {
            (u64_at(b, off + 4), u64_at(b, off + 12))
        } else {
            (u64::from(u32_at(b, off + 4)), u64::from(u32_at(b, off + 8)))
        };
        if crc == e.crc && c == e.compressed && u == e.uncompressed {
            return Ok(None);
        }
        if sig == has_sig && wide == wide_expected {
            preferred = Some((crc, c, u));
        }
    }
    Ok(Some(match preferred {
        None => "data descriptor is missing".into(),
        Some((crc, c, u)) => format!(
            "data descriptor (CRC-32 {crc:08x}, sizes {c}/{u}) doesn't match the central \
             directory (CRC-32 {:08x}, sizes {}/{})",
            e.crc, e.compressed, e.uncompressed
        ),
    }))
}

/// Bytes `total_*` advanced by, for counters that may be 32-bit (Windows).
fn delta(after: u64, before: u64) -> usize {
    (after.wrapping_sub(before) & 0xFFFF_FFFF) as usize
}

impl Checker<'_> {
    fn check_cancel(&self) -> std::result::Result<(), Stop> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Count `n` checked bytes, reporting progress every [`PROGRESS_STEP`].
    fn advance(&mut self, n: usize) -> std::result::Result<(), Stop> {
        self.p.bytes_done = self.p.bytes_done.saturating_add(n as u64);
        if self.p.bytes_done >= self.next_report {
            // Next multiple of the step, so reports land every 4 MiB exactly.
            self.next_report =
                (self.p.bytes_done / PROGRESS_STEP + 1).saturating_mul(PROGRESS_STEP);
            (self.progress)(&self.p);
        }
        self.check_cancel()
    }

    fn check_entry(
        &mut self,
        e: &CentralEntry,
        lh: &LocalHeader,
        msgs: &mut Vec<String>,
    ) -> std::result::Result<(), Stop> {
        if let Err(m) = verifiable(e) {
            msgs.push(m);
            return Ok(());
        }
        let r = if e.method == METHOD_STORED {
            self.check_stored(e, lh.data_start)?
        } else {
            self.check_deflated(e, lh.data_start)?
        };
        if let Some(m) = r.problem {
            msgs.push(m);
            return Ok(());
        }
        if r.size != e.uncompressed {
            msgs.push(format!(
                "size mismatch: {} bytes of data, the central directory says {}",
                r.size, e.uncompressed
            ));
        }
        if r.crc != e.crc {
            msgs.push(format!(
                "bad CRC-32: computed {:08x}, the central directory says {:08x}",
                r.crc, e.crc
            ));
        }
        Ok(())
    }

    fn check_stored(
        &mut self,
        e: &CentralEntry,
        start: u64,
    ) -> std::result::Result<DataCheck, Stop> {
        self.src.file.seek(SeekFrom::Start(start))?;
        let mut h = crc32fast::Hasher::new();
        let mut left = e.compressed;
        while left > 0 {
            let n = left.min(BUF_LEN as u64) as usize;
            self.src.file.read_exact(&mut self.inbuf[..n])?;
            h.update(&self.inbuf[..n]);
            left -= n as u64;
            self.advance(n)?;
        }
        Ok(DataCheck {
            crc: h.finalize(),
            size: e.compressed,
            problem: None,
        })
    }

    fn check_deflated(
        &mut self,
        e: &CentralEntry,
        start: u64,
    ) -> std::result::Result<DataCheck, Stop> {
        let mut h = crc32fast::Hasher::new();
        let mut out_total = 0u64;
        if e.compressed == 0 {
            // Some writers store empty files as zero bytes of "deflate".
            return Ok(DataCheck {
                crc: 0,
                size: 0,
                problem: None,
            });
        }
        self.src.file.seek(SeekFrom::Start(start))?;
        let mut left = e.compressed;
        let mut inf = Inflate::new(false, WINDOW_BITS);
        let (mut in_pos, mut in_len) = (0usize, 0usize);
        let problem = loop {
            if in_pos == in_len && left > 0 {
                let n = left.min(BUF_LEN as u64) as usize;
                self.src.file.read_exact(&mut self.inbuf[..n])?;
                left -= n as u64;
                in_pos = 0;
                in_len = n;
            }
            let (bi, bo) = (inf.total_in(), inf.total_out());
            let r = inf.decompress(
                &self.inbuf[in_pos..in_len],
                &mut self.outbuf,
                InflateFlush::NoFlush,
            );
            let consumed = delta(inf.total_in(), bi).min(in_len - in_pos);
            let produced = delta(inf.total_out(), bo).min(self.outbuf.len());
            in_pos += consumed;
            if produced > 0 {
                h.update(&self.outbuf[..produced]);
                out_total += produced as u64;
                self.advance(produced)?;
                if out_total > e.uncompressed {
                    break Some(format!(
                        "decompresses to more than the {} bytes the central directory \
                         declares (stopped; possible zip bomb)",
                        e.uncompressed
                    ));
                }
            }
            match r {
                Ok(Status::StreamEnd) => {
                    let extra = (in_len - in_pos) as u64 + left;
                    break (extra > 0).then(|| {
                        format!("{extra} bytes of unexpected data after the compressed stream")
                    });
                }
                Ok(Status::Ok | Status::BufError) => {
                    if consumed == 0 && produced == 0 {
                        if in_pos == in_len && left == 0 {
                            break Some(
                                "compressed data ends before the end of the deflate stream \
                                 (truncated or damaged)"
                                    .into(),
                            );
                        }
                        break Some("inflate made no progress (damaged data)".into());
                    }
                }
                Err(err) => {
                    let msg = inf.error_message().unwrap_or(err.as_str());
                    break Some(format!("invalid compressed data: {msg}"));
                }
            }
        };
        Ok(DataCheck {
            crc: h.finalize(),
            size: out_total,
            problem,
        })
    }
}
