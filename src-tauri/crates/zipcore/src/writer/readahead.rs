//! Read-ahead for batches of small files.
//!
//! A worker compressing a batch keeps the next few files of the batch open
//! and has asked the OS to start reading each of them (`F_RDADVISE` on
//! macOS, `POSIX_FADV_WILLNEED` on Linux/FreeBSD), so when it gets to a
//! file its data is usually already in memory. When the page cache is cold
//! (or small files keep getting evicted under memory pressure) this turns
//! one synchronous random read per file into many reads in flight; with one
//! worker it is the difference between waiting on the disk for every tiny
//! file and hardly at all.
//!
//! Purely advisory: a file that fails to open here is simply opened (and its
//! error reported) the normal way when its turn comes, and nothing about the
//! archive depends on whether read-ahead happened.

use std::collections::VecDeque;
use std::fs::File;

use crate::walk::Entry;

/// Most files a worker keeps open ahead of the one it is compressing.
const MAX_DEPTH: usize = 16;
/// Most descriptors all workers together keep open for read-ahead, as a
/// fraction of the soft `RLIMIT_NOFILE` (macOS defaults to 256).
const FD_SHARE: u64 = 4;
const MAX_FDS: u64 = 1024;

/// Read-ahead depth per worker for `threads` workers (0: disabled).
pub(super) fn depth_for(threads: usize) -> usize {
    if !SUPPORTED {
        return 0;
    }
    let fds = (open_files_limit() / FD_SHARE).min(MAX_FDS);
    let depth = (fds / threads.max(1) as u64).min(MAX_DEPTH as u64) as usize;
    // Opening only the file about to be read gains nothing.
    if depth < 2 {
        0
    } else {
        depth
    }
}

const SUPPORTED: bool = cfg!(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd"
));

#[cfg(unix)]
fn open_files_limit() -> u64 {
    let mut rl = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit only writes the struct we pass.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } != 0 {
        return 0;
    }
    // `rlim_t` is not 64 bits everywhere.
    #[allow(clippy::useless_conversion)]
    u64::try_from(rl.rlim_cur).unwrap_or(u64::MAX)
}

#[cfg(not(unix))]
fn open_files_limit() -> u64 {
    0
}

/// Ask the OS to start reading the first `len` bytes of `file`.
#[allow(unused_variables)]
fn advise(file: &File, len: u64) {
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        let ra = libc::radvisory {
            ra_offset: 0,
            ra_count: len.min(i32::MAX as u64) as libc::c_int,
        };
        // SAFETY: a valid descriptor and a valid `radvisory`; advisory only.
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_RDADVISE, &ra) };
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    {
        use std::os::fd::AsRawFd;
        let len = libc::off_t::try_from(len).unwrap_or(0);
        // SAFETY: a valid descriptor; advisory only.
        unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, len, libc::POSIX_FADV_WILLNEED) };
    }
}

/// The open-ahead window over one batch `(entry index, size)`.
pub(super) struct Window<'a> {
    entries: &'a [Entry],
    files: &'a [(usize, u64)],
    depth: usize,
    /// Files `taken..next` of the batch, opened (or not) in order.
    open: VecDeque<Option<File>>,
    next: usize,
}

impl<'a> Window<'a> {
    pub fn new(entries: &'a [Entry], files: &'a [(usize, u64)], depth: usize) -> Self {
        Window {
            entries,
            files,
            depth,
            open: VecDeque::with_capacity(depth),
            next: 0,
        }
    }

    /// The batch's file number `i` (taken strictly in order, from 0) if it
    /// was opened ahead; tops the window up first.
    pub fn take(&mut self, i: usize) -> Option<File> {
        if self.depth == 0 {
            return None;
        }
        debug_assert_eq!(self.next - self.open.len(), i, "files taken out of order");
        while self.next < self.files.len() && self.next < i + self.depth {
            let (idx, size) = self.files[self.next];
            self.open.push_back(open_ahead(&self.entries[idx], size));
            self.next += 1;
        }
        self.open.pop_front().flatten()
    }

    /// Close everything opened ahead (before an open that might be failing
    /// for lack of descriptors) and stop opening ahead.
    pub fn close_all(&mut self) {
        self.open.iter_mut().for_each(|f| *f = None);
        self.depth = 0;
    }
}

fn open_ahead(entry: &Entry, size: u64) -> Option<File> {
    if size == 0 {
        return None;
    }
    let file = File::open(&entry.path).ok()?;
    advise(&file, size);
    Some(file)
}
