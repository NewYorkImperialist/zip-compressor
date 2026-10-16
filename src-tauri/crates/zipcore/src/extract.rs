//! `extract_archive`: safe, verified extraction.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::{ExtractOptions, ExtractSummary, Progress, Result};

pub(crate) fn extract_archive(
    _path: &Path,
    _dest: &Path,
    _opts: &ExtractOptions,
    _progress: &mut dyn FnMut(&Progress),
    _cancel: &AtomicBool,
) -> Result<ExtractSummary> {
    todo!("extract_archive")
}
