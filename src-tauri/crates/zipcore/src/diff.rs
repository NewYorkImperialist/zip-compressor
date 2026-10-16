//! `diff_archive`: compare an archive with a directory.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::{DiffOptions, DiffReport, Progress, Result};

pub(crate) fn diff_archive(
    _archive: &Path,
    _dir: &Path,
    _opts: &DiffOptions,
    _progress: &mut dyn FnMut(&Progress),
    _cancel: &AtomicBool,
) -> Result<DiffReport> {
    todo!("diff_archive")
}
