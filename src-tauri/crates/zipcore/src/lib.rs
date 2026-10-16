//! Compression engine for zip-compressor.
//!
//! The public API in this file is the contract shared by the CLI (`zipc`),
//! the Tauri app, and the test suite. Implementation lives in private modules.

use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

mod codec;
mod container;
mod extract;
mod list;
mod output;
mod reader;
mod timeconv;
mod verify;
mod walk;
mod writer;
mod zlib;
mod zopfli_codec;

/// How each entry's data is encoded.
///
/// Every method produces standard Deflate or Stored entries, so archives open
/// in every unzip tool (Explorer, Archive Utility, Info-ZIP, 7-Zip). That is a
/// product rule: no method may require the recipient to install anything.
///
/// Whatever the method, an entry is never written larger than storing it
/// (beyond the documented chunk overhead for big files).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// No compression.
    Store,
    /// Deflate at the given level (0-9; higher values mean 9, 0 means Store).
    /// One zlib-rs pass per unit, whatever the level.
    Deflate { level: u8 },
    /// Deflate; tries level 9 and level 6 per small file (plus level 9 with
    /// zlib's `FILTERED` strategy when the data looks like a numeric table)
    /// and keeps the smallest; plain level 9 for large files.
    ///
    /// "Small" means a file under 1 MiB, compressed whole; anything bigger,
    /// and every chunk of a file of 4 MiB or more, is one level-9 pass. Ties
    /// go to plain level 9. Never larger than `Deflate { level: 9 }`, file by file.
    DeflateBest,
    /// Deflate produced by the Zopfli optimizer: same format as
    /// [`Method::Deflate`], typically 3-8% smaller, but far slower.
    /// `iterations` trades time for size (Zopfli's default is 15); 0 is
    /// treated as 1 and values above 1000 as 1000.
    ///
    /// Each unit also gets everything [`Method::DeflateBest`] tries and keeps
    /// the smallest result, so it is never larger than level 9 (or
    /// `DeflateBest`) unit by unit.
    /// Units that level 9 shrinks by less than 0.1% skip Zopfli entirely.
    Zopfli { iterations: u16 },
}

/// Named speed/size trade-offs offered by the CLI and the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Preset {
    /// Deflate level 1.
    Fast,
    /// Deflate level 6.
    #[default]
    Balanced,
    /// [`Method::DeflateBest`]: the smallest of several encoder settings
    /// per small file; level 9 for large files.
    Best,
    /// Zopfli. Smallest archive; slow.
    Max,
}

impl Preset {
    pub const ALL: [Preset; 4] = [Preset::Fast, Preset::Balanced, Preset::Best, Preset::Max];

    pub fn method(self) -> Method {
        match self {
            Preset::Fast => Method::Deflate { level: 1 },
            Preset::Balanced => Method::Deflate { level: 6 },
            Preset::Best => Method::DeflateBest,
            Preset::Max => Method::Zopfli { iterations: 15 },
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Preset::Fast => "fast",
            Preset::Balanced => "balanced",
            Preset::Best => "best",
            Preset::Max => "max",
        }
    }
}

impl Default for Method {
    fn default() -> Self {
        Method::Deflate { level: 6 }
    }
}

/// What to do when the input tree contains a symbolic link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SymlinkPolicy {
    /// Store the link itself (Unix-style symlink entry). Default.
    #[default]
    Store,
    /// Follow the link and archive what it points to.
    Follow,
    /// Leave the link out of the archive.
    Skip,
}

#[derive(Debug, Clone)]
pub struct CompressOptions {
    pub method: Method,
    pub symlinks: SymlinkPolicy,
    /// Leave out OS clutter: `.DS_Store`, `__MACOSX/`, AppleDouble `._*`, `Thumbs.db`, `desktop.ini`.
    pub exclude_junk: bool,
    /// Worker threads for compression. `0` means one per available CPU core.
    /// The archive bytes are identical whatever this is set to.
    pub threads: usize,
}

impl Default for CompressOptions {
    fn default() -> Self {
        Self {
            method: Method::default(),
            symlinks: SymlinkPolicy::default(),
            exclude_junk: true,
            threads: 0,
        }
    }
}

/// Reported after each entry is written, and periodically while large files stream.
#[derive(Debug, Clone)]
pub struct Progress {
    /// Archive path of the entry currently being written (forward slashes).
    pub current: String,
    pub files_done: u64,
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
}

/// Result of a successful compression.
#[derive(Debug, Clone)]
pub struct Summary {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    /// Total uncompressed bytes of file contents.
    pub bytes_in: u64,
    /// Size of the finished archive.
    pub bytes_out: u64,
    pub elapsed: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The file is not a readable ZIP archive at all (e.g. no end-of-central-
    /// directory record). Damage to individual entries is reported in
    /// [`TestReport::problems`] instead.
    #[error("not a valid ZIP archive: {0}")]
    InvalidArchive(String),
    #[error("input not found: {0}")]
    NotFound(PathBuf),
    #[error("refusing to write the archive inside one of its own inputs: {0}")]
    OutputInsideInput(PathBuf),
    #[error(
        "two inputs would both be stored as `{0}`; rename one or pass its parent folder instead"
    )]
    DuplicateName(String),
    #[error("cancelled")]
    Cancelled,
    /// An entry name that would land outside the destination: absolute,
    /// containing `..`, a drive letter, or (on extract) a path through a
    /// symlink. Nothing from such an archive is written.
    #[error("unsafe entry name `{0}`: it would be written outside the destination")]
    UnsafePath(String),
    /// Extracting would replace an existing file and
    /// [`Overwrite::Never`] is in effect.
    #[error("{0} already exists (pass --overwrite to replace it)")]
    AlreadyExists(PathBuf),
    /// An entry that cannot be extracted: encrypted, or a compression method
    /// other than Stored/Deflate.
    #[error("cannot extract `{entry}`: {reason}")]
    Unsupported { entry: String, reason: String },
    /// An entry's data is damaged (bad CRC, wrong size, truncated stream).
    #[error("`{entry}` is damaged: {reason}")]
    Corrupt { entry: String, reason: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// Compress `inputs` into a ZIP written to `out`.
///
/// Archive layout: each input appears under its own file or directory name,
/// e.g. compressing `/a/photos` and `/b/notes.txt` yields `photos/...` and
/// `notes.txt`. Entries are written in a deterministic (sorted) order.
///
/// `progress` is called as work advances. If `cancel` becomes true the call
/// stops promptly and returns [`Error::Cancelled`]; the partial output is the
/// caller's to discard.
pub fn compress<W: Write + Seek>(
    inputs: &[PathBuf],
    out: W,
    opts: &CompressOptions,
    progress: &mut dyn FnMut(&Progress),
    cancel: &AtomicBool,
) -> Result<Summary> {
    let started = Instant::now();
    let entries = walk::scan(inputs, opts, cancel)?;
    let written = writer::write_archive(
        &entries,
        out,
        opts,
        progress,
        cancel,
        Path::new("<archive output>"),
    )?;
    let mut summary = written.summary;
    summary.elapsed = started.elapsed();
    Ok(summary)
}

/// Like [`compress`], but writes to `out_path` via a temporary file in the same
/// directory that is renamed into place only on success, so a failed or
/// cancelled run never leaves a half-written archive behind.
pub fn compress_to_path(
    inputs: &[PathBuf],
    out_path: &Path,
    opts: &CompressOptions,
    progress: &mut dyn FnMut(&Progress),
    cancel: &AtomicBool,
) -> Result<Summary> {
    output::compress_to_path(inputs, out_path, opts, progress, cancel)
}

/// One problem found by [`test_archive`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Archive path of the affected entry, or `None` for archive-level problems.
    pub entry: Option<String>,
    pub message: String,
}

/// Result of checking an archive with [`test_archive`].
#[derive(Debug, Clone)]
pub struct TestReport {
    /// Entries listed in the central directory.
    pub entries: u64,
    /// Total uncompressed bytes that were decompressed and checked.
    pub bytes_checked: u64,
    pub problems: Vec<Problem>,
    pub elapsed: Duration,
}

impl TestReport {
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Check that the ZIP at `path` is intact, like `unzip -t`: every entry is
/// decompressed and its CRC-32 and sizes are compared with the headers.
///
/// Works on archives from any tool, not just ours. Entries that cannot be
/// verified (encrypted, or a compression method other than Stored/Deflate) are
/// reported as problems rather than silently skipped. Nothing is written to disk.
///
/// Returns `Err` only when the archive cannot be read at all (I/O failure,
/// [`Error::InvalidArchive`]) or on cancel; per-entry damage goes in the report.
/// `progress` reports entries and uncompressed bytes checked.
pub fn test_archive(
    path: &Path,
    progress: &mut dyn FnMut(&Progress),
    cancel: &AtomicBool,
) -> Result<TestReport> {
    verify::test_archive(path, progress, cancel)
}

/// What kind of thing an archive entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

/// One entry as listed by [`list_archive`], in central directory order.
#[derive(Debug, Clone)]
pub struct EntryInfo {
    /// Archive path (forward slashes; directories end in `/`).
    pub name: String,
    pub kind: EntryKind,
    /// Raw ZIP method number (0 = Stored, 8 = Deflate, ...).
    pub method: u16,
    pub compressed: u64,
    pub uncompressed: u64,
    pub crc32: u32,
    /// Modification time: the 0x5455 extended timestamp when present (UTC),
    /// otherwise the DOS date/time read as local time. `None` if invalid.
    pub modified: Option<std::time::SystemTime>,
    /// Unix permission bits (e.g. `0o644`) when the archive was made on Unix.
    pub unix_mode: Option<u32>,
    pub encrypted: bool,
}

/// Result of [`list_archive`].
#[derive(Debug, Clone)]
pub struct Listing {
    pub entries: Vec<EntryInfo>,
    /// Size of the archive file.
    pub archive_bytes: u64,
    /// Archive-level problems noticed while reading the directory (not fatal).
    pub problems: Vec<String>,
}

/// Read an archive's central directory without decompressing anything.
/// Works on archives from any tool. Fails only if the directory can't be read.
pub fn list_archive(path: &Path) -> Result<Listing> {
    list::list_archive(path)
}

/// What to do when an extracted file already exists at the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overwrite {
    /// Fail with [`Error::AlreadyExists`] before writing anything. Default.
    #[default]
    Never,
    /// Leave existing files alone and extract the rest (counted as skipped).
    Skip,
    /// Replace existing files.
    Always,
}

#[derive(Debug, Clone)]
pub struct ExtractOptions {
    pub overwrite: Overwrite,
    /// Restore Unix permission bits when the archive has them
    /// (never setuid/setgid/sticky).
    pub restore_permissions: bool,
    /// Restore modification times.
    pub restore_mtime: bool,
    /// Create symlink entries as symlinks (only if the target stays inside
    /// the destination; otherwise [`Error::UnsafePath`]). When false, they are
    /// skipped.
    pub symlinks: bool,
    /// Only extract entries whose archive path equals one of these or lies
    /// under one of them (a directory prefix). Empty means everything.
    pub only: Vec<String>,
    /// Worker threads. `0` means one per available CPU core.
    pub threads: usize,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            overwrite: Overwrite::default(),
            restore_permissions: true,
            restore_mtime: true,
            symlinks: true,
            only: Vec::new(),
            threads: 0,
        }
    }
}

/// Result of a successful [`extract_archive`].
#[derive(Debug, Clone)]
pub struct ExtractSummary {
    pub files: u64,
    pub dirs: u64,
    pub symlinks: u64,
    /// Existing files left alone under [`Overwrite::Skip`].
    pub skipped: u64,
    /// Total bytes written.
    pub bytes_out: u64,
    pub elapsed: Duration,
}

/// Extract the archive at `path` into the directory `dest` (created if missing).
///
/// Safety rules, checked for every entry before anything is written: no entry
/// may resolve outside `dest` (absolute paths, `..`, drive letters, writing
/// through a symlink); existing files are handled per [`Overwrite`]. Each file
/// is written to a temporary name and renamed into place only after its CRC-32
/// and size check out, so a damaged entry never leaves a bad file behind.
///
/// `progress` reports files and uncompressed bytes. On cancel, returns
/// [`Error::Cancelled`]; files already completed stay in place.
pub fn extract_archive(
    path: &Path,
    dest: &Path,
    opts: &ExtractOptions,
    progress: &mut dyn FnMut(&Progress),
    cancel: &AtomicBool,
) -> Result<ExtractSummary> {
    extract::extract_archive(path, dest, opts, progress, cancel)
}

/// A wall-clock date and time in the local time zone, as returned by
/// [`local_time`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i64,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// `t` as local wall-clock time, using the zone rules (UTC offset and daylight
/// saving) in force at that instant: the conversion used for the DOS time
/// fields, offered so front ends can display [`EntryInfo::modified`] the same
/// way. Falls back to UTC where the local zone can't be determined.
pub fn local_time(t: std::time::SystemTime) -> LocalTime {
    let c = timeconv::civil_from_unix_local(timeconv::unix_secs(t));
    LocalTime {
        year: c.year,
        month: c.month,
        day: c.day,
        hour: c.hour,
        minute: c.minute,
        second: c.second,
    }
}
