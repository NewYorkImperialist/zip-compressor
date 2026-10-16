//! `extract_archive`: safe, verified extraction.
//!
//! Extraction is where "zip slip" bugs live, so the work is split into a
//! *plan* that reads the archive and the destination but writes nothing, and
//! a *write* phase that only starts once the whole plan checks out:
//!
//! 1. **Names.** Every entry's name (not just the selected ones) is split
//!    into components by [`split_name`] and refused with
//!    [`Error::UnsafePath`] if it is absolute, has a drive letter or a `..`
//!    component, or contains NUL. Backslashes count as separators (see
//!    [`split_name`] for why). Empty and `.` components are dropped.
//! 2. **Selection.** `only` patterns pick entries by exact path or directory
//!    prefix; a pattern that matches nothing is [`Error::NotFound`]. Symlink
//!    entries are dropped when `symlinks` is off, and always on non-Unix
//!    systems.
//! 3. **Consistency.** Two entries for the same path are refused with
//!    [`Error::Corrupt`] (except repeated directory entries, which merge), as
//!    is a file that other entries need as a directory. On macOS and Windows,
//!    whose file systems normally ignore case, names that differ only in case
//!    count as the same path. An entry beneath a symlink entry would be
//!    written through the link: [`Error::UnsafePath`].
//! 4. **Data.** Encrypted entries and methods other than Stored/Deflate are
//!    [`Error::Unsupported`]; damaged headers, data spans outside the archive
//!    and spans overlapping another entry's (non-recursive zip bombs) are
//!    [`Error::Corrupt`]. Symlink targets are read and checked here, so an
//!    escaping link is refused before anything is written.
//! 5. **Destination.** Every path is `lstat`ed component by component under
//!    `dest`: an existing symlink in the way of any entry is
//!    [`Error::UnsafePath`]; existing files are handled per [`Overwrite`]
//!    (under `Never` the first one is [`Error::AlreadyExists`]).
//!
//! Then, writing:
//!
//! 6. Directories are created in path order, one component at a time, each
//!    checked to be a real directory.
//! 7. Files are written by a rayon pool of `threads` workers, which read the
//!    archive through positional reads on one shared handle. Each file goes
//!    to a hidden temporary name in its own directory, is checked against the
//!    central directory's CRC-32 and size, gets its permissions and mtime,
//!    and only then is renamed over the final name (atomically replacing an
//!    existing file under `Always`). A damaged entry, an I/O error or a
//!    cancel removes the temporary file, so no partial or bad file is left.
//! 8. Symlinks are created last (same temp-and-rename), so no file can be
//!    written through one even if a check above were wrong.
//! 9. Directory permissions and mtimes are applied last, deepest first:
//!    writing into a directory changes its mtime, and a read-only directory
//!    could not be written into.
//!
//! Restoring permissions and mtimes is best effort (like Info-ZIP's `unzip`):
//! a file system that can't store them (FAT, some network mounts) doesn't
//! fail the extraction.
//!
//! Out of scope: another process changing `dest` while extraction runs. The
//! plan's checks are not repeated per file (only directory creation re-checks
//! what it finds), so such a race could defeat them; extracting into a
//! directory an attacker can write to concurrently is unsafe with any tool.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use crate::reader::{self, CentralEntry, FLAG_ENCRYPTED, FLAG_STRONG_ENCRYPTION};
use crate::verify::{self, Buffers, Stop, PROGRESS_STEP};
use crate::writer::worker_count;
use crate::{Error, ExtractOptions, ExtractSummary, Overwrite, Progress, Result};

/// Longest symlink target accepted (Linux `PATH_MAX`; macOS allows 1024).
const MAX_LINK_TARGET: u64 = 4096;
/// How often the main thread checks for byte progress while workers run.
const POLL: Duration = Duration::from_millis(50);
const METHOD_AES: u16 = 99;

/// Names that differ only in case are the same file on the default file
/// systems of macOS (APFS, HFS+) and Windows (NTFS), so on those platforms
/// they are treated as one path when looking for duplicates and for entries
/// beneath symlinks. (A case-sensitive volume there gets the stricter check;
/// Unicode normalization differences are not folded.)
const CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

/// Whether a symlink entry can be created on this platform at all. Windows
/// symlinks need privileges or Developer Mode and differ for files and
/// directories, so there they are always skipped.
const CAN_SYMLINK: bool = cfg!(unix);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Dir,
    Symlink,
}

/// One selected entry, with its validated relative path.
struct Item<'a> {
    e: &'a CentralEntry,
    kind: Kind,
    comps: Vec<&'a str>,
    /// Path under `dest`.
    path: PathBuf,
    /// Normalized target, for symlinks (filled in by the data checks).
    target: Vec<u8>,
    /// Data offset in the archive (files).
    data_start: u64,
    /// Left alone under [`Overwrite::Skip`].
    skip: bool,
}

/// Split an entry name into the components it will be written under, or
/// `None` if it is unsafe: NUL anywhere; absolute (`/x`, `\x`, `//server`);
/// a drive letter (`C:x`, `C:\x`); a `..` component anywhere (even one that
/// would stay inside, like `a/../b`: no legitimate writer produces them).
/// Empty components (`a//b`, a trailing `/`) and `.` are dropped, so the
/// result may be empty (the destination itself).
///
/// **Backslashes are separators**, on every platform. Windows tools have
/// long written them (old .NET `ZipFile`, PowerShell 5's Compress-Archive,
/// some Java tools), and Windows itself would treat them as separators when
/// writing, so a name like `..\evil` must be refused everywhere; treating
/// them the same on Unix makes such archives extract to the same tree on
/// every system, as 7-Zip and libarchive do. The cost is that a Unix file
/// whose name contains a literal backslash is extracted as nested folders.
///
/// With `windows` (the target is Windows), components are also refused if
/// they contain `:` (an NTFS alternate data stream, or a drive-relative
/// path), end in `.` or space (Windows strips those, so `.. ` would become
/// `..`), or are a reserved device name (`CON`, `NUL.txt`, `COM1`, ...),
/// which would open the device instead of a file.
pub(crate) fn split_name(name: &str, windows: bool) -> Option<Vec<&str>> {
    if name.contains('\0') || name.starts_with(['/', '\\']) || has_drive_prefix(name.as_bytes()) {
        return None;
    }
    let mut out = Vec::new();
    for c in name.split(['/', '\\']) {
        match c {
            "" | "." => {}
            ".." => return None,
            c if windows && !windows_component_ok(c) => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

/// `X:` at the start: a drive letter (absolute or drive-relative on Windows).
fn has_drive_prefix(b: &[u8]) -> bool {
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

fn windows_component_ok(c: &str) -> bool {
    if c.contains(':') || c.ends_with(['.', ' ']) {
        return false;
    }
    // The device names apply with any extension and trailing spaces
    // (`nul.txt`, `CON .x`), and to COM/LPT with superscript digits.
    let stem = c.split('.').next().unwrap_or(c).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    let reserved = matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ((upper.starts_with("COM") || upper.starts_with("LPT"))
        && matches!(
            &upper[3..],
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        ));
    !reserved
}

/// Normalize a symlink target and check that it stays inside the
/// destination, or `None`. `depth` is the number of directories between the
/// destination and the link (0 for a link at the top level).
///
/// The target must be relative, and `..` components may only come first:
/// they climb out of the link's own directory, which is a real directory
/// this extraction made (the checks guarantee nothing above an entry is a
/// symlink), so `k` leading `..` from depth `d` land inside when `k <= d`.
/// After that the target only descends, through real directories or through
/// other links that were checked the same way, so it can never climb out. A
/// `..` after a name (`x/../..`) is refused because `x` may itself be a link
/// (to `.`, say), and then lexical resolution would be wrong.
///
/// Returns the target to create: `../` repeated, then the names joined with
/// `/` (backslashes are separators here too, as in names), or `.`.
pub(crate) fn check_link_target(target: &[u8], depth: usize) -> Option<Vec<u8>> {
    if target.is_empty()
        || target.contains(&0)
        || matches!(target[0], b'/' | b'\\')
        || has_drive_prefix(target)
    {
        return None;
    }
    let mut ups = 0usize;
    let mut names: Vec<&[u8]> = Vec::new();
    for c in target.split(|&b| b == b'/' || b == b'\\') {
        match c {
            b"" | b"." => {}
            b".." if names.is_empty() => ups += 1,
            b".." => return None,
            c => names.push(c),
        }
    }
    if ups > depth {
        return None;
    }
    let mut out = b"../".repeat(ups);
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(n);
    }
    if out.last() == Some(&b'/') {
        out.pop();
    }
    if out.is_empty() {
        out.push(b'.');
    }
    Some(out)
}

fn fold(s: &str) -> String {
    if CASE_INSENSITIVE {
        s.to_lowercase()
    } else {
        s.to_owned()
    }
}

fn io_err(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn corrupt(e: &CentralEntry, reason: impl Into<String>) -> Error {
    Error::Corrupt {
        entry: e.name.clone(),
        reason: reason.into(),
    }
}

/// `Err` with the reason when an entry's data can't be extracted (the same
/// rules as [`verify::verifiable`], worded for extraction).
fn check_supported(e: &CentralEntry) -> Result<()> {
    let reason =
        if e.method == METHOD_AES || e.flags & (FLAG_ENCRYPTED | FLAG_STRONG_ENCRYPTION) != 0 {
            "it is encrypted (password-protected archives are not supported)".to_string()
        } else if verify::verifiable(e).is_err() {
            format!(
                "compression method {} ({}) is not supported; only Stored and Deflate are",
                e.method,
                verify::method_name(e.method)
            )
        } else {
            return Ok(());
        };
    Err(Error::Unsupported {
        entry: e.name.clone(),
        reason,
    })
}

fn kind_of(e: &CentralEntry) -> Kind {
    if e.name.ends_with(['/', '\\']) {
        Kind::Dir
    } else if e.is_symlink() {
        Kind::Symlink
    } else {
        Kind::File
    }
}

fn join(dest: &Path, comps: &[&str]) -> PathBuf {
    let mut p = dest.to_path_buf();
    p.extend(comps);
    p
}

/// What is at a path in the destination (not following symlinks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    Missing,
    Dir,
    Symlink,
    /// A file or anything else that isn't a directory.
    File,
}

fn lstat(path: &Path) -> Result<Found> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Ok(Found::Symlink),
        Ok(m) if m.is_dir() => Ok(Found::Dir),
        Ok(_) => Ok(Found::File),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Found::Missing),
        Err(e) => Err(io_err(path, e)),
    }
}

pub(crate) fn extract_archive(
    path: &Path,
    dest: &Path,
    opts: &ExtractOptions,
    progress: &mut dyn FnMut(&Progress),
    cancel: &AtomicBool,
) -> Result<ExtractSummary> {
    let started = Instant::now();
    let cancelled = || cancel.load(Ordering::Relaxed);
    if cancelled() {
        return Err(Error::Cancelled);
    }
    let (mut src, dir) = reader::open_archive(path)?;

    let mut items = plan_names(&dir.entries, dest, opts)?;
    check_consistency(&mut items)?;
    check_data(path, &mut src, &dir, &mut items, cancel)?;
    if cancelled() {
        return Err(Error::Cancelled);
    }
    let mut trie = Trie::new(false);
    let remove = check_destination(dest, opts.overwrite, &mut items, &mut trie)?;

    // --- Writing starts here. ---
    let mut summary = ExtractSummary {
        files: 0,
        dirs: 0,
        symlinks: 0,
        skipped: 0,
        bytes_out: 0,
        elapsed: Duration::ZERO,
    };
    summary.skipped = items
        .iter()
        .filter(|i| i.skip && i.kind != Kind::Dir)
        .count() as u64;
    let live: Vec<&Item> = items.iter().filter(|i| !i.skip).collect();
    let files: Vec<&Item> = live
        .iter()
        .copied()
        .filter(|i| i.kind == Kind::File)
        .collect();
    let links: Vec<&Item> = live
        .iter()
        .copied()
        .filter(|i| i.kind == Kind::Symlink)
        .collect();
    let dirs: Vec<&Item> = live
        .iter()
        .copied()
        .filter(|i| i.kind == Kind::Dir)
        .collect();

    let mut p = Progress {
        current: String::new(),
        files_done: 0,
        files_total: (files.len() + links.len()) as u64,
        bytes_done: 0,
        bytes_total: files
            .iter()
            .fold(0u64, |a, i| a.saturating_add(i.e.uncompressed)),
    };

    create_dirs(dest, &live, &mut trie, &remove, cancel)?;
    summary.dirs = dirs.len() as u64;

    write_files(
        path,
        &src.file,
        &files,
        opts,
        &mut p,
        progress,
        cancel,
        &mut summary,
    )?;

    for item in &links {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        make_symlink(item, opts)?;
        summary.symlinks += 1;
        p.files_done += 1;
        p.current = item.e.name.clone();
        progress(&p);
    }

    if opts.restore_permissions || opts.restore_mtime {
        // Deepest first, so a parent is still writable (and its mtime not yet
        // set) while its children are handled.
        let mut ordered = dirs.clone();
        ordered.sort_by_key(|i| std::cmp::Reverse(i.comps.len()));
        for item in ordered {
            if cancelled() {
                return Err(Error::Cancelled);
            }
            restore_dir_attrs(item, opts);
        }
    }

    summary.elapsed = started.elapsed();
    Ok(summary)
}

/// Steps 1 and 2: validate every name, then keep the selected entries.
fn plan_names<'a>(
    entries: &'a [CentralEntry],
    dest: &Path,
    opts: &ExtractOptions,
) -> Result<Vec<Item<'a>>> {
    let mut all = Vec::with_capacity(entries.len());
    for e in entries {
        let comps =
            split_name(&e.name, cfg!(windows)).ok_or_else(|| Error::UnsafePath(e.name.clone()))?;
        let kind = kind_of(e);
        if comps.is_empty() && kind != Kind::Dir {
            // A file named "" or "./": it would replace the destination itself.
            return Err(Error::UnsafePath(e.name.clone()));
        }
        all.push((e, kind, comps));
    }

    let patterns: Vec<(&String, Vec<&str>)> = opts
        .only
        .iter()
        .map(|p| {
            let comps = p
                .split(['/', '\\'])
                .filter(|c| !c.is_empty() && *c != ".")
                .collect();
            (p, comps)
        })
        .collect();
    let mut matched = vec![false; patterns.len()];

    let mut items = Vec::new();
    for (e, kind, comps) in all {
        let selected = patterns.is_empty() || {
            let mut any = false;
            for (k, (_, pat)) in patterns.iter().enumerate() {
                if comps.len() >= pat.len() && comps[..pat.len()] == pat[..] {
                    matched[k] = true;
                    any = true;
                }
            }
            any
        };
        // The destination itself needs nothing; links may be off.
        if !selected
            || comps.is_empty()
            || (kind == Kind::Symlink && !(opts.symlinks && CAN_SYMLINK))
        {
            continue;
        }
        items.push(Item {
            e,
            kind,
            path: join(dest, &comps),
            comps,
            target: Vec::new(),
            data_start: 0,
            skip: false,
        });
    }
    if let Some(k) = matched.iter().position(|m| !m) {
        return Err(Error::NotFound(PathBuf::from(patterns[k].0)));
    }
    Ok(items)
}

/// Interns paths component by component, giving every prefix of every path
/// a small id in time linear in the name's length.
///
/// Names may be 65,535 bytes long, so tens of thousands of components deep;
/// keying maps or sorted sets by each prefix as a string or slice would cost
/// time proportional to depth x length per entry, which a hostile archive
/// could make enormous. Here each component is hashed once.
struct Trie {
    edges: HashMap<(usize, String), usize>,
    len: usize,
    folded: bool,
}

impl Trie {
    /// The destination itself.
    const ROOT: usize = 0;

    /// `folded`: compare components the way this platform's file system
    /// does by default ([`fold`]); otherwise exactly.
    fn new(folded: bool) -> Self {
        Trie {
            edges: HashMap::new(),
            len: 1,
            folded,
        }
    }

    /// Id of the path `parent`/`comp`.
    fn child(&mut self, parent: usize, comp: &str) -> usize {
        let key = (
            parent,
            if self.folded {
                fold(comp)
            } else {
                comp.to_owned()
            },
        );
        let next = self.len;
        let id = *self.edges.entry(key).or_insert(next);
        if id == next {
            self.len += 1;
        }
        id
    }
}

/// Step 3: duplicates, files that are also directories, and entries beneath
/// symlink entries. Repeated directory entries are dropped (the first one's
/// attributes are restored).
fn check_consistency(items: &mut Vec<Item<'_>>) -> Result<()> {
    /// What the archive says about one (folded) path.
    #[derive(Default, Clone, Copy)]
    struct Node<'e> {
        /// The first entry for this path.
        entry: Option<(Kind, &'e CentralEntry)>,
        /// The first entry beneath this path.
        beneath: Option<&'e CentralEntry>,
    }
    let mut trie = Trie::new(CASE_INSENSITIVE);
    let mut nodes: Vec<Node> = vec![Node::default()];
    let mut keep = vec![true; items.len()];
    for (i, item) in items.iter().enumerate() {
        let (last, parents) = item.comps.split_last().expect("items have a name");
        let mut at = Trie::ROOT;
        for c in parents {
            at = trie.child(at, c);
            nodes.resize(trie.len, Node::default());
            let n = &mut nodes[at];
            n.beneath.get_or_insert(item.e);
            if let Some((kind, e)) = n.entry {
                through(kind, e, item.e)?;
            }
        }
        at = trie.child(at, last);
        nodes.resize(trie.len, Node::default());
        let n = &mut nodes[at];
        if let Some((kind, first)) = n.entry {
            if kind == Kind::Dir && item.kind == Kind::Dir {
                keep[i] = false;
                continue;
            }
            let how = if first.name.trim_end_matches(['/', '\\'])
                == item.e.name.trim_end_matches(['/', '\\'])
            {
                String::new()
            } else {
                format!(
                    " (`{}` and `{}` are the same path here)",
                    first.name, item.e.name
                )
            };
            return Err(corrupt(
                item.e,
                format!("the archive has more than one entry for this path{how}"),
            ));
        }
        n.entry = Some((item.kind, item.e));
        if let Some(child) = n.beneath {
            through(item.kind, item.e, child)?;
        }
    }
    let mut keep = keep.into_iter();
    items.retain(|_| keep.next().unwrap_or(true));
    Ok(())
}

/// `child` lies beneath the entry `e` of kind `kind`: fine for a directory;
/// writing through a symlink entry; a file that would have to be a directory.
fn through(kind: Kind, e: &CentralEntry, child: &CentralEntry) -> Result<()> {
    match kind {
        Kind::Dir => Ok(()),
        Kind::Symlink => Err(Error::UnsafePath(child.name.clone())),
        Kind::File => Err(corrupt(
            e,
            format!(
                "it is a file, but `{}` needs it to be a directory",
                child.name
            ),
        )),
    }
}

/// Step 4: everything about the entries' data that can be checked without
/// writing: support, header damage, data spans, overlaps, symlink targets.
fn check_data(
    archive: &Path,
    src: &mut reader::Source,
    dir: &reader::Directory,
    items: &mut [Item<'_>],
    cancel: &AtomicBool,
) -> Result<()> {
    // Locate every selected file's and link's data, in the shape
    // `find_overlaps` wants (one slot per item; directories take no part).
    let mut located = Vec::with_capacity(items.len());
    for item in items.iter() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let e = item.e;
        if item.kind == Kind::Dir {
            located.push(Err(String::new()));
            continue;
        }
        check_supported(e)?;
        if !e.problems.is_empty() {
            return Err(corrupt(e, e.problems.join("; ")));
        }
        let l = verify::locate(src, e, dir.cd_start).map_err(|err| io_err(archive, err))?;
        match &l {
            Err(m) => return Err(corrupt(e, m.clone())),
            Ok((lh, _)) => {
                let mut msgs = Vec::new();
                verify::compare_headers(e, lh, &mut msgs);
                if !msgs.is_empty() {
                    return Err(corrupt(e, msgs.join("; ")));
                }
            }
        }
        located.push(l);
    }
    let overlaps = verify::find_overlaps(|i| items[i].e.local_offset, &located);
    for (i, o) in overlaps.into_iter().enumerate() {
        if let Some(o) = o {
            return Err(corrupt(
                items[i].e,
                format!(
                    "its data overlaps entry `{}` (possible zip bomb)",
                    items[o].e.name
                ),
            ));
        }
    }
    let mut bufs = None;
    for (item, l) in items.iter_mut().zip(located) {
        let Ok((lh, _)) = l else { continue };
        item.data_start = lh.data_start;
        if item.kind == Kind::Symlink {
            let e = item.e;
            if e.uncompressed > MAX_LINK_TARGET {
                return Err(corrupt(
                    e,
                    format!(
                        "symlink target is {} bytes long (at most {MAX_LINK_TARGET} allowed)",
                        e.uncompressed
                    ),
                ));
            }
            let mut target = Vec::new();
            let bufs = bufs.get_or_insert_with(Buffers::new);
            let r = verify::decode(e, &src.file, lh.data_start, bufs, &mut |chunk| {
                target.extend_from_slice(chunk);
                Ok(())
            });
            let r = match r {
                Ok(r) => r,
                Err(Stop::Io(err) | Stop::Sink(err)) => return Err(io_err(archive, err)),
                Err(Stop::Cancelled) => return Err(Error::Cancelled),
            };
            if let Some(m) = r.mismatch(e) {
                return Err(corrupt(e, m));
            }
            item.target = check_link_target(&target, item.comps.len() - 1)
                .ok_or_else(|| Error::UnsafePath(e.name.clone()))?;
        }
    }
    Ok(())
}

/// Step 5: what already exists under `dest`. Marks items to skip under
/// [`Overwrite::Skip`] and returns the files to delete (to make way for a
/// directory) under [`Overwrite::Always`], as ids in `trie`. The trie
/// compares names exactly, so every distinct spelling is looked up: on a
/// case-insensitive file system each reports what it really reaches.
fn check_destination(
    dest: &Path,
    overwrite: Overwrite,
    items: &mut [Item<'_>],
    trie: &mut Trie,
) -> Result<HashSet<usize>> {
    let mut remove = HashSet::new();
    match lstat(dest)? {
        Found::Missing => return Ok(remove),
        Found::Dir => {}
        // The destination itself may be a symlink to a directory: that is
        // the caller's choice, not the archive's.
        Found::Symlink if dest.is_dir() => {}
        Found::Symlink | Found::File => {
            return Err(io_err(
                dest,
                io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "the destination is not a directory",
                ),
            ))
        }
    }
    let mut cache: HashMap<usize, Found> = HashMap::new();
    'items: for item in items.iter_mut() {
        let n = item.comps.len();
        let mut path = dest.to_path_buf();
        let mut at = Trie::ROOT;
        for (k, c) in item.comps.iter().enumerate() {
            path.push(c);
            at = trie.child(at, c);
            let found = if remove.contains(&at) {
                Found::Missing
            } else if let Some(f) = cache.get(&at) {
                *f
            } else {
                let f = lstat(&path)?;
                cache.insert(at, f);
                f
            };
            let last = k + 1 == n;
            let wants_dir = !last || item.kind == Kind::Dir;
            match (found, wants_dir) {
                (Found::Missing, _) => continue 'items,
                (Found::Dir, true) => {}
                (Found::Symlink, true) => return Err(Error::UnsafePath(item.e.name.clone())),
                // A file where a directory is needed.
                (Found::File, true) => match overwrite {
                    Overwrite::Never => return Err(Error::AlreadyExists(path)),
                    Overwrite::Skip => {
                        item.skip = true;
                        continue 'items;
                    }
                    Overwrite::Always => {
                        remove.insert(at);
                        continue 'items;
                    }
                },
                // Something where the file or link goes.
                (found, false) => match overwrite {
                    Overwrite::Never => return Err(Error::AlreadyExists(path)),
                    Overwrite::Skip => item.skip = true,
                    Overwrite::Always if found == Found::Dir => {
                        return Err(io_err(
                            &path,
                            io::Error::new(
                                io::ErrorKind::IsADirectory,
                                format!(
                                    "a directory is in the way of `{}`; not replacing a \
                                     directory with a file",
                                    item.e.name
                                ),
                            ),
                        ))
                    }
                    // Replaced by the rename.
                    Overwrite::Always => {}
                },
            }
        }
    }
    Ok(remove)
}

/// Step 6: `dest`, then every directory the live items need, parents first
/// (each item's path is walked from the top, creating what is new).
fn create_dirs(
    dest: &Path,
    live: &[&Item<'_>],
    trie: &mut Trie,
    remove: &HashSet<usize>,
    cancel: &AtomicBool,
) -> Result<()> {
    fs::create_dir_all(dest).map_err(|e| io_err(dest, e))?;
    let mut made: HashSet<usize> = HashSet::new();
    for item in live {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let n = item.comps.len();
        let upto = if item.kind == Kind::Dir { n } else { n - 1 };
        let mut path = dest.to_path_buf();
        let mut at = Trie::ROOT;
        for c in &item.comps[..upto] {
            path.push(c);
            at = trie.child(at, c);
            if !made.insert(at) {
                continue;
            }
            if remove.contains(&at) {
                fs::remove_file(&path).map_err(|e| io_err(&path, e))?;
            }
            match fs::create_dir(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // Checked in the plan, but make sure nothing turned it
                    // into a link since: the next steps write inside it.
                    if lstat(&path)? != Found::Dir {
                        return Err(Error::UnsafePath(item.e.name.clone()));
                    }
                }
                Err(e) => return Err(io_err(&path, e)),
            }
        }
    }
    Ok(())
}

/// Shared by the file workers.
struct Shared<'a> {
    archive: &'a Path,
    file: &'a File,
    jobs: &'a [&'a Item<'a>],
    opts: &'a ExtractOptions,
    next: AtomicUsize,
    cancel: &'a AtomicBool,
    /// Set on the first failure so the other workers stop.
    abort: AtomicBool,
    failure: Mutex<Option<Error>>,
    bytes_done: AtomicU64,
}

impl Shared<'_> {
    fn stopped(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || self.abort.load(Ordering::Relaxed)
    }

    /// Record `err` if it is the first, and stop everyone.
    fn fail(&self, err: Error) {
        let mut slot = self.failure.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(err);
        }
        self.abort.store(true, Ordering::Relaxed);
    }
}

/// Step 7: write the files on a worker pool, reporting progress from this
/// thread (the callback isn't `Send`).
#[allow(clippy::too_many_arguments)]
fn write_files(
    archive: &Path,
    file: &File,
    jobs: &[&Item<'_>],
    opts: &ExtractOptions,
    p: &mut Progress,
    progress: &mut dyn FnMut(&Progress),
    cancel: &AtomicBool,
    summary: &mut ExtractSummary,
) -> Result<()> {
    if jobs.is_empty() {
        return Ok(());
    }
    let threads = worker_count(opts.threads).min(jobs.len());
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("zipcore-x{i}"))
        .build()
        .map_err(|e| io_err(archive, io::Error::other(e.to_string())))?;
    let shared = Shared {
        archive,
        file,
        jobs,
        opts,
        next: AtomicUsize::new(0),
        cancel,
        abort: AtomicBool::new(false),
        failure: Mutex::new(None),
        bytes_done: AtomicU64::new(0),
    };
    let (tx, rx) = mpsc::channel::<(usize, u64)>();
    let shared_ref = &shared;
    pool.in_place_scope(|scope| {
        for _ in 0..threads {
            let tx = tx.clone();
            scope.spawn(move |_| worker(shared_ref, &tx));
        }
        drop(tx);
        let mut next_report = PROGRESS_STEP;
        loop {
            let msg = rx.recv_timeout(POLL);
            p.bytes_done = shared_ref.bytes_done.load(Ordering::Relaxed);
            match msg {
                Ok((i, bytes)) => {
                    summary.files += 1;
                    summary.bytes_out += bytes;
                    p.files_done += 1;
                    p.current = jobs[i].e.name.clone();
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
    let failure = shared
        .failure
        .into_inner()
        .unwrap_or_else(|p| p.into_inner());
    match failure {
        Some(e) => Err(e),
        None if cancel.load(Ordering::Relaxed) => Err(Error::Cancelled),
        None => Ok(()),
    }
}

fn worker(sh: &Shared<'_>, tx: &mpsc::Sender<(usize, u64)>) {
    let mut bufs = Buffers::new();
    loop {
        if sh.stopped() {
            return;
        }
        let i = sh.next.fetch_add(1, Ordering::Relaxed);
        let Some(job) = sh.jobs.get(i) else { return };
        match write_file(sh, job, &mut bufs) {
            Ok(bytes) => {
                // The receiver only goes away after every worker is done.
                let _ = tx.send((i, bytes));
            }
            Err(None) => return,
            Err(Some(e)) => {
                sh.fail(e);
                return;
            }
        }
    }
}

/// A temporary file (or link) that is deleted unless it was renamed into
/// place.
struct Temp {
    path: PathBuf,
    armed: bool,
}

impl Temp {
    /// Rename over `dest` (atomically replacing what is there); on failure
    /// the temporary is still removed on drop.
    fn persist(mut self, dest: &Path) -> Result<()> {
        fs::rename(&self.path, dest).map_err(|e| io_err(dest, e))?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh hidden name in `dir`. It doesn't derive from the entry's name, so
/// it can't exceed the file system's name length limit.
fn temp_path(dir: &Path) -> PathBuf {
    let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".zipc-{}-{n}.partial", std::process::id()))
}

/// Run `create` on fresh temporary names in `dir` until one doesn't exist
/// (`create` must fail with `AlreadyExists` rather than follow or replace).
fn with_temp<T>(dir: &Path, mut create: impl FnMut(&Path) -> io::Result<T>) -> Result<(Temp, T)> {
    let mut last = None;
    for _ in 0..64 {
        let p = temp_path(dir);
        match create(&p) {
            Ok(v) => {
                let temp = Temp {
                    path: p,
                    armed: true,
                };
                return Ok((temp, v));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(io_err(&p, e)),
        }
    }
    Err(io_err(
        dir,
        last.unwrap_or_else(|| io::Error::other("no free temporary name")),
    ))
}

fn parent_of(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new("."))
}

/// Extract one file. `Err(None)`: stopped because of a cancel or another
/// worker's failure (nothing to report).
fn write_file(
    sh: &Shared<'_>,
    item: &Item<'_>,
    bufs: &mut Buffers,
) -> std::result::Result<u64, Option<Error>> {
    let e = item.e;
    let (temp, mut out) = with_temp(parent_of(&item.path), |p| {
        OpenOptions::new().write(true).create_new(true).open(p)
    })?;
    let r = verify::decode(e, sh.file, item.data_start, bufs, &mut |chunk| {
        out.write_all(chunk).map_err(Stop::Sink)?;
        sh.bytes_done
            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
        if sh.stopped() {
            return Err(Stop::Cancelled);
        }
        Ok(())
    });
    let r = match r {
        Ok(r) => r,
        Err(Stop::Io(err)) => return Err(Some(io_err(sh.archive, err))),
        Err(Stop::Sink(err)) => return Err(Some(io_err(&item.path, err))),
        Err(Stop::Cancelled) if sh.cancel.load(Ordering::Relaxed) => {
            return Err(Some(Error::Cancelled))
        }
        Err(Stop::Cancelled) => return Err(None),
    };
    if let Some(m) = r.mismatch(e) {
        return Err(Some(corrupt(e, m)));
    }
    if sh.opts.restore_permissions {
        set_file_mode(&out, e);
    }
    if sh.opts.restore_mtime {
        if let Some(t) = e.modified() {
            let _ = out.set_modified(t);
        }
    }
    drop(out);
    temp.persist(&item.path)?;
    #[cfg(not(unix))]
    if sh.opts.restore_permissions && read_only(e) {
        if let Ok(m) = fs::metadata(&item.path) {
            let mut perm = m.permissions();
            perm.set_readonly(true);
            let _ = fs::set_permissions(&item.path, perm);
        }
    }
    Ok(r.size)
}

/// Permission bits to restore: the Unix mode's `rwxrwxrwx` only, never
/// setuid, setgid or sticky.
fn perm_bits(e: &CentralEntry) -> Option<u32> {
    e.unix_mode().map(|m| m & 0o777)
}

#[cfg(unix)]
fn set_file_mode(f: &File, e: &CentralEntry) {
    use std::os::unix::fs::PermissionsExt;
    if let Some(bits) = perm_bits(e) {
        let _ = f.set_permissions(fs::Permissions::from_mode(bits));
    }
}

#[cfg(not(unix))]
fn set_file_mode(_: &File, _: &CentralEntry) {}

/// Windows: the entry is read-only (DOS attribute, or a Unix mode without
/// the owner write bit). Applied after the rename, which could otherwise
/// fail on some file systems.
#[cfg(not(unix))]
fn read_only(e: &CentralEntry) -> bool {
    match perm_bits(e) {
        Some(bits) => bits & 0o200 == 0,
        None => e.external_attrs & 0x01 != 0,
    }
}

/// Step 8 (Unix): create the link under a temporary name, then rename it
/// into place (replacing an existing file or link under `Always`).
#[cfg(unix)]
fn make_symlink(item: &Item<'_>, opts: &ExtractOptions) -> Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let target = OsStr::from_bytes(&item.target);
    let (temp, ()) = with_temp(parent_of(&item.path), |p| {
        std::os::unix::fs::symlink(target, p)
    })?;
    if opts.restore_mtime {
        if let Some(t) = item.e.modified() {
            set_link_mtime(&temp.path, t);
        }
    }
    temp.persist(&item.path)
}

#[cfg(not(unix))]
fn make_symlink(_: &Item<'_>, _: &ExtractOptions) -> Result<()> {
    // Never reached: symlink entries are dropped from the plan here.
    Ok(())
}

/// `utimensat(..., AT_SYMLINK_NOFOLLOW)`: the link's own mtime (std has no
/// way to set it without following the link). Best effort.
#[cfg(unix)]
fn set_link_mtime(path: &Path, t: std::time::SystemTime) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    let secs = crate::timeconv::unix_secs(t);
    // time_t is i32 on some targets, where this can fail.
    #[allow(irrefutable_let_patterns, clippy::useless_conversion)]
    let Ok(tv_sec) = libc::time_t::try_from(secs) else {
        return;
    };
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec { tv_sec, tv_nsec: 0 },
    ];
    // SAFETY: `c` is a NUL-terminated path and `times` two valid timespecs,
    // both alive for the call; utimensat only reads them.
    unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        );
    }
}

/// Step 9: a directory entry's permissions and mtime, through a handle
/// opened without following symlinks. Best effort.
fn restore_dir_attrs(item: &Item<'_>, opts: &ExtractOptions) {
    let Ok(d) = open_dir(&item.path) else {
        return;
    };
    #[cfg(unix)]
    if opts.restore_permissions {
        set_file_mode(&d, item.e);
    }
    if opts.restore_mtime {
        if let Some(t) = item.e.modified() {
            let _ = d.set_modified(t);
        }
    }
}

#[cfg(unix)]
fn open_dir(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
}

/// Windows: a directory handle needs `FILE_FLAG_BACKUP_SEMANTICS`, and
/// setting its times `FILE_WRITE_ATTRIBUTES`; `FILE_FLAG_OPEN_REPARSE_POINT`
/// keeps it from following a junction or symlink.
#[cfg(windows)]
fn open_dir(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(name: &str) -> Option<Vec<&str>> {
        split_name(name, false)
    }

    #[test]
    fn safe_names_split() {
        assert_eq!(split("a/b/c.txt"), Some(vec!["a", "b", "c.txt"]));
        assert_eq!(split("dir/"), Some(vec!["dir"]));
        assert_eq!(split("a//./b/"), Some(vec!["a", "b"]));
        assert_eq!(
            split("a\\b\\c"),
            Some(vec!["a", "b", "c"]),
            "backslash separates"
        );
        assert_eq!(split("./"), Some(vec![]));
        assert_eq!(split("..."), Some(vec!["..."]), "not a parent reference");
        assert_eq!(split("a..b/..c"), Some(vec!["a..b", "..c"]));
        assert_eq!(split("ab:c"), Some(vec!["ab:c"]), "colon is fine on Unix");
        assert_eq!(split("ünïcødé/文件.txt"), Some(vec!["ünïcødé", "文件.txt"]));
    }

    #[test]
    fn unsafe_names_refused() {
        for bad in [
            "../x",
            "a/../../x",
            "a/../b",
            "..",
            "a/..",
            "/etc/passwd",
            "//server/share/x",
            "\\x",
            "\\\\server\\share",
            "..\\x",
            "a\\..\\..\\x",
            "C:\\x",
            "C:x",
            "c:/x",
            "a\0b",
        ] {
            assert_eq!(split(bad), None, "{bad:?} must be refused");
            assert_eq!(
                split_name(bad, true),
                None,
                "{bad:?} must be refused (Windows)"
            );
        }
    }

    #[test]
    fn windows_only_rules() {
        for bad in [
            "ab:c",
            "file.txt:stream",
            "dir. /x",
            "x.",
            "x ",
            ".. ",
            "CON",
            "con.txt",
            "a/NUL",
            "nul .txt",
            "COM1",
            "lpt9.log",
            "COM¹",
            "CONIN$",
        ] {
            assert_eq!(split_name(bad, true), None, "{bad:?} on Windows");
            assert!(
                split_name(bad, false).is_some(),
                "{bad:?} is fine elsewhere"
            );
        }
        for ok in ["CONSOLE", "COM10", "comx", "nul_", "a.b.c", ".hidden"] {
            assert!(split_name(ok, true).is_some(), "{ok:?} on Windows");
        }
    }

    fn target(t: &str, depth: usize) -> Option<String> {
        check_link_target(t.as_bytes(), depth).map(|v| String::from_utf8(v).unwrap())
    }

    #[test]
    fn link_targets() {
        assert_eq!(target("file.txt", 0).as_deref(), Some("file.txt"));
        assert_eq!(target("./a//b/", 0).as_deref(), Some("a/b"));
        assert_eq!(target("../x", 1).as_deref(), Some("../x"));
        assert_eq!(target("../../x/y", 2).as_deref(), Some("../../x/y"));
        assert_eq!(target("..", 1).as_deref(), Some(".."));
        assert_eq!(target(".", 0).as_deref(), Some("."));
        assert_eq!(target("a\\b", 0).as_deref(), Some("a/b"));
        for (bad, depth) in [
            ("../x", 0),
            ("../../x", 1),
            ("..", 0),
            ("/etc/passwd", 3),
            ("\\x", 3),
            ("C:\\x", 3),
            ("x/../..", 3),
            ("x/../y", 3),
            ("..\\..\\x", 1),
            ("", 0),
            ("a\0b", 0),
        ] {
            assert_eq!(target(bad, depth), None, "{bad:?} at depth {depth}");
        }
    }
}
