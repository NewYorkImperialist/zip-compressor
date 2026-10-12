//! The parallel compression pipeline and the archive writer.
//!
//! # Pipeline
//! The pre-scan (on the calling thread) yields the entries in archive order.
//! Work is cut into *units*, identified by `(entry index, chunk number)`:
//! - a small regular file (< [`codec::BIG_FILE`]) is one unit: a worker opens
//!   it, reads it whole, computes its CRC and its exact deflated size, and
//!   keeps Deflate only if that is strictly smaller than the file (else Store);
//! - a big file is one unit per [`codec::CHUNK`]. Chunk 0 doubles as the
//!   probe: if deflate saves less than 0.5% on it, the whole file is Stored
//!   (video, archives, photos stay fast). Otherwise every chunk is deflated
//!   independently (see [`codec`]); chunks 1.. are dispatched once the probe
//!   result is in, so they know which method to use;
//! - directories and symlinks need no work and are written inline.
//!
//! Units run on a dedicated rayon pool of `threads` workers; the workers do
//! all file reading too (opening many tiny files dominates for source trees).
//! Each unit sends exactly one message back over a channel to the calling
//! thread, which keeps a reorder buffer and writes units strictly in scan
//! order. The calling thread is also the dispatcher, so all scheduling state
//! (budget, cursors, reorder buffer) is single-threaded and lock-free, and the
//! `progress` callback (not `Send`) is only ever called there.
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
//! file needs room for its input and its deflate output). Not counted: one
//! deflate state per busy worker (~300 KB), the 256 KiB output buffer, and the
//! central directory, which is kept in memory (~70 bytes + name per entry).
//!
//! # Determinism
//! Entries are written in scan order whatever order workers finish in; each
//! unit's bytes depend only on its input data, the level and its position
//! (chunk boundaries are fixed by the file size), never on scheduling or the
//! number of threads. The deflate compressor is reset between uses.
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
//! returns (the pool scope joins them, so no work outlives the call).

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::codec::{self, CrcCombiner, BIG_FILE, DICT};
use crate::container::{self, CMethod, EntryType, Meta, Sizes};
use crate::walk::{Entry, Kind};
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
/// How long the writer waits for a result before re-checking `cancel`.
const POLL: Duration = Duration::from_millis(50);

const MIB: u64 = 1 << 20;

/// In-flight byte budget for `threads` workers.
pub(crate) fn budget_for(threads: usize) -> u64 {
    (threads as u64 * 16 * MIB).clamp(64 * MIB, 512 * MIB)
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
/// (deflate-0 is just Store with overhead).
fn effective_method(m: Method) -> Method {
    match m {
        Method::Store => Method::Store,
        Method::Deflate { level } => match level.min(9) {
            0 => Method::Store,
            l => Method::Deflate { level: l },
        },
    }
}

/// Estimated peak buffer use of a unit over `len` input bytes.
fn unit_cost(len: u64, compress: bool) -> u64 {
    if compress {
        2 * len + len / 8 + DICT as u64 + 256
    } else {
        len + 256
    }
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

struct Msg {
    key: Key,
    cost: u64,
    res: Result<Unit>,
}

enum Job {
    Small {
        size: u64,
    },
    Chunk {
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
    /// Deflate level, or `None` for Store.
    level: Option<u32>,
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

/// Read `[start, end)` of a big file.
fn read_range(file: &File, start: u64, end: u64, path: &Path) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; (end - start) as usize];
    read_exact_at(file, &mut buf, start).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            changed_error(path, "file shrank while it was being archived")
        } else {
            Error::Io {
                path: path.to_path_buf(),
                source: e,
            }
        }
    })?;
    Ok(buf)
}

fn shrink(mut v: Vec<u8>) -> Vec<u8> {
    if v.capacity() > v.len() + v.len() / 4 + 4096 {
        v.shrink_to_fit();
    }
    v
}

fn run_small(entry: &Entry, size: u64, level: Option<u32>) -> Result<SmallOut> {
    let io_err = |e| Error::Io {
        path: entry.path.clone(),
        source: e,
    };
    let file = File::open(&entry.path).map_err(io_err)?;
    let mut raw = Vec::with_capacity(size as usize);
    file.take(size).read_to_end(&mut raw).map_err(io_err)?;
    let crc = crc32fast::hash(&raw);
    let size = raw.len() as u64;
    if let Some(level) = level.filter(|_| !raw.is_empty()) {
        let d = codec::deflate(&raw, &[], level, true).map_err(io_err)?;
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
    level: Option<u32>,
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
        // The probe: decides the method for the whole file.
        let raw = read_range(&file, start, end, path)?;
        let crc = crc32fast::hash(&raw);
        let tail = tail_crc(&raw);
        let (method, data) = match level {
            None => (CMethod::Stored, raw),
            Some(level) => {
                let d = codec::deflate(&raw, &[], level, last).map_err(io_err)?;
                if codec::probe_says_deflate(raw.len(), d.len()) {
                    (CMethod::Deflated, shrink(d))
                } else {
                    (CMethod::Stored, raw)
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
            let level = level.expect("deflated chunk without a level");
            let buf = read_range(&file, start - DICT as u64, end, path)?;
            let (dict, input) = buf.split_at(DICT);
            let data = codec::encode_chunk(input, dict, level, last).map_err(io_err)?;
            Ok(ChunkOut {
                raw_len: end - start,
                crc: crc32fast::hash(input),
                dict_crc: crc32fast::hash(dict),
                tail_crc: tail_crc(input),
                data: shrink(data),
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

fn run_job(entry: &Entry, job: Job, level: Option<u32>) -> Result<Unit> {
    match job {
        Job::Small { size } => run_small(entry, size, level).map(Unit::Small),
        Job::Chunk {
            size,
            k,
            method,
            file,
        } => run_chunk(entry, size, k, method, file, level).map(Unit::Chunk),
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
    ready: Reorder<Key, (u64, Result<Unit>)>,

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
        (self.progress)(&Progress {
            current: current.to_string(),
            files_done: self.files_done,
            files_total: self.files_total,
            bytes_done: self.bytes_done,
            bytes_total: self.bytes_total,
        });
        self.check_cancel()
    }

    fn compressing(&self) -> bool {
        self.shared.level.is_some()
    }

    /// Method (and open file) for the chunks of big entry `idx`, if known.
    fn decision(&self, idx: usize) -> Option<Probe> {
        if self.compressing() {
            self.decisions.get(&idx).cloned()
        } else {
            Some(Probe {
                method: CMethod::Stored,
                file: None,
            })
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

    fn spawn(&mut self, scope: &rayon::Scope<'s>, key: Key, cost: u64, job: Job) {
        let tx = self.tx.clone();
        let shared = self.shared;
        let entry = &self.entries[key.0];
        self.in_flight += 1;
        self.outstanding += 1;
        scope.spawn(move |_| {
            let res = if shared.skip(key.0) {
                Err(Error::Cancelled)
            } else {
                std::panic::catch_unwind(AssertUnwindSafe(|| run_job(entry, job, shared.level)))
                    .unwrap_or_else(|_| {
                        Err(Error::Io {
                            path: entry.path.clone(),
                            source: io::Error::other("compression worker panicked"),
                        })
                    })
            };
            // The writer may already have returned; then nobody needs this.
            let _ = tx.send(Msg { key, cost, res });
        });
    }

    /// Hand out as much work as the budget allows, in scan order.
    fn dispatch(&mut self, scope: &rayon::Scope<'s>) {
        let n = self.entries.len();
        let stop = self.shared.stop_after.load(Ordering::Relaxed);
        let compress = self.compressing();
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
                if !self.admit(unit_cost(size, compress), idx == self.w_idx) {
                    break;
                }
                self.spawn(
                    scope,
                    (idx, 0),
                    unit_cost(size, compress),
                    Job::Small { size },
                );
                self.d_idx += 1;
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
                    size,
                    k,
                    method: None,
                    file: None,
                };
                (unit_cost(end - start, compress), job)
            } else {
                let Some(p) = self.decision(idx) else {
                    break; // waiting for the probe
                };
                let deflate = p.method == CMethod::Deflated;
                let job = Job::Chunk {
                    size,
                    k,
                    method: Some(p.method),
                    file: p.file,
                };
                (unit_cost(end - start, deflate), job)
            };
            let next_needed =
                self.w_idx == idx && self.big.as_ref().map_or(0, |b| b.next_chunk) == k;
            if !self.admit(cost, next_needed) {
                break;
            }
            self.spawn(scope, (idx, k), cost, job);
            self.d_chunk += 1;
        }

        // Probe upcoming big files early.
        if !compress {
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
                    let cost = unit_cost(end - start, true);
                    if !self.admit(cost, false) {
                        break;
                    }
                    let job = Job::Chunk {
                        size,
                        k: 0,
                        method: None,
                        file: None,
                    };
                    self.spawn(scope, (idx, 0), cost, job);
                    self.probed.insert(idx);
                    self.probes_ahead += 1;
                }
            }
            self.p_idx += 1;
        }
    }

    fn receive(&mut self, msg: Msg) {
        self.in_flight -= 1;
        self.used -= msg.cost;
        let mut res = msg.res;
        match &mut res {
            Ok(Unit::Chunk(c)) => {
                if let Some(p) = c.probe.take() {
                    self.decisions.insert(msg.key.0, p);
                }
            }
            Ok(Unit::Small(_)) => {}
            Err(Error::Cancelled) => {}
            Err(_) => {
                self.shared
                    .stop_after
                    .fetch_min(msg.key.0, Ordering::Relaxed);
            }
        }
        let held = res.as_ref().map_or(0, Unit::held);
        self.used += held;
        self.ready.insert(msg.key, (held, res));
    }

    /// Take the result for `key` if it has arrived.
    fn take(&mut self, key: Key) -> Option<Result<Unit>> {
        let (held, res) = self.ready.take(&key)?;
        self.used -= held;
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
    let level = match effective_method(opts.method) {
        Method::Store => None,
        Method::Deflate { level } => Some(u32::from(level)),
    };
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
        level,
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
            budget: budget_for(threads),
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
        assert!(unit_cost(BIG_FILE - 1, true) < budget_for(1));
        assert!(unit_cost(CHUNK as u64, true) < budget_for(1));
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
