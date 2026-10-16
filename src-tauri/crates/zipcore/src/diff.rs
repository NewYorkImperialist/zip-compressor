//! `diff_archive`: compare an archive with a directory, like `diff -r`.
//!
//! 1. **Archive side.** The central directory is read (any tool's archive).
//!    Every name is split by [`split_name`], the same normalization
//!    extraction uses; a name that would land outside the directory
//!    (absolute, `..`, a drive letter) is never turned into a disk path but
//!    reported as [`DiffKind::OnlyInArchive`] with a detail saying why. The
//!    prefix (explicit, or detected as `<directory name>/`) is stripped;
//!    entries outside it are not part of the comparison. Directories that
//!    entries imply but the archive doesn't list (many tools write no
//!    directory entries) count as present.
//! 2. **Disk side.** The directory is walked without following symlinks
//!    (a link is compared as a link, by its target); sockets, FIFOs and
//!    devices are skipped, as `compress` skips them.
//! 3. **Filtering.** `exclude_junk` drops OS clutter (the names
//!    `CompressOptions::exclude_junk` drops) on both sides, so an archive
//!    made with clutter, such as macOS's `__MACOSX/`, still matches the tree
//!    it came from. `exclude` patterns are gitignore-style and matched
//!    against the path relative to the compared roots on both sides; an
//!    excluded directory excludes everything beneath it, as in git.
//! 4. **Comparison.** Paths present on both sides are compared by a rayon
//!    pool of `threads` workers, which read the archive through positional
//!    reads on one shared handle:
//!    - kinds differ: [`DiffKind::TypeMismatch`];
//!    - an archive entry with damaged headers, a data span outside the
//!      archive or overlapping another entry's, or (in full mode) damaged
//!      data: [`DiffKind::Different`] with what is wrong;
//!    - files: sizes, then the disk file's CRC-32 against the central
//!      directory's (default), or the decompressed bytes against the file's
//!      (`full`); symlinks: the targets;
//!    - when the contents match, mode and mtime if asked for
//!      ([`DiffKind::MetadataDiffers`]). Symlinks' own mode and mtime are
//!      never compared (extraction doesn't restore them), nor are mode bits
//!      on Windows, which has none.
//!
//! Details read "archive vs disk", e.g. `size 120 vs 118`.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use walkdir::WalkDir;

use crate::extract::split_name;
use crate::filter::{Patterns, TreeFilter};
use crate::reader::{self, extra_fields, CentralEntry};
use crate::timeconv;
use crate::verify::{self, Buffers, Located, Stop, BUF_LEN, PROGRESS_STEP};
use crate::walk::{is_junk, top_level_name};
use crate::writer::worker_count;
use crate::{DiffEntry, DiffKind, DiffOptions, DiffReport, Error, Progress, Result};

/// How often the main thread checks for progress while workers run.
const POLL: Duration = Duration::from_millis(50);
/// Longest symlink target read from the archive (as extraction allows).
const MAX_LINK_TARGET: u64 = 4096;
const METHOD_AES: u16 = 99;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
    Symlink,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Dir => "directory",
            Kind::Symlink => "symlink",
        }
    }
}

/// The archive side of one path.
struct ArcSide {
    /// Index into the central directory; `None` for a directory that is only
    /// implied by entries beneath it.
    entry: Option<usize>,
    kind: Kind,
    /// More than one entry (other than repeated directories) maps here.
    duplicates: usize,
}

/// The disk side of one path.
struct DiskSide {
    path: PathBuf,
    kind: Kind,
    md: fs::Metadata,
}

#[derive(Default)]
struct Slot {
    arc: Option<ArcSide>,
    disk: Option<DiskSide>,
}

/// One path present on both sides with the same kind, to be compared by a worker.
struct Job<'a> {
    /// Index of the result this job fills in.
    out: usize,
    path: String,
    arc: &'a ArcSide,
    disk: &'a DiskSide,
}

/// Everything the workers share.
struct Shared<'a> {
    file: &'a File,
    entries: &'a [CentralEntry],
    /// Per central-directory entry: its located header (or why not), for
    /// entries whose data is compared.
    located: &'a BTreeMap<usize, Located>,
    /// Entries whose data span overlaps an earlier entry's, and which one.
    overlaps: &'a BTreeMap<usize, usize>,
    opts: &'a DiffOptions,
    cancel: &'a AtomicBool,
    next: AtomicUsize,
    bytes_done: AtomicU64,
}

fn io_err(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub(crate) fn diff_archive(
    archive: &Path,
    dir: &Path,
    opts: &DiffOptions,
    progress: &mut dyn FnMut(&Progress),
    cancel: &AtomicBool,
) -> Result<DiffReport> {
    let started = Instant::now();
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let (src, cd) = reader::open_archive(archive)?;
    match fs::metadata(dir) {
        Ok(md) if md.is_dir() => {}
        Ok(_) => {
            return Err(io_err(
                dir,
                io::Error::new(io::ErrorKind::InvalidInput, "not a directory"),
            ))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(Error::NotFound(dir.to_path_buf()))
        }
        Err(e) => return Err(io_err(dir, e)),
    }
    let exclude = Patterns::new(&opts.exclude)?;
    let entries = &cd.entries;

    // Archive names as components; `None` for unsafe ones.
    let windows = cfg!(windows);
    let split: Vec<Option<Vec<&str>>> = entries
        .iter()
        .map(|e| split_name(&e.name, windows))
        .collect();
    let junk = |comps: &[&str]| opts.exclude_junk && comps.iter().any(|c| is_junk(OsStr::new(c)));

    let prefix: Vec<String> = match &opts.prefix {
        // A leading `/` is harmless here: the prefix is always archive-relative.
        Some(p) => split_name(p.trim_start_matches(['/', '\\']), false)
            .ok_or_else(|| Error::UnsafePath(p.clone()))?
            .into_iter()
            .map(String::from)
            .collect(),
        None => {
            let top = top_level_name(dir)?;
            let mut candidates = entries
                .iter()
                .zip(&split)
                .filter_map(|(e, s)| s.as_ref().map(|c| (e, c)))
                .filter(|(_, c)| !junk(c))
                .peekable();
            let under = candidates.peek().is_some()
                && candidates
                    .all(|(e, c)| c.first() == Some(&top.as_str()) && (c.len() > 1 || e.is_dir()));
            // `dir/<top>` existing means `dir` is where the archive was
            // extracted (`extract x.zip -d x` makes `x/x/...`), not the folder
            // that was compressed: compare from the archive root.
            if under && !dir.join(&top).is_dir() {
                vec![top]
            } else {
                Vec::new()
            }
        }
    };

    let mut slots: BTreeMap<String, Slot> = BTreeMap::new();
    let mut results: Vec<DiffEntry> = Vec::new();

    // 1. The archive side.
    for (i, (e, comps)) in entries.iter().zip(&split).enumerate() {
        let Some(comps) = comps else {
            results.push(DiffEntry {
                path: e.name.clone(),
                kind: DiffKind::OnlyInArchive,
                detail: Some("unsafe name (would be outside the directory); not compared".into()),
            });
            continue;
        };
        if comps.len() < prefix.len() || comps.iter().zip(&prefix).any(|(a, b)| a != b) {
            continue;
        }
        let rel = &comps[prefix.len()..];
        if rel.is_empty() {
            continue;
        }
        // The directories above it exist in the archive too, as far down as
        // they are compared at all.
        for depth in 1..rel.len() {
            let above = &rel[..depth];
            if junk(&above[depth - 1..]) || excluded(&exclude, above, true) {
                break;
            }
            slots
                .entry(above.join("/"))
                .or_default()
                .arc
                .get_or_insert(ArcSide {
                    entry: None,
                    kind: Kind::Dir,
                    duplicates: 0,
                });
        }
        let kind = if e.is_dir() {
            Kind::Dir
        } else if e.is_symlink() {
            Kind::Symlink
        } else {
            Kind::File
        };
        if junk(rel) || excluded(&exclude, rel, kind == Kind::Dir) {
            continue;
        }
        let slot = slots.entry(rel.join("/")).or_default();
        match &mut slot.arc {
            Some(a) if a.entry.is_none() => {
                *a = ArcSide {
                    entry: Some(i),
                    kind,
                    duplicates: 0,
                }
            }
            Some(a) if a.kind == Kind::Dir && kind == Kind::Dir => {}
            Some(a) => a.duplicates += 1,
            None => {
                slot.arc = Some(ArcSide {
                    entry: Some(i),
                    kind,
                    duplicates: 0,
                })
            }
        }
    }

    // 2. The disk side.
    walk_disk(dir, opts, &exclude, cancel, &mut slots)?;

    // 3. Pair them up; same-kind pairs become jobs.
    let mut jobs: Vec<Job<'_>> = Vec::new();
    for (path, slot) in &slots {
        let out = results.len();
        let (kind, detail) = match (&slot.arc, &slot.disk) {
            (Some(_), None) => (DiffKind::OnlyInArchive, None),
            (None, Some(_)) => (DiffKind::OnlyOnDisk, None),
            (Some(a), Some(d)) if a.kind != d.kind => (
                DiffKind::TypeMismatch,
                Some(format!(
                    "{} in archive, {} on disk",
                    a.kind.name(),
                    d.kind.name()
                )),
            ),
            (Some(a), Some(_)) if a.duplicates > 0 => (
                DiffKind::Different,
                Some(format!(
                    "the archive has {} entries for this path",
                    a.duplicates + 1
                )),
            ),
            (Some(a), Some(d)) => {
                jobs.push(Job {
                    out,
                    path: path.clone(),
                    arc: a,
                    disk: d,
                });
                // Filled in by the job.
                (DiffKind::Same, None)
            }
            (None, None) => unreachable!("slots are created with one side"),
        };
        let shown = slot
            .arc
            .as_ref()
            .map(|a| a.kind)
            .or(slot.disk.as_ref().map(|d| d.kind));
        results.push(DiffEntry {
            path: display(path, shown == Some(Kind::Dir)),
            kind,
            detail,
        });
    }

    // Locate the data of every entry a job will read, and check for overlaps.
    let mut located: BTreeMap<usize, Located> = BTreeMap::new();
    let mut src = src;
    for job in &jobs {
        if job.arc.kind == Kind::Dir {
            continue;
        }
        let Some(i) = job.arc.entry else { continue };
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let l =
            verify::locate(&mut src, &entries[i], cd.cd_start).map_err(|e| io_err(archive, e))?;
        located.insert(i, l);
    }
    let ids: Vec<usize> = located.keys().copied().collect();
    let spans: Vec<Located> = located.values().cloned().collect();
    let overlaps: BTreeMap<usize, usize> =
        verify::find_overlaps(|k| entries[ids[k]].local_offset, &spans)
            .into_iter()
            .enumerate()
            .filter_map(|(k, o)| o.map(|o| (ids[k], ids[o])))
            .collect();

    // 4. Compare.
    let bytes_total = jobs
        .iter()
        .filter(|j| j.arc.kind == Kind::File)
        .fold(0u64, |acc, j| acc.saturating_add(j.disk.md.len()));
    let mut p = Progress {
        current: String::new(),
        files_done: (results.len() - jobs.len()) as u64,
        files_total: results.len() as u64,
        bytes_done: 0,
        bytes_total,
    };
    let shared = Shared {
        file: &src.file,
        entries,
        located: &located,
        overlaps: &overlaps,
        opts,
        cancel,
        next: AtomicUsize::new(0),
        bytes_done: AtomicU64::new(0),
    };
    let outcomes = run_jobs(&shared, &jobs, archive, &mut p, progress)?;
    for (job, (kind, detail)) in jobs.iter().zip(outcomes) {
        results[job.out].kind = kind;
        results[job.out].detail = detail;
    }
    p.files_done = p.files_total;
    p.bytes_done = p.bytes_done.max(shared.bytes_done.load(Ordering::Relaxed));
    progress(&p);

    results.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DiffReport {
        entries: results,
        prefix: display(&prefix.join("/"), !prefix.is_empty()),
        elapsed: started.elapsed(),
    })
}

/// `path` as reported: directories end in `/`.
fn display(path: &str, dir: bool) -> String {
    if dir && !path.is_empty() {
        format!("{path}/")
    } else {
        path.to_string()
    }
}

/// Whether `rel` (components relative to the compared root) is excluded:
/// it, or one of the directories above it, matches. That is the answer the
/// disk walk gives by not descending into excluded directories, so a
/// `!negation` cannot re-include something on one side only.
fn excluded(exclude: &Patterns, rel: &[&str], is_dir: bool) -> bool {
    exclude.excludes_archive_name(&display(&rel.join("/"), is_dir))
}

fn walk_disk(
    dir: &Path,
    opts: &DiffOptions,
    exclude: &Patterns,
    cancel: &AtomicBool,
    slots: &mut BTreeMap<String, Slot>,
) -> Result<()> {
    // The same filter `compress` applies, minus .gitignore files.
    let mut filter = TreeFilter::new(exclude, opts.exclude_junk, false);
    let mut walker = WalkDir::new(dir)
        .min_depth(1)
        .follow_links(false)
        .follow_root_links(true)
        .sort_by_file_name()
        .into_iter();
    while let Some(item) = walker.next() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let dent = item.map_err(|e| {
            let path = e.path().unwrap_or(dir).to_path_buf();
            Error::Io {
                path,
                source: io::Error::from(e),
            }
        })?;
        let rel = dent.path().strip_prefix(dir).unwrap_or(dent.path());
        let is_dir = dent.file_type().is_dir();
        if !filter.keep(rel, dent.depth(), is_dir) {
            if is_dir {
                walker.skip_current_dir();
            }
            continue;
        }
        if is_dir {
            filter.enter_dir(dent.path(), rel, dent.depth())?;
        }
        let ft = dent.file_type();
        let kind = if ft.is_dir() {
            Kind::Dir
        } else if ft.is_symlink() {
            Kind::Symlink
        } else if ft.is_file() {
            Kind::File
        } else {
            continue;
        };
        // Asked of the entry itself: on Windows the directory listing's
        // metadata can lag (see `walk::entry_metadata`).
        let md = fs::symlink_metadata(dent.path()).map_err(|e| io_err(dent.path(), e))?;
        let key = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        slots.entry(key).or_default().disk = Some(DiskSide {
            path: dent.path().to_path_buf(),
            kind,
            md,
        });
    }
    Ok(())
}

/// Run every job on a pool of workers, reporting progress from this thread.
/// Results come back in job order.
fn run_jobs(
    sh: &Shared<'_>,
    jobs: &[Job<'_>],
    archive: &Path,
    p: &mut Progress,
    progress: &mut dyn FnMut(&Progress),
) -> Result<Vec<(DiffKind, Option<String>)>> {
    let mut out: Vec<Option<(DiffKind, Option<String>)>> = (0..jobs.len()).map(|_| None).collect();
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let threads = worker_count(sh.opts.threads).min(jobs.len());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("zipcore-d{i}"))
        .build()
        .map_err(|e| io_err(archive, io::Error::other(e.to_string())))?;
    let (tx, rx) = mpsc::channel::<(usize, (DiffKind, Option<String>))>();
    pool.in_place_scope(|scope| {
        for _ in 0..threads {
            let tx = tx.clone();
            scope.spawn(move |_| {
                let mut bufs = Buffers::new();
                loop {
                    if sh.cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let i = sh.next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(i) else { return };
                    match compare(sh, job, &mut bufs) {
                        Some(r) => {
                            // The receiver only goes away after every worker is done.
                            let _ = tx.send((i, r));
                        }
                        None => return,
                    }
                }
            });
        }
        drop(tx);
        let mut next_report = PROGRESS_STEP;
        loop {
            let msg = rx.recv_timeout(POLL);
            p.bytes_done = sh.bytes_done.load(Ordering::Relaxed);
            match msg {
                Ok((i, r)) => {
                    out[i] = Some(r);
                    p.files_done += 1;
                    p.current = jobs[i].path.clone();
                    progress(p);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if p.bytes_done >= next_report {
                        progress(p);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if p.bytes_done >= next_report {
                next_report = (p.bytes_done / PROGRESS_STEP + 1).saturating_mul(PROGRESS_STEP);
            }
        }
    });
    if sh.cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    out.into_iter().map(|r| r.ok_or(Error::Cancelled)).collect()
}

/// Compare one path present on both sides with the same kind. `None` means
/// cancelled.
fn compare(
    sh: &Shared<'_>,
    job: &Job<'_>,
    bufs: &mut Buffers,
) -> Option<(DiffKind, Option<String>)> {
    let e = job.arc.entry.map(|i| (i, &sh.entries[i]));
    let different = |m: String| Some((DiffKind::Different, Some(m)));
    let mut note = None;
    match job.arc.kind {
        Kind::Dir => {}
        Kind::File | Kind::Symlink => {
            let (i, e) = e.expect("only directories are implied");
            let data_start = match damage(sh, i, e) {
                Err(m) => return different(format!("archive entry is damaged: {m}")),
                Ok(start) => start,
            };
            if e.method == METHOD_AES {
                return different("AES-encrypted; contents can't be compared".into());
            }
            let r = if job.arc.kind == Kind::Symlink {
                compare_link(sh, e, data_start, &job.disk.path, bufs)
            } else {
                compare_file(sh, e, data_start, job.disk, bufs, &mut note)
            };
            match r {
                Err(Stop::Cancelled) => return None,
                Err(Stop::Io(err)) => {
                    return different(format!("could not read the archive: {err}"))
                }
                Err(Stop::Sink(err)) => {
                    return different(format!("could not read the file: {err}"))
                }
                Ok(Some(m)) => return different(m),
                Ok(None) => {}
            }
        }
    }
    let meta = match e {
        Some((_, e)) if job.arc.kind != Kind::Symlink => {
            metadata_differences(sh.opts, e, &job.disk.md)
        }
        _ => Vec::new(),
    };
    if meta.is_empty() {
        Some((DiffKind::Same, note))
    } else {
        Some((DiffKind::MetadataDiffers, Some(meta.join("; "))))
    }
}

/// The data start of entry `i`, or what is wrong with its headers.
fn damage(sh: &Shared<'_>, i: usize, e: &CentralEntry) -> std::result::Result<u64, String> {
    let mut msgs = e.problems.clone();
    let start = match sh.located.get(&i) {
        Some(Ok((lh, _))) => {
            verify::compare_headers(e, lh, &mut msgs);
            Some(lh.data_start)
        }
        Some(Err(m)) => {
            msgs.push(m.clone());
            None
        }
        None => {
            msgs.push("not located".into());
            None
        }
    };
    if let Some(&o) = sh.overlaps.get(&i) {
        msgs.push(format!(
            "data overlaps entry \"{}\" (possible zip bomb)",
            sh.entries[o].name
        ));
    }
    match start {
        Some(s) if msgs.is_empty() => Ok(s),
        _ => Err(msgs.join("; ")),
    }
}

/// Why the symlink entry's target differs from the link at `path`, if it does.
fn compare_link(
    sh: &Shared<'_>,
    e: &CentralEntry,
    data_start: u64,
    path: &Path,
    bufs: &mut Buffers,
) -> std::result::Result<Option<String>, Stop> {
    if let Err(m) = verify::verifiable(e) {
        return Ok(Some(m));
    }
    if e.uncompressed > MAX_LINK_TARGET {
        return Ok(Some(format!(
            "archive symlink target is {} bytes long",
            e.uncompressed
        )));
    }
    let mut target = Vec::new();
    let r = verify::decode(e, sh.file, data_start, bufs, &mut |chunk| {
        target.extend_from_slice(chunk);
        Ok(())
    })?;
    if let Some(m) = r.mismatch(e) {
        return Ok(Some(format!("archive entry is damaged: {m}")));
    }
    let disk = fs::read_link(path).map_err(Stop::Sink)?;
    let disk = link_bytes(&disk);
    if disk == target {
        return Ok(None);
    }
    Ok(Some(format!(
        "link target `{}` vs `{}`",
        String::from_utf8_lossy(&target),
        String::from_utf8_lossy(&disk)
    )))
}

#[cfg(unix)]
fn link_bytes(target: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    target.as_os_str().as_bytes().to_vec()
}

/// Windows link targets use backslashes; archives use forward slashes.
#[cfg(not(unix))]
fn link_bytes(target: &Path) -> Vec<u8> {
    target.to_string_lossy().replace('\\', "/").into_bytes()
}

/// Why the file entry's contents differ from the file on disk, if they do.
/// `note` is set when the comparison was weaker than asked for.
fn compare_file(
    sh: &Shared<'_>,
    e: &CentralEntry,
    data_start: u64,
    disk: &DiskSide,
    bufs: &mut Buffers,
    note: &mut Option<String>,
) -> std::result::Result<Option<String>, Stop> {
    if e.uncompressed != disk.md.len() {
        return Ok(Some(format!(
            "size {} vs {}",
            e.uncompressed,
            disk.md.len()
        )));
    }
    let mut file = File::open(&disk.path).map_err(Stop::Sink)?;
    if sh.opts.full {
        match verify::verifiable(e) {
            Ok(()) => return compare_bytes(sh, e, data_start, &mut file, bufs),
            Err(m) => *note = Some(format!("{m}; compared by CRC-32 only")),
        }
    }
    let crc = hash_file(sh, &mut file)?;
    Ok((crc != e.crc).then(|| format!("CRC-32 {:08x} vs {crc:08x}", e.crc)))
}

/// CRC-32 of the rest of `file`, counting bytes for progress.
fn hash_file(sh: &Shared<'_>, file: &mut File) -> std::result::Result<u32, Stop> {
    let mut h = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 4 * BUF_LEN];
    loop {
        if sh.cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        let n = match file.read(&mut buf) {
            Ok(0) => return Ok(h.finalize()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Stop::Sink(e)),
        };
        h.update(&buf[..n]);
        sh.bytes_done.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// Decompress the entry and compare it byte for byte with `file` (whose size
/// already matches).
fn compare_bytes(
    sh: &Shared<'_>,
    e: &CentralEntry,
    data_start: u64,
    file: &mut File,
    bufs: &mut Buffers,
) -> std::result::Result<Option<String>, Stop> {
    let mut theirs = vec![0u8; BUF_LEN];
    let mut offset = 0u64;
    let mut differs_at: Option<u64> = None;
    let r = verify::decode(e, sh.file, data_start, bufs, &mut |chunk| {
        if sh.cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        let disk = &mut theirs[..chunk.len()];
        let got = read_full(file, disk).map_err(Stop::Sink)?;
        if let Some(k) = (0..got).find(|&k| chunk[k] != disk[k]) {
            differs_at = Some(offset + k as u64);
        } else if got < chunk.len() {
            differs_at = Some(offset + got as u64);
        }
        if differs_at.is_some() {
            // Stop decoding; told apart from a real failure below.
            return Err(Stop::Sink(io::Error::other("contents differ")));
        }
        offset += chunk.len() as u64;
        sh.bytes_done
            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
        Ok(())
    });
    if let Some(at) = differs_at {
        return Ok(Some(format!("contents differ at byte {at}")));
    }
    let r = r?;
    if let Some(m) = r.mismatch(e) {
        return Ok(Some(format!("archive entry is damaged: {m}")));
    }
    // The file may have grown since it was measured.
    let mut one = [0u8; 1];
    if read_full(file, &mut one).map_err(Stop::Sink)? != 0 {
        return Ok(Some(format!("contents differ at byte {offset}")));
    }
    Ok(None)
}

/// Read until `buf` is full or end of file; the number of bytes read.
fn read_full(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match file.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// Mode and mtime differences between an entry and what is on disk, as
/// asked for in `opts`.
fn metadata_differences(opts: &DiffOptions, e: &CentralEntry, md: &fs::Metadata) -> Vec<String> {
    let mut out = Vec::new();
    if opts.compare_mode {
        if let (Some(a), Some(d)) = (e.unix_mode(), disk_mode(md)) {
            let (a, d) = (a & 0o7777, d & 0o7777);
            if a != d {
                out.push(format!("mode {a:04o} vs {d:04o}"));
            }
        }
    }
    if opts.compare_mtime {
        if let (Some(a), Ok(d)) = (e.modified(), md.modified()) {
            let exact = extra_fields(&e.extra).any(|(id, data)| {
                id == timeconv::EXT_TIMESTAMP_ID && timeconv::ext_timestamp_mtime(data).is_some()
            });
            let same = if exact {
                timeconv::unix_secs(a) == timeconv::unix_secs(d)
            } else {
                // DOS times have 2-second steps; writers round either way.
                (secs_f64(a) - secs_f64(d)).abs() < 2.0
            };
            if !same {
                out.push(format!("mtime {} vs {}", rfc3339(a), rfc3339(d)));
            }
        }
    }
    out
}

#[cfg(unix)]
fn disk_mode(md: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(md.permissions().mode())
}

/// Windows has no permission bits to compare.
#[cfg(not(unix))]
fn disk_mode(_md: &fs::Metadata) -> Option<u32> {
    None
}

fn secs_f64(t: SystemTime) -> f64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64(),
        Err(e) => -e.duration().as_secs_f64(),
    }
}

/// `t` as RFC 3339 UTC, to the second.
fn rfc3339(t: SystemTime) -> String {
    let c = timeconv::civil_from_unix_utc(timeconv::unix_secs(t));
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        c.year, c.month, c.day, c.hour, c.minute, c.second
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_marks_directories() {
        assert_eq!(display("a/b", true), "a/b/");
        assert_eq!(display("a/b", false), "a/b");
        assert_eq!(display("", true), "");
    }

    #[test]
    fn rfc3339_format() {
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(86_400 + 61)),
            "1970-01-02T00:01:01Z"
        );
    }
}
