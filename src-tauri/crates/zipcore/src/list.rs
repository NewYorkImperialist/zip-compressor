//! `list_archive`: read the central directory, decompress nothing.

use std::path::Path;

use crate::{Listing, Result};

pub(crate) fn list_archive(_path: &Path) -> Result<Listing> {
    todo!("list_archive")
}
