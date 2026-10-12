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
mod output;
mod timeconv;
mod walk;
mod writer;

/// How each entry's data is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// No compression.
    Store,
    /// Deflate at the given level (0-9). Opens in every unzip tool.
    Deflate { level: u8 },
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
    #[error("zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
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
