//! The parallel compression pipeline and the archive writer.
//!
//! # Pipeline
//! The pre-scan (on the calling thread) yields the entries in archive order.
//! Work is cut into *units*, identified by `(entry index, chunk number)`:
//! - a small regular file (< [`codec::BIG_FILE`]) is one unit: a worker opens
//!   it, reads it whole, computes its CRC and its exact deflated size, and
//!   keeps Deflate only if that is strictly smaller than the file (else
//!   Store). With `store_incompressible`, a file that [`detect`] finds
//!   incompressible is Stored without a deflate attempt;
//! - a big file is one unit per [`codec::CHUNK`], every chunk encoded
//!   independently (see [`codec`]) and falling back to stored blocks on its
//!   own if that is smaller. With `store_incompressible`, chunk 0 doubles as
//!   the probe: the whole file is Stored if [`detect`] finds its first chunk,
//!   its last MiB and samples of everything in between incompressible (video,
//!   archives, photos stay fast); otherwise chunks that look incompressible
//!   on their own are written as stored blocks without a deflate attempt.
//!   Chunks 1.. are dispatched once the probe verdict is in, so they know
//!   which method to use; the probe sends its verdict as soon as detection is
//!   done, before it encodes chunk 0, so the other chunks don't wait for
//!   that. Without detection a compressed big file is always Deflated, so
//!   its chunks need no verdict;
//! - directories and symlinks need no work and are written inline.
//!
//! Units run on a dedicated rayon pool of `threads` workers; the workers do
//! all file reading too (opening many tiny files dominates for source trees).
//! Consecutive small files are handed out in batches (one pool task and one
//! message for up to 64 files, see [`batch_limits`]), so a tiny file costs
//! its own work rather than a task hand-off and a wake-up of the writer
//! each; within a batch the worker keeps the next few files open with an OS
//! read-ahead requested (see [`readahead`]), so a cold page cache doesn't
//! mean one synchronous disk read per file. Each task sends exactly one
//! message (results for each of its units) back over a channel to the
//! calling thread, which keeps a reorder buffer and writes units strictly in
//! scan order. The calling thread is also the dispatcher, so all scheduling state
//! (budget, cursors, reorder buffer) is single-threaded and lock-free, and the
//! `progress` callback (not `Send`) is only ever called there. Zopfli units
//! count the input bytes of every piece they finish into a shared counter,
//! so while they run the writer still reports progress about every
//! [`PROGRESS_INTERVAL`]: `bytes_done` is then the bytes written plus those
//! already compressed by units not yet written (monotonic, never above the
//! total, and equal to it at the end).
//!
//! To keep the pool busy across big files, chunk 0 of upcoming big files is
//! dispatched ahead of the main cursor (at most `threads.min(16)` such probes
//! outstanding), so their method is usually known by the time they come up.
//!
//! # Memory bound
//! Every unit reserves a conservative estimate of its peak buffer use (its
//! input and output buffers) from an in-flight budget of
//! `clamp(max(64 MiB, threads x 16 MiB), ..=512 MiB)` before it is handed to
//! a worker; once its result arrives the reservation is replaced by the bytes
//! actually held, which are released when the unit is written. Work is only
//! handed out while the budget allows -- except the unit the writer needs
//! next, which is always admitted, so the pipeline can't stall behind its own
//! budget (head-of-line). When that exception applies, every other unit
//! still holding budget is an outstanding probe, so buffered data never
//! exceeds `budget + max unit cost` (about 9 MiB: a just-under-4 MiB small
//! file needs room for its input and its deflate output). Unit costs depend
//! on the codec: DeflateBest keeps one output while trying the other
//! (~3x the input); Zopfli also counts its scratch memory (~4 MiB + 48 bytes
//! per byte of a [`zopfli_codec`] piece, at most ~10 MiB) and its extra
//! buffers (max unit ~35 MiB), and gets `threads x (that + 4 MiB)` more
//! budget so every worker can still be busy. Not counted: one deflate state
//! per busy worker (~400 KB at most, far less for tiny files), the 256 KiB
//! output buffer, and the central directory, which is kept in memory
//! (~70 bytes + name per entry). Read-ahead holds at most a quarter of the
//! process's open-file limit in descriptors (and at most 1024).
//!
//! # Determinism
//! Entries are written in scan order whatever order workers finish in; each
//! unit's bytes depend only on its input data, the level and its position
//! (chunk boundaries are fixed by the file size), never on scheduling or the
//! number of threads. Every encoder is created fresh for each use (see
//! [`crate::zlib`]), and the Best / Zopfli choices are made per unit from the
//! unit's own sizes with a fixed tie-break order.
//!
//! # File contents snapshot
//! A regular file is archived as its first `size` bytes, `size` being what the
//! scan saw: bytes appended later are ignored. A small file that shrank is
//! archived as it is now; a big file that shrank, or changed between the
//! reads of two adjacent chunks (detected through a CRC of the overlapping
//! dictionary window), fails the run with an I/O error rather than producing
//! an entry that fails its CRC check.
//!
//! # Errors and cancellation
//! A failing unit (e.g. an unreadable file) stops dispatch of anything after
//! it, and later units that haven't started are skipped. Its error is
//! returned when the writer reaches it, so the error reported is always the
//! first one in scan order. On cancel the writer returns
//! [`Error::Cancelled`] within one progress step or 50 ms; queued units are
//! skipped and running ones finish their current unit before the call
//! returns (the pool scope joins them, so no work outlives the call). Slow
//! units poll for cancellation as they go: DeflateBest between its two
//! attempts, Zopfli between pieces (each well under a second of work on typical data, see
//! [`zopfli_codec`]); an interrupted unit produces no output.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::codec::{self, Codec, CrcCombiner, Poll, BIG_FILE, DICT};
use crate::container::{self, CMethod, EntryType, Meta, Sizes};
use crate::detect;
use crate::walk::{Entry, Kind};
use crate::zopfli_codec;
use crate::{CompressOptions, Error, Method, Progress, Result, Summary};

/// Report progress at least this often while streaming one big file.
const PROGRESS_EVERY: u64 = 4 << 20;
/// Output buffer size.
const OUT_BUF: usize = 256 << 10;
/// Big files at least this large get a ZIP64 extra reserved in their local
/// header (their sizes are only known after streaming). Below 4 GiB on
/// purpose: a stored-block chunk adds up to 1/13000 of framing.
const ZIP64_RESERVE: u64 = 0xF000_0000;
/// Cap on dispatched-but-unwritten units (bounds the reorder buffer and the
/// queue for trees of millions of empty files).
const MAX_OUTSTANDING: usize = 8192;
/// While slow units run, progress is reported about this often (as long as
/// some Zopfli piece finished since the last report).
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
/// How long the writer waits for a result before re-checking `cancel`.
const POLL: Duration = Duration::from_millis(50);

const MIB: u64 = 1 << 20;

mod readahead;

/// In-flight byte budget for `threads` workers.
pub(crate) fn budget_for(threads: usize) -> u64 {
    (threads as u64 * 16 * MIB).clamp(64 * MIB, 512 * MIB)
}

/// In-flight byte budget for `threads` workers running `codec`: Zopfli
/// units carry their scratch memory, so they get room for one each on top.
fn budget_with(threads: usize, codec: Option<Codec>) -> u64 {
    let base = budget_for(threads);
    match codec {
        Some(Codec::Zopfli { .. }) => {
            base + threads as u64 * (zopfli_codec::SCRATCH_MAX + 4 * codec::CHUNK as u64)
        }
        _ => base,
    }
}

/// Worker count for the `threads` option (0 = one per core).
pub(crate) fn worker_count(threads: usize) -> usize {
    match threads {
        0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
        n => n,
    }
    .min(256)
}

/// Effective method: deflate level clamped to 0..=9; level 0 means Store
/// (deflate-0 is just Store with overhead); Zopfli iterations clamped to
/// 1..=1000.
fn effective_method(m: Method) -> Method {
    match m {
        Method::Store => Method::Store,
        Method::Deflate { level } => match level.min(9) {
            0 => Method::Store,
            l => Method::Deflate { level: l },
        },
        Method::DeflateBest => Method::DeflateBest,
        Method::Zopfli { iterations } => Method::Zopfli {
            iterations: iterations.clamp(1, zopfli_codec::MAX_ITERATIONS),
        },
    }
}

/// The codec for `m`, or `None` for Store.
fn codec_for(m: Method) -> Option<Codec> {
    match effective_method(m) {
        Method::Store => None,
        Method::Deflate { level } => Some(Codec::Deflate(u32::from(level))),
        Method::DeflateBest => Some(Codec::Best),
        Method::Zopfli { iterations } => Some(Codec::Zopfli { iterations }),
    }
}

/// Small-file batch limits `(files, input bytes)` for `threads` workers:
/// at most 64 files, and at most a few milliseconds of `codec` work, but
/// small enough that there are about 8 batches per worker (so a small tree
/// still spreads over the whole pool). The first file of a batch is always
/// taken whatever its size.
///
/// Batching only changes which worker compresses which file, never the
/// bytes, so these may depend on the thread count.
fn batch_limits(entries: &[Entry], threads: usize, codec: Option<Codec>) -> (usize, u64) {
    let (count, bytes) = entries.iter().fold((0u64, 0u64), |(c, b), e| match e.kind {
        Kind::File { size } if size < BIG_FILE => (c + 1, b + size),
        _ => (c, b),
    });
    let per = threads as u64 * 8;
    let work_cap: u64 = match codec {
        None => 4 << 20,
        Some(Codec::Deflate(_)) => 512 << 10,
        Some(Codec::Best) => 256 << 10,
        Some(Codec::Zopfli { .. }) => 16 << 10,
    };
    let files = (count / per).clamp(1, 64) as usize;
    let bytes = (bytes / per).clamp(1, work_cap);
    (files, bytes)
}

/// Estimated peak buffer use of a unit over `len` input bytes encoded with
/// `codec` (`None`: stored).
fn unit_cost(len: u64, codec: Option<Codec>) -> u64 {
    let fixed = DICT as u64 + 256;
    match codec {
        None => len + 256,
        Some(Codec::Deflate(_)) => 2 * len + len / 8 + fixed,
        // Input, best output so far, current attempt.
        Some(Codec::Best) => 3 * len + len / 4 + fixed,
        // Plus Zopfli's raw and realigned outputs, the verification buffer
        // and its scratch memory.
        Some(Codec::Zopfli { .. }) => 6 * len + 2 * fixed + zopfli_codec::SCRATCH_MAX,
    }
}

/// Budget reserved for a batch of small files: they run one at a time, so
/// the peak is the biggest file's [`unit_cost`] plus the finished outputs
/// held until the batch reports back (summing every file's working memory
/// instead would starve the pool under Zopfli, whose per-unit scratch is
/// megabytes).
fn batch_cost(max_unit: u64, held: u64) -> u64 {
    max_unit + held
}

/// Bound on the output a finished small file of `len` bytes holds.
fn small_held(len: u64) -> u64 {
    len + len / 8 + 256
}

/// Output sink: buffered, with position tracking and in-place patching.
struct Out<W: Write + Seek> {
    inner: BufWriter<W>,
    start: u64,
    pos: u64,
}

impl<W: Write + Seek> Out<W> {
    fn new(mut w: W) -> io::Result<Self> {
        let pos = w.stream_position()?;
        Ok(Out {
            inner: BufWriter::with_capacity(OUT_BUF, w),
            start: pos,
            pos,
        })
    }
    fn write(&mut self, b: &[u8]) -> io::Result<()> {
        self.inner.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }
    /// Overwrite already-written bytes at absolute offset `at`.
    fn patch(&mut self, at: u64, b: &[u8]) -> io::Result<()> {
        debug_assert!(at + b.len() as u64 <= self.pos);
        self.inner.seek(SeekFrom::Start(at))?;
        self.inner.write_all(b)?;
        self.inner.seek(SeekFrom::Start(self.pos))?;
        Ok(())
    }
    fn finish(self) -> io::Result<W> {
        let mut w = self.inner.into_inner().map_err(|e| e.into_error())?;
        w.flush()?;
        Ok(w)
    }
}

/// Reorder buffer: results keyed by unit, taken out in scan order.
pub(crate) struct Reorder<K, V> {
    map: HashMap<K, V>,
}

impl<K: std::hash::Hash + Eq, V> Reorder<K, V> {
    pub fn new() -> Self {
        Reorder {
            map: HashMap::new(),
        }
    }
    pub fn insert(&mut self, k: K, v: V) {
        let prev = self.map.insert(k, v);
        debug_assert!(prev.is_none(), "unit completed twice");
    }
    pub fn take(&mut self, k: &K) -> Option<V> {
        self.map.remove(k)
    }
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.map.len()
    }
}

/// `(entry index, chunk number)`; small files are chunk 0.
type Key = (usize, u64);

/// A small file's encoded data.
struct SmallOut {
    method: CMethod,
    crc: u32,
    size: u64,
    data: Vec<u8>,
}

/// One chunk of a big file.
struct ChunkOut {
    data: Vec<u8>,
    raw_len: u64,
    crc: u32,
    /// CRC of the dictionary window this chunk was primed with (deflate only).
    dict_crc: u32,
    /// CRC of this chunk's last `DICT` input bytes (deflate only).
    tail_crc: u32,
    /// Chunk 0 only: the method for the whole file and the open file.
    probe: Option<Probe>,
}

#[derive(Clone)]
struct Probe {
    method: CMethod,
    file: Option<Arc<File>>,
}

enum Unit {
    Small(SmallOut),
    Chunk(ChunkOut),
}

impl Unit {
    fn held(&self) -> u64 {
        match self {
            Unit::Small(s) => s.data.capacity() as u64,
            Unit::Chunk(c) => c.data.capacity() as u64,
        }
    }
}

enum Msg {
    /// A probe's verdict, sent ahead of its chunk by slow codecs.
    Decision { idx: usize, probe: Probe },
    /// A finished (or failed) job: one result per unit it covered.
    Done {
        /// Budget reserved for the whole job.
        cost: u64,
        units: Vec<Done>,
    },
}

/// One unit's result.
struct Done {
    key: Key,
    /// Input bytes this unit reported through `Shared::advanced`.
    credited: u64,
    res: Result<Unit>,
}

/// A piece of work for one worker task.
enum Job {
    /// A batch of small files `(entry index, size)`, in scan order: one task
    /// and one message for many tiny files, so per-file costs are a file's
    /// own work rather than a task hand-off and a writer wake-up each.
    Small { files: Vec<(usize, u64)> },
    Chunk {
        idx: usize,
        size: u64,
        k: u64,
        method: Option<CMethod>,
        file: Option<Arc<File>>,
    },
}

/// State shared with the workers.
struct Shared<'a> {
    cancel: &'a AtomicBool,
    /// Set when the writer bails out (any reason): queued units are skipped.
    abort: AtomicBool,
    /// Units of entries after this index aren't needed (an earlier one failed).
    stop_after: AtomicUsize,
    /// How to encode, or `None` for Store.
    codec: Option<Codec>,
    /// `store_incompressible`, while compressing: see [`detect`].
    detect: bool,
    /// Input bytes slow units have finished so far (Zopfli pieces), for
    /// progress while they run.
    advanced: AtomicU64,
    /// Small files each worker opens ahead within a batch (0: none).
    readahead: usize,
}

impl Shared<'_> {
    fn skip(&self, idx: usize) -> bool {
        self.cancel.load(Ordering::Relaxed)
            || self.abort.load(Ordering::Relaxed)
            || idx > self.stop_after.load(Ordering::Relaxed)
    }
}

struct AbortOnDrop<'a>(&'a AtomicBool);
impl Drop for AbortOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn changed_error(path: &Path, what: &str) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidData, what.to_string()),
    }
}

/// Positional read of exactly `buf.len()` bytes at `offset`.
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            match file.seek_read(&mut buf[done..], offset + done as u64) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    ))
                }
                Ok(n) => done += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// The error for a failed positional read of a big file.
fn read_error(e: io::Error, path: &Path) -> Error {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        changed_error(path, "file shrank while it was being archived")
    } else {
        Error::Io {
            path: path.to_path_buf(),
            source: e,
        }
    }
}

/// Read `[start, end)` of a big file.
fn read_range(file: &File, start: u64, end: u64, path: &Path) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; (end - start) as usize];
    read_exact_at(file, &mut buf, start).map_err(|e| read_error(e, path))?;
    Ok(buf)
}

fn shrink(mut v: Vec<u8>) -> Vec<u8> {
    if v.capacity() > v.len() + v.len() / 4 + 4096 {
        v.shrink_to_fit();
    }
    v
}

/// What a worker needs besides its job: how to encode, whether to stop, and
/// where to send an early probe verdict.
struct Ctx<'a> {
    codec: Option<Codec>,
    /// Store data that looks incompressible (see [`detect`]).
    detect: bool,
    poll: Poll<'a>,
    on_decision: &'a dyn Fn(Probe),
}

fn cancelled_or<T>(r: Option<T>) -> Result<T> {
    r.ok_or(Error::Cancelled)
}

/// Encode small file `entry` of scanned size `size`, from `file` if the
/// read-ahead window already opened it.
fn run_small(
    entry: &Entry,
    size: u64,
    file: Option<File>,
    window: &mut readahead::Window<'_>,
    cx: &Ctx<'_>,
) -> Result<SmallOut> {
    let io_err = |e| Error::Io {
        path: entry.path.clone(),
        source: e,
    };
    let file = match file {
        Some(f) => Ok(f),
        None => File::open(&entry.path).or_else(|_| {
            // Maybe out of descriptors: give back the read-ahead ones.
            window.close_all();
            File::open(&entry.path)
        }),
    }
    .map_err(io_err)?;
    let mut raw = Vec::with_capacity(size as usize);
    file.take(size).read_to_end(&mut raw).map_err(io_err)?;
    let crc = crc32fast::hash(&raw);
    let size = raw.len() as u64;
    let codec = cx.codec.filter(|_| !raw.is_empty());
    let skip = codec.is_some() && cx.detect && detect::unit_incompressible(&raw).map_err(io_err)?;
    if let Some(codec) = codec.filter(|_| !skip) {
        let d = codec::encode_unit(&raw, &[], codec, true, &cx.poll).map_err(io_err)?;
        let d = cancelled_or(d)?;
        if d.len() < raw.len() {
            return Ok(SmallOut {
                method: CMethod::Deflated,
                crc,
                size,
                data: shrink(d),
            });
        }
    }
    Ok(SmallOut {
        method: CMethod::Stored,
        crc,
        size,
        data: raw,
    })
}

fn run_chunk(
    entry: &Entry,
    size: u64,
    k: u64,
    method: Option<CMethod>,
    file: Option<Arc<File>>,
    cx: &Ctx<'_>,
) -> Result<ChunkOut> {
    let path = &entry.path;
    let file = match file {
        Some(f) => f,
        None => Arc::new(File::open(path).map_err(|e| Error::Io {
            path: path.clone(),
            source: e,
        })?),
    };
    let n = codec::chunk_count(size);
    let last = k + 1 == n;
    let (start, end) = codec::chunk_range(size, k);
    let io_err = |e| Error::Io {
        path: path.clone(),
        source: e,
    };
    let tail_crc = |input: &[u8]| {
        if input.len() >= DICT {
            crc32fast::hash(&input[input.len() - DICT..])
        } else {
            0
        }
    };

    if k == 0 {
        // The probe: decides the method for the whole file (see `detect`).
        let raw = read_range(&file, start, end, path)?;
        let crc = crc32fast::hash(&raw);
        let tail = tail_crc(&raw);
        let (method, data) = match cx.codec {
            None => (CMethod::Stored, raw),
            Some(codec) => {
                let head_incompressible =
                    cx.detect && detect::unit_incompressible(&raw).map_err(io_err)?;
                let stored = head_incompressible
                    && detect::rest_incompressible(size, &mut |off, buf| {
                        read_exact_at(&file, buf, off)
                    })
                    .map_err(|e| read_error(e, path))?;
                if stored {
                    (CMethod::Stored, raw)
                } else {
                    if cx.detect {
                        // Let the other chunks start while this one encodes.
                        (cx.on_decision)(Probe {
                            method: CMethod::Deflated,
                            file: Some(file.clone()),
                        });
                    }
                    let data = if head_incompressible {
                        Some(codec::stored_chunk(&raw, last))
                    } else {
                        codec::encode_chunk(&raw, &[], codec, last, &cx.poll).map_err(io_err)?
                    };
                    (CMethod::Deflated, shrink(cancelled_or(data)?))
                }
            }
        };
        return Ok(ChunkOut {
            raw_len: end - start,
            crc,
            dict_crc: 0,
            tail_crc: tail,
            data,
            probe: Some(Probe {
                method,
                file: Some(file),
            }),
        });
    }

    match method {
        Some(CMethod::Deflated) => {
            let codec = cx.codec.expect("deflated chunk without a codec");
            let buf = read_range(&file, start - DICT as u64, end, path)?;
            let (dict, input) = buf.split_at(DICT);
            let data = if cx.detect && detect::unit_incompressible(input).map_err(io_err)? {
                Some(codec::stored_chunk(input, last))
            } else {
                codec::encode_chunk(input, dict, codec, last, &cx.poll).map_err(io_err)?
            };
            Ok(ChunkOut {
                raw_len: end - start,
                crc: crc32fast::hash(input),
                dict_crc: crc32fast::hash(dict),
                tail_crc: tail_crc(input),
                data: shrink(cancelled_or(data)?),
                probe: None,
            })
        }
        _ => {
            let raw = read_range(&file, start, end, path)?;
            Ok(ChunkOut {
                raw_len: end - start,
                crc: crc32fast::hash(&raw),
                dict_crc: 0,
                tail_crc: 0,
                data: raw,
                probe: None,
            })
        }
    }
}

/// Run one unit of entry `idx` on a worker: `f` with that unit's hooks,
/// panics turned into errors. Returns the input bytes it credited to
/// `Shared::advanced` and its result.
fn run_unit(
    shared: &Shared<'_>,
    tx: &Sender<Msg>,
    entry: &Entry,
    idx: usize,
    f: impl FnOnce(&Ctx<'_>) -> Result<Unit>,
) -> (u64, Result<Unit>) {
    if shared.skip(idx) {
        return (0, Err(Error::Cancelled));
    }
    let credited = std::cell::Cell::new(0u64);
    let interrupted = || shared.skip(idx);
    let advanced = |n| {
        credited.set(credited.get() + n);
        shared.advanced.fetch_add(n, Ordering::Relaxed);
    };
    let on_decision = |probe| {
        let _ = tx.send(Msg::Decision { idx, probe });
    };
    let cx = Ctx {
        codec: shared.codec,
        detect: shared.detect,
        poll: Poll {
            interrupted: &interrupted,
            advanced: &advanced,
        },
        on_decision: &on_decision,
    };
    let res = std::panic::catch_unwind(AssertUnwindSafe(|| f(&cx))).unwrap_or_else(|_| {
        Err(Error::Io {
            path: entry.path.clone(),
            source: io::Error::other("compression worker panicked"),
        })
    });
    (credited.get(), res)
}

/// Run `job` on a worker; one result per unit, in order.
fn run_job(entries: &[Entry], shared: &Shared<'_>, tx: &Sender<Msg>, job: Job) -> Vec<Done> {
    match job {
        Job::Small { files } => {
            let mut out = Vec::with_capacity(files.len());
            let mut window = readahead::Window::new(entries, &files, shared.readahead);
            let mut failed = false;
            for (i, &(idx, size)) in files.iter().enumerate() {
                let entry = &entries[idx];
                // After a failure the rest of the batch isn't needed (the
                // writer stops at the first error in scan order); once
                // cancelled, nothing is.
                let (credited, res) = if failed || shared.skip(idx) {
                    (0, Err(Error::Cancelled))
                } else {
                    let file = window.take(i);
                    run_unit(shared, tx, entry, idx, |cx| {
                        run_small(entry, size, file, &mut window, cx).map(Unit::Small)
                    })
                };
                failed |= res.is_err();
                out.push(Done {
                    key: (idx, 0),
                    credited,
                    res,
                });
            }
            out
        }
        Job::Chunk {
            idx,
            size,
            k,
            method,
            file,
        } => {
            let entry = &entries[idx];
            let (credited, res) = run_unit(shared, tx, entry, idx, |cx| {
                run_chunk(entry, size, k, method, file, cx).map(Unit::Chunk)
            });
            vec![Done {
                key: (idx, k),
                credited,
                res,
            }]
        }
    }
}

pub(crate) struct Written<W> {
    pub inner: W,
    pub summary: Summary,
}

/// A big file being streamed out.
struct BigWrite {
    header_offset: u64,
    method: CMethod,
    version_needed: u16,
    zip64: bool,
    chunks: u64,
    next_chunk: u64,
    crc: CrcCombiner,
    compressed: u64,
    uncompressed: u64,
    prev_tail_crc: u32,
}

struct Engine<'s, 'p, W: Write + Seek> {
    entries: &'s [Entry],
    shared: &'s Shared<'s>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    max_probes: usize,
    /// Most small files, and most input bytes (beyond the first file), in
    /// one batch.
    batch_files: usize,
    batch_bytes: u64,

    budget: u64,
    used: u64,
    in_flight: usize,
    outstanding: usize,

    // Dispatch.
    d_idx: usize,
    d_chunk: u64,
    p_idx: usize,
    probes_ahead: usize,
    probed: HashSet<usize>,
    decisions: HashMap<usize, Probe>,
    /// Arrived units: (budget held, bytes credited to progress, result).
    ready: Reorder<Key, (u64, u64, Result<Unit>)>,

    // Writing.
    w_idx: usize,
    big: Option<BigWrite>,
    out: Out<W>,
    out_label: &'s Path,
    hdr: Vec<u8>,
    cd: Vec<u8>,
    cd_count: u64,

    // Progress.
    progress: &'p mut dyn FnMut(&Progress),
    files_done: u64,
    files_total: u64,
    bytes_done: u64,
    bytes_total: u64,
    last_report: u64,
    last_report_at: Instant,
    /// `Shared::advanced` as of the last report.
    seen_advanced: u64,
    /// Credited bytes of units already taken for writing (those bytes are in
    /// `bytes_done` now).
    credit_written: u64,
    /// Last `bytes_done` reported.
    shown: u64,
    files: u64,
    dirs: u64,
    symlinks: u64,
}

impl<'s, W: Write + Seek> Engine<'s, '_, W> {
    fn out_err(&self, source: io::Error) -> Error {
        Error::Io {
            path: self.out_label.to_path_buf(),
            source,
        }
    }

    fn check_cancel(&self) -> Result<()> {
        if self.shared.cancel.load(Ordering::Relaxed) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    fn report(&mut self, current: &str) -> Result<()> {
        self.last_report = self.bytes_done;
        self.last_report_at = Instant::now();
        // Bytes written, plus bytes that running or finished-but-unwritten
        // slow units have already compressed; monotonic and <= total.
        self.seen_advanced = self.shared.advanced.load(Ordering::Relaxed);
        let in_flight = self.seen_advanced.saturating_sub(self.credit_written);
        let shown = (self.bytes_done + in_flight)
            .min(self.bytes_total)
            .max(self.shown);
        self.shown = shown;
        (self.progress)(&Progress {
            current: current.to_string(),
            files_done: self.files_done,
            files_total: self.files_total,
            bytes_done: shown,
            bytes_total: self.bytes_total,
        });
        self.check_cancel()
    }

    /// Report work done inside slow units if there is news and the last
    /// report is [`PROGRESS_INTERVAL`] old.
    fn report_in_flight(&mut self) -> Result<()> {
        if self.w_idx < self.entries.len()
            && self.shared.advanced.load(Ordering::Relaxed) != self.seen_advanced
            && self.last_report_at.elapsed() >= PROGRESS_INTERVAL
        {
            let entries = self.entries;
            self.report(&entries[self.w_idx].name)?;
        }
        Ok(())
    }

    fn compressing(&self) -> bool {
        self.shared.codec.is_some()
    }

    /// Method (and open file) for the chunks of big entry `idx`, if known.
    /// Only detection can make a compressed big file Stored, so without it
    /// the method is known before the probe has run.
    fn decision(&self, idx: usize) -> Option<Probe> {
        let known = |method| Probe { method, file: None };
        if !self.compressing() {
            Some(known(CMethod::Stored))
        } else if let Some(p) = self.decisions.get(&idx) {
            Some(p.clone())
        } else if !self.shared.detect {
            Some(known(CMethod::Deflated))
        } else {
            None
        }
    }

    fn admit(&mut self, cost: u64, head_of_line: bool) -> bool {
        if head_of_line || self.used + cost <= self.budget {
            self.used += cost;
            true
        } else {
            false
        }
    }

    /// Hand `job` (covering `units` units, reserving `cost`) to the pool.
    fn spawn(&mut self, scope: &rayon::Scope<'s>, cost: u64, units: usize, job: Job) {
        let tx = self.tx.clone();
        let shared = self.shared;
        let entries = self.entries;
        self.in_flight += 1;
        self.outstanding += units;
        scope.spawn(move |_| {
            let units = run_job(entries, shared, &tx, job);
            // The writer may already have returned; then nobody needs this.
            let _ = tx.send(Msg::Done { cost, units });
        });
    }

    /// Hand out as much work as the budget allows, in scan order.
    fn dispatch(&mut self, scope: &rayon::Scope<'s>) {
        let n = self.entries.len();
        let stop = self.shared.stop_after.load(Ordering::Relaxed);
        let codec = self.shared.codec;
        while self.d_idx < n && self.d_idx <= stop && self.outstanding < MAX_OUTSTANDING {
            let idx = self.d_idx;
            let size = match self.entries[idx].kind {
                Kind::File { size } => size,
                _ => {
                    self.d_idx += 1;
                    continue;
                }
            };
            if size < BIG_FILE {
                let mut max_unit = unit_cost(size, codec);
                let mut held = small_held(size);
                let mut cost = batch_cost(max_unit, held);
                if !self.admit(cost, idx == self.w_idx) {
                    break;
                }
                self.d_idx += 1;
                // Batch the small files that follow (directories and
                // symlinks in between need no work), up to the next big file.
                let mut files = vec![(idx, size)];
                let mut bytes = size;
                while files.len() < self.batch_files
                    && self.d_idx < n
                    && self.d_idx <= stop
                    && self.outstanding + files.len() < MAX_OUTSTANDING
                {
                    let size = match self.entries[self.d_idx].kind {
                        Kind::File { size } if size < BIG_FILE => size,
                        Kind::File { .. } => break,
                        _ => {
                            self.d_idx += 1;
                            continue;
                        }
                    };
                    let grown = batch_cost(
                        max_unit.max(unit_cost(size, codec)),
                        held + small_held(size),
                    );
                    if bytes + size > self.batch_bytes || !self.admit(grown - cost, false) {
                        break;
                    }
                    files.push((self.d_idx, size));
                    bytes += size;
                    max_unit = max_unit.max(unit_cost(size, codec));
                    held += small_held(size);
                    cost = grown;
                    self.d_idx += 1;
                }
                let units = files.len();
                self.spawn(scope, cost, units, Job::Small { files });
                continue;
            }
            let chunks = codec::chunk_count(size);
            if self.d_chunk >= chunks {
                self.d_idx += 1;
                self.d_chunk = 0;
                continue;
            }
            let k = self.d_chunk;
            if k == 0 && self.probed.remove(&idx) {
                self.probes_ahead -= 1;
                self.d_chunk = 1;
                continue;
            }
            let (start, end) = codec::chunk_range(size, k);
            let (cost, job) = if k == 0 {
                let job = Job::Chunk {
                    idx,
                    size,
                    k,
                    method: None,
                    file: None,
                };
                (unit_cost(end - start, codec), job)
            } else {
                let Some(p) = self.decision(idx) else {
                    break; // waiting for the probe
                };
                let chunk_codec = codec.filter(|_| p.method == CMethod::Deflated);
                let job = Job::Chunk {
                    idx,
                    size,
                    k,
                    method: Some(p.method),
                    file: p.file,
                };
                (unit_cost(end - start, chunk_codec), job)
            };
            let next_needed =
                self.w_idx == idx && self.big.as_ref().map_or(0, |b| b.next_chunk) == k;
            if !self.admit(cost, next_needed) {
                break;
            }
            self.spawn(scope, cost, 1, job);
            self.d_chunk += 1;
        }

        // Probe upcoming big files early.
        if codec.is_none() {
            return;
        }
        self.p_idx = self.p_idx.max(self.d_idx + 1);
        while self.probes_ahead < self.max_probes
            && self.p_idx < n
            && self.p_idx <= stop
            && self.outstanding < MAX_OUTSTANDING
        {
            let idx = self.p_idx;
            if let Kind::File { size } = self.entries[idx].kind {
                if size >= BIG_FILE {
                    let (start, end) = codec::chunk_range(size, 0);
                    let cost = unit_cost(end - start, codec);
                    if !self.admit(cost, false) {
                        break;
                    }
                    let job = Job::Chunk {
                        idx,
                        size,
                        k: 0,
                        method: None,
                        file: None,
                    };
                    self.spawn(scope, cost, 1, job);
                    self.probed.insert(idx);
                    self.probes_ahead += 1;
                }
            }
            self.p_idx += 1;
        }
    }

    fn receive(&mut self, msg: Msg) {
        let (cost, units) = match msg {
            Msg::Decision { idx, probe } => {
                self.decisions.entry(idx).or_insert(probe);
                return;
            }
            Msg::Done { cost, units } => (cost, units),
        };
        self.in_flight -= 1;
        self.used -= cost;
        for d in units {
            self.receive_unit(d);
        }
    }

    fn receive_unit(&mut self, d: Done) {
        let Done {
            key,
            credited,
            mut res,
        } = d;
        match &mut res {
            Ok(Unit::Chunk(c)) => {
                if let Some(p) = c.probe.take() {
                    self.decisions.entry(key.0).or_insert(p);
                }
            }
            Ok(Unit::Small(_)) => {}
            Err(Error::Cancelled) => {}
            Err(_) => {
                self.shared.stop_after.fetch_min(key.0, Ordering::Relaxed);
            }
        }
        let held = res.as_ref().map_or(0, Unit::held);
        self.used += held;
        self.ready.insert(key, (held, credited, res));
    }

    /// Take the result for `key` if it has arrived.
    fn take(&mut self, key: Key) -> Option<Result<Unit>> {
        let (held, credited, res) = self.ready.take(&key)?;
        self.used -= held;
        self.credit_written += credited;
        self.outstanding -= 1;
        Some(res)
    }

    /// Write an entry whose data and sizes are all known.
    fn write_exact(
        &mut self,
        meta: &Meta<'_>,
        method: CMethod,
        sizes: Sizes,
        data: &[u8],
    ) -> Result<()> {
        let offset = self.out.pos;
        let zip64 = container::central_needs_zip64(&sizes, offset);
        let vn = container::version_needed(meta.kind, method, zip64);
        self.hdr.clear();
        container::local_header(meta, method, vn, &sizes, sizes.overflow(), &mut self.hdr);
        let r = self.out.write(&self.hdr).and_then(|_| self.out.write(data));
        r.map_err(|e| self.out_err(e))?;
        container::central_header(meta, method, vn, &sizes, offset, &mut self.cd);
        self.cd_count += 1;
        Ok(())
    }

    fn meta(entry: &Entry, kind: EntryType) -> Result<Meta<'_>> {
        if !container::name_fits(&entry.name) {
            return Err(Error::Io {
                path: entry.path.clone(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "archive path is longer than 65535 bytes",
                ),
            });
        }
        Ok(Meta::new(&entry.name, kind, entry.mode, entry.mtime))
    }

    /// Write every entry whose data is available, in order. Returns when the
    /// next entry is still waiting on a worker (or all are written).
    fn write_ready(&mut self) -> Result<()> {
        let entries = self.entries;
        while self.w_idx < entries.len() {
            let idx = self.w_idx;
            let entry = &entries[idx];
            match &entry.kind {
                Kind::Dir => {
                    let meta = Self::meta(entry, EntryType::Dir)?;
                    self.write_exact(&meta, CMethod::Stored, Sizes::default(), &[])?;
                    self.dirs += 1;
                }
                Kind::Symlink { target } => {
                    let meta = Self::meta(entry, EntryType::Symlink)?;
                    let data = target.as_bytes();
                    let sizes = Sizes {
                        crc: crc32fast::hash(data),
                        compressed: data.len() as u64,
                        uncompressed: data.len() as u64,
                    };
                    self.write_exact(&meta, CMethod::Stored, sizes, data)?;
                    self.symlinks += 1;
                    self.files_done += 1;
                }
                Kind::File { size } if *size < BIG_FILE => {
                    let Some(res) = self.take((idx, 0)) else {
                        return Ok(());
                    };
                    let Unit::Small(s) = res? else {
                        unreachable!("small file got a chunk result")
                    };
                    let meta = Self::meta(entry, EntryType::File)?;
                    let sizes = Sizes {
                        crc: s.crc,
                        compressed: s.data.len() as u64,
                        uncompressed: s.size,
                    };
                    self.write_exact(&meta, s.method, sizes, &s.data)?;
                    self.bytes_done += s.size;
                    self.files += 1;
                    self.files_done += 1;
                }
                Kind::File { size } => {
                    if !self.write_big_chunks(entry, idx, *size)? {
                        return Ok(());
                    }
                    self.files += 1;
                    self.files_done += 1;
                }
            }
            self.report(&entry.name)?;
            self.w_idx += 1;
        }
        Ok(())
    }

    /// Write the available chunks of big entry `idx`; true once it's complete.
    fn write_big_chunks(&mut self, entry: &Entry, idx: usize, size: u64) -> Result<bool> {
        let meta = Self::meta(entry, EntryType::File)?;
        loop {
            let k = self.big.as_ref().map_or(0, |b| b.next_chunk);
            let Some(res) = self.take((idx, k)) else {
                return Ok(false);
            };
            let Unit::Chunk(c) = res? else {
                unreachable!("big file got a small-file result")
            };
            if k == 0 {
                let method = self
                    .decision(idx)
                    .expect("probe result recorded on receipt")
                    .method;
                let offset = self.out.pos;
                let zip64 = size >= ZIP64_RESERVE;
                let vn = container::version_needed(
                    EntryType::File,
                    method,
                    zip64 || offset >= 0xFFFF_FFFF,
                );
                self.hdr.clear();
                container::local_header(&meta, method, vn, &Sizes::default(), zip64, &mut self.hdr);
                let r = self.out.write(&self.hdr);
                r.map_err(|e| self.out_err(e))?;
                self.big = Some(BigWrite {
                    header_offset: offset,
                    method,
                    version_needed: vn,
                    zip64,
                    chunks: codec::chunk_count(size),
                    next_chunk: 0,
                    crc: CrcCombiner::default(),
                    compressed: 0,
                    uncompressed: 0,
                    prev_tail_crc: 0,
                });
            }
            let b = self.big.as_mut().expect("big entry started");
            if b.method == CMethod::Deflated && k > 0 && c.dict_crc != b.prev_tail_crc {
                return Err(changed_error(
                    &entry.path,
                    "file changed while it was being archived",
                ));
            }
            b.crc.push(c.crc, c.raw_len);
            b.compressed += c.data.len() as u64;
            b.uncompressed += c.raw_len;
            b.prev_tail_crc = c.tail_crc;
            b.next_chunk += 1;
            let finished = b.next_chunk == b.chunks;
            let r = self.out.write(&c.data);
            r.map_err(|e| self.out_err(e))?;
            self.bytes_done += c.raw_len;
            drop(c);
            if finished {
                break;
            }
            if self.bytes_done - self.last_report >= PROGRESS_EVERY {
                self.report(&entry.name)?;
            }
        }

        let b = self.big.take().expect("big entry started");
        self.decisions.remove(&idx);
        let sizes = Sizes {
            crc: b.crc.finish(),
            compressed: b.compressed,
            uncompressed: b.uncompressed,
        };
        if !b.zip64 && sizes.overflow() {
            return Err(changed_error(
                &entry.path,
                "entry unexpectedly exceeded 4 GiB without a ZIP64 header",
            ));
        }
        self.hdr.clear();
        container::local_header(
            &meta,
            b.method,
            b.version_needed,
            &sizes,
            b.zip64,
            &mut self.hdr,
        );
        let r = self.out.patch(b.header_offset, &self.hdr);
        r.map_err(|e| self.out_err(e))?;
        container::central_header(
            &meta,
            b.method,
            b.version_needed,
            &sizes,
            b.header_offset,
            &mut self.cd,
        );
        self.cd_count += 1;
        Ok(true)
    }

    fn run(&mut self, scope: &rayon::Scope<'s>) -> Result<()> {
        loop {
            self.check_cancel()?;
            self.write_ready()?;
            if self.w_idx == self.entries.len() {
                return Ok(());
            }
            self.dispatch(scope);
            if self.in_flight == 0 {
                // Unreachable by construction (the next unit is always
                // admitted); fail rather than wait forever.
                return Err(self.out_err(io::Error::other(
                    "internal error: compression pipeline stalled",
                )));
            }
            match self.rx.recv_timeout(POLL) {
                Ok(msg) => {
                    self.receive(msg);
                    while let Ok(msg) = self.rx.try_recv() {
                        self.receive(msg);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    unreachable!("the engine holds a sender")
                }
            }
            self.report_in_flight()?;
        }
    }

    /// Write the central directory and end records.
    fn finish(self) -> Result<Written<W>> {
        let mut out = self.out;
        let label = self.out_label;
        let err = |e| Error::Io {
            path: label.to_path_buf(),
            source: e,
        };
        let cd_offset = out.pos;
        out.write(&self.cd).map_err(err)?;
        let mut end = Vec::new();
        container::end_records(self.cd_count, cd_offset, self.cd.len() as u64, &mut end);
        out.write(&end).map_err(err)?;
        let bytes_out = out.pos - out.start;
        let inner = out.finish().map_err(err)?;
        Ok(Written {
            inner,
            summary: Summary {
                files: self.files,
                dirs: self.dirs,
                symlinks: self.symlinks,
                bytes_in: self.bytes_done,
                bytes_out,
                elapsed: Duration::ZERO,
            },
        })
    }
}

/// Write `entries` as a ZIP into `out`.
pub(crate) fn write_archive<W: Write + Seek>(
    entries: &[Entry],
    out: W,
    opts: &CompressOptions,
    progress: &mut dyn FnMut(&Progress),
    cancel: &AtomicBool,
    out_label: &Path,
) -> Result<Written<W>> {
    let started = Instant::now();
    let out_err = |source| Error::Io {
        path: out_label.to_path_buf(),
        source,
    };
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let codec = codec_for(opts.method);
    let threads = worker_count(opts.threads);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("zipcore-{i}"))
        .build()
        .map_err(|e| out_err(io::Error::other(e.to_string())))?;
    let out = Out::new(out).map_err(out_err)?;

    let shared = Shared {
        cancel,
        abort: AtomicBool::new(false),
        stop_after: AtomicUsize::new(usize::MAX),
        codec,
        detect: opts.store_incompressible && codec.is_some(),
        advanced: AtomicU64::new(0),
        readahead: readahead::depth_for(threads),
    };
    let (tx, rx) = mpsc::channel();
    let files_total = entries
        .iter()
        .filter(|e| !matches!(e.kind, Kind::Dir))
        .count() as u64;
    let bytes_total = entries
        .iter()
        .map(|e| match e.kind {
            Kind::File { size } => size,
            _ => 0,
        })
        .sum();

    let (batch_files, batch_bytes) = batch_limits(entries, threads, codec);

    let shared_ref = &shared;
    let mut written = pool.in_place_scope(|scope| {
        // Whatever happens below, queued units become no-ops before the
        // scope joins them.
        let _abort = AbortOnDrop(&shared_ref.abort);
        let mut engine = Engine {
            entries,
            shared: shared_ref,
            tx,
            rx,
            max_probes: threads.min(16),
            batch_files,
            batch_bytes,
            budget: budget_with(threads, codec),
            used: 0,
            in_flight: 0,
            outstanding: 0,
            d_idx: 0,
            d_chunk: 0,
            p_idx: 0,
            probes_ahead: 0,
            probed: HashSet::new(),
            decisions: HashMap::new(),
            ready: Reorder::new(),
            w_idx: 0,
            big: None,
            out,
            out_label,
            hdr: Vec::with_capacity(512),
            cd: Vec::new(),
            cd_count: 0,
            progress,
            files_done: 0,
            files_total,
            bytes_done: 0,
            bytes_total,
            last_report: 0,
            last_report_at: Instant::now(),
            seen_advanced: 0,
            credit_written: 0,
            shown: 0,
            files: 0,
            dirs: 0,
            symlinks: 0,
        };
        engine.run(scope)?;
        engine.finish()
    })?;
    written.summary.elapsed = started.elapsed();
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::CHUNK;

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
        for (given, used) in [(0, 1), (1, 1), (15, 15), (1000, 1000), (u16::MAX, 1000)] {
            assert_eq!(
                effective_method(Method::Zopfli { iterations: given }),
                Method::Zopfli { iterations: used }
            );
        }
        assert_eq!(codec_for(Method::Store), None);
        assert_eq!(codec_for(Method::Deflate { level: 0 }), None);
        assert_eq!(
            codec_for(Method::Deflate { level: 1 }),
            Some(Codec::Deflate(1))
        );
        assert_eq!(
            codec_for(Method::Deflate { level: 8 }),
            Some(Codec::Deflate(8))
        );
        assert_eq!(
            codec_for(Method::Deflate { level: 9 }),
            Some(Codec::Deflate(9))
        );
        assert_eq!(
            codec_for(Method::Deflate { level: 42 }),
            Some(Codec::Deflate(9))
        );
        assert_eq!(codec_for(Method::DeflateBest), Some(Codec::Best));
        assert_eq!(
            codec_for(Method::Zopfli { iterations: 0 }),
            Some(Codec::Zopfli { iterations: 1 })
        );
    }

    #[test]
    fn budget_bounds() {
        assert_eq!(budget_for(1), 64 * MIB);
        assert_eq!(budget_for(4), 64 * MIB);
        assert_eq!(budget_for(8), 128 * MIB);
        assert_eq!(budget_for(64), 512 * MIB);
        assert!(worker_count(0) >= 1);
        assert_eq!(worker_count(3), 3);
        // The largest unit always fits in the smallest budget.
        let codecs = [
            None,
            Some(Codec::Deflate(1)),
            Some(Codec::Best),
            Some(Codec::Zopfli { iterations: 15 }),
        ];
        for c in codecs {
            assert!(unit_cost(BIG_FILE - 1, c) < budget_with(1, c), "{c:?}");
            assert!(unit_cost(CHUNK as u64, c) < budget_with(1, c), "{c:?}");
            // Every worker can hold a chunk unit at once.
            for t in [1, 8, 64] {
                assert!(
                    t as u64 * unit_cost(CHUNK as u64, c) <= budget_with(t, c),
                    "{c:?} {t}"
                );
            }
        }
    }

    #[test]
    fn reorder_buffer_releases_in_key_order() {
        let mut r: Reorder<Key, &str> = Reorder::new();
        r.insert((2, 0), "c");
        r.insert((0, 1), "b");
        assert!(r.take(&(0, 0)).is_none(), "not arrived yet");
        r.insert((0, 0), "a");
        let mut got = Vec::new();
        for k in [(0, 0), (0, 1), (1, 0), (2, 0)] {
            if let Some(v) = r.take(&k) {
                got.push(v);
            }
        }
        assert_eq!(got, ["a", "b", "c"]);
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn out_patches_in_place() {
        let mut o = Out::new(io::Cursor::new(Vec::new())).unwrap();
        o.write(b"hello world").unwrap();
        o.patch(0, b"J").unwrap();
        o.write(b"!").unwrap();
        assert_eq!(o.pos, 12);
        assert_eq!(o.finish().unwrap().into_inner(), b"Jello world!");
    }
}
