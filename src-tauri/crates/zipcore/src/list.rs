//! `list_archive`: read the central directory, decompress nothing.

use std::fs::File;
use std::io;
use std::path::Path;
use std::time::SystemTime;

use crate::reader::{self, CentralEntry, ReadError, Source, FLAG_ENCRYPTED};
use crate::timeconv;
use crate::{EntryInfo, EntryKind, Error, Listing, Result};

/// "Version made by" host byte for Unix (external attributes hold `st_mode`).
const HOST_UNIX: u16 = 3;
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;
/// MS-DOS directory attribute (low byte of the external attributes).
const DOS_DIR: u32 = 0x10;

pub(crate) fn list_archive(path: &Path) -> Result<Listing> {
    let io_err = |e: io::Error| Error::Io {
        path: path.to_path_buf(),
        source: e,
    };
    let file = File::open(path).map_err(io_err)?;
    if !file.metadata().map_err(io_err)?.is_file() {
        return Err(Error::InvalidArchive("not a regular file".into()));
    }
    let mut src = Source::new(file).map_err(io_err)?;
    let dir = reader::read_directory(&mut src).map_err(|e| match e {
        ReadError::Io(e) => io_err(e),
        ReadError::Invalid(m) => Error::InvalidArchive(m),
    })?;

    let mut problems = dir.problems;
    for e in &dir.entries {
        problems.extend(e.problems.iter().map(|p| format!("{}: {p}", e.name)));
    }
    Ok(Listing {
        entries: dir.entries.iter().map(entry_info).collect(),
        archive_bytes: src.len,
        problems,
    })
}

fn entry_info(e: &CentralEntry) -> EntryInfo {
    let mode = unix_mode_word(e);
    let kind = kind(e, mode);
    let mut name = e.name.clone();
    // A directory recognized only by its attributes still lists with a slash.
    if kind == EntryKind::Dir && !name.ends_with('/') {
        name.push('/');
    }
    EntryInfo {
        name,
        kind,
        method: e.method,
        compressed: e.compressed,
        uncompressed: e.uncompressed,
        crc32: e.crc,
        modified: modified(e),
        unix_mode: mode.map(|m| m & 0o7777),
        encrypted: e.flags & FLAG_ENCRYPTED != 0,
    }
}

/// The full `st_mode` word, when the archive was made on Unix and records one.
fn unix_mode_word(e: &CentralEntry) -> Option<u32> {
    let mode = e.external_attrs >> 16;
    (e.version_made_by >> 8 == HOST_UNIX && mode != 0).then_some(mode)
}

fn kind(e: &CentralEntry, mode: Option<u32>) -> EntryKind {
    let file_type = mode.map(|m| m & S_IFMT);
    if e.is_dir() {
        return EntryKind::Dir;
    }
    // Attribute-only directories must be empty: a named entry with data is a
    // file whatever its attributes claim.
    let attr_dir = file_type == Some(S_IFDIR) || e.external_attrs & DOS_DIR != 0;
    if attr_dir && e.uncompressed == 0 {
        EntryKind::Dir
    } else if file_type == Some(S_IFLNK) {
        EntryKind::Symlink
    } else {
        EntryKind::File
    }
}

/// The 0x5455 mtime (UTC) if present, else the DOS fields as local time.
fn modified(e: &CentralEntry) -> Option<SystemTime> {
    let ext = reader::extra_fields(&e.extra)
        .find(|(id, _)| *id == timeconv::EXT_TIMESTAMP_ID)
        .and_then(|(_, data)| timeconv::ext_timestamp_mtime(data));
    let secs = match ext {
        Some(s) => s,
        None => {
            let c = timeconv::civil_from_dos(e.dos_time, e.dos_date)?;
            timeconv::unix_from_civil_local(c)
        }
    };
    timeconv::system_time_from_unix(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, host: u16, attrs: u32, size: u64) -> CentralEntry {
        CentralEntry {
            name: name.into(),
            raw_name: name.as_bytes().to_vec(),
            flags: 0,
            method: 0,
            crc: 0,
            compressed: size,
            uncompressed: size,
            version_made_by: host << 8 | 20,
            dos_time: 0,
            dos_date: 0,
            external_attrs: attrs,
            extra: Vec::new(),
            local_offset: 0,
            problems: Vec::new(),
        }
    }

    #[test]
    fn kinds_and_modes() {
        let unix = |mode: u32| mode << 16;
        let cases = [
            (entry("d/", 0, 0, 0), EntryKind::Dir, "d/", None),
            (entry("d", 0, DOS_DIR, 0), EntryKind::Dir, "d/", None),
            (entry("d", 0, DOS_DIR, 5), EntryKind::File, "d", None),
            (
                entry("u", 3, unix(0o040755), 0),
                EntryKind::Dir,
                "u/",
                Some(0o755),
            ),
            (
                entry("l", 3, unix(0o120777), 3),
                EntryKind::Symlink,
                "l",
                Some(0o777),
            ),
            (
                entry("f", 3, unix(0o104755), 3),
                EntryKind::File,
                "f",
                Some(0o4755),
            ),
            // Host Unix but no mode recorded.
            (entry("f", 3, 0, 3), EntryKind::File, "f", None),
            // A symlink type word from a non-Unix host means nothing.
            (entry("w", 0, unix(0o120777), 3), EntryKind::File, "w", None),
        ];
        for (e, kind, name, mode) in cases {
            let info = entry_info(&e);
            assert_eq!(
                (info.kind, info.name.as_str(), info.unix_mode),
                (kind, name, mode),
                "{e:?}"
            );
        }
    }
}
