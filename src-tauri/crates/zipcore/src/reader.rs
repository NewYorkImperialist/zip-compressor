//! A defensive ZIP structure reader (PKWARE APPNOTE 6.3.x) for archives made
//! by any tool, used by [`crate::test_archive`].
//!
//! Nothing read from the file is trusted: every offset and size is checked
//! against the real file length before it is used, nothing is allocated from
//! a declared size (only from bytes that were actually read, or from 16-bit
//! length fields), and arithmetic is checked. Damage that makes the archive
//! unreadable as a whole is a [`ReadError::Invalid`]; damage to one entry is
//! recorded on that entry instead.
//!
//! Supported: archive comments (the end record is searched for backwards over
//! the last 65,557 bytes), ZIP64 end records and extra fields, data prepended
//! to the archive (self-extractors; offsets are shifted and a problem noted),
//! Info-ZIP Unicode Path extra fields. Multi-disk (split/spanned) archives
//! are rejected.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};

pub(crate) const LOCAL_SIG: u32 = 0x0403_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const EOCD_SIG: u32 = 0x0605_4b50;
const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
pub(crate) const DESCRIPTOR_SIG: u32 = 0x0807_4b50;
const ZIP64_EXTRA_ID: u16 = 0x0001;
const UNICODE_PATH_EXTRA_ID: u16 = 0x7075;

const EOCD_LEN: u64 = 22;
const MAX_COMMENT: u64 = 0xFFFF;
const ZIP64_LOCATOR_LEN: u64 = 20;
const ZIP64_EOCD_LEN: u64 = 56;
const CENTRAL_FIXED_LEN: u64 = 46;
pub(crate) const LOCAL_FIXED_LEN: u64 = 30;

const U16_SAT: u16 = 0xFFFF;
const U32_SAT: u32 = 0xFFFF_FFFF;

/// General-purpose flag bits.
pub(crate) const FLAG_ENCRYPTED: u16 = 1 << 0;
pub(crate) const FLAG_DESCRIPTOR: u16 = 1 << 3;
pub(crate) const FLAG_STRONG_ENCRYPTION: u16 = 1 << 6;
const FLAG_UTF8: u16 = 1 << 11;

#[derive(Debug)]
pub(crate) enum ReadError {
    Io(io::Error),
    Invalid(String),
}

impl From<io::Error> for ReadError {
    fn from(e: io::Error) -> Self {
        ReadError::Io(e)
    }
}

fn invalid<T>(msg: impl Into<String>) -> Result<T, ReadError> {
    Err(ReadError::Invalid(msg.into()))
}

/// The archive file and its length (taken once, up front).
pub(crate) struct Source {
    pub file: File,
    pub len: u64,
}

impl Source {
    pub fn new(file: File) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Source { file, len })
    }

    /// Fill `buf` from `offset`; the caller has checked that it lies within `len`.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)
    }
}

pub(crate) fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}
pub(crate) fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
pub(crate) fn u64_at(b: &[u8], i: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[i..i + 8]);
    u64::from_le_bytes(a)
}

/// One central directory record, ZIP64 values resolved.
#[derive(Debug, Clone)]
pub(crate) struct CentralEntry {
    /// Display name (UTF-8, CP437 or the Unicode Path extra).
    pub name: String,
    /// Name bytes exactly as stored, for comparison with the local header.
    pub raw_name: Vec<u8>,
    pub flags: u16,
    pub method: u16,
    pub crc: u32,
    pub compressed: u64,
    pub uncompressed: u64,
    /// Absolute file offset of the local header (prepended data accounted for).
    pub local_offset: u64,
    /// Problems found while parsing this record.
    pub problems: Vec<String>,
}

impl CentralEntry {
    pub fn is_dir(&self) -> bool {
        self.raw_name.last() == Some(&b'/')
    }
}

/// The parsed central directory.
#[derive(Debug)]
pub(crate) struct Directory {
    pub entries: Vec<CentralEntry>,
    /// Absolute offset where the central directory starts; entry data must end
    /// at or before it.
    pub cd_start: u64,
    /// Archive-level problems that did not stop the parse.
    pub problems: Vec<String>,
}

/// Location of the end of central directory record.
struct Eocd {
    pos: u64,
    disk: u16,
    cd_disk: u16,
    entries_here: u16,
    entries: u16,
    cd_size: u32,
    cd_offset: u32,
}

fn find_eocd(src: &mut Source, problems: &mut Vec<String>) -> Result<Eocd, ReadError> {
    if src.len == 0 {
        return invalid("the file is empty");
    }
    if src.len < EOCD_LEN {
        return invalid(format!(
            "the file is only {} bytes, too short for a ZIP archive",
            src.len
        ));
    }
    let tail_len = src.len.min(EOCD_LEN + MAX_COMMENT);
    let tail_start = src.len - tail_len;
    // At most 65,557 bytes.
    let mut tail = vec![0u8; tail_len as usize];
    src.read_at(tail_start, &mut tail)?;

    let last = tail.len() - EOCD_LEN as usize;
    let mut exact = None;
    let mut fallback = None;
    for i in (0..=last).rev() {
        if u32_at(&tail, i) != EOCD_SIG {
            continue;
        }
        let end = i + EOCD_LEN as usize + usize::from(u16_at(&tail, i + 20));
        if end == tail.len() {
            exact = Some(i);
            break;
        }
        if end < tail.len() && fallback.is_none() {
            fallback = Some(i);
        }
    }
    let Some(i) = exact.or(fallback) else {
        let mut head = [0u8; 4];
        src.read_at(0, &mut head)?;
        return if u32::from_le_bytes(head) == LOCAL_SIG {
            invalid("no end-of-central-directory record: the archive is truncated or damaged")
        } else {
            invalid("no end-of-central-directory record found (not a ZIP file?)")
        };
    };
    let end = i + EOCD_LEN as usize + usize::from(u16_at(&tail, i + 20));
    if end < tail.len() {
        problems.push(format!(
            "{} unexpected bytes after the end of the archive",
            tail.len() - end
        ));
    }
    Ok(Eocd {
        pos: tail_start + i as u64,
        disk: u16_at(&tail, i + 4),
        cd_disk: u16_at(&tail, i + 6),
        entries_here: u16_at(&tail, i + 8),
        entries: u16_at(&tail, i + 10),
        cd_size: u32_at(&tail, i + 12),
        cd_offset: u32_at(&tail, i + 16),
    })
}

const SPLIT: &str = "split archives are not supported";

/// Where the central directory is, as declared, and where it actually ends.
struct CdLocation {
    entries: u64,
    size: u64,
    offset: u64,
    /// Offset of the record that follows the central directory (ZIP64 end
    /// record or end record).
    actual_end: u64,
}

fn locate_cd(src: &mut Source, eocd: &Eocd) -> Result<CdLocation, ReadError> {
    // ZIP64 locator directly before the end record?
    if let Some(loc_pos) = eocd.pos.checked_sub(ZIP64_LOCATOR_LEN) {
        let mut loc = [0u8; ZIP64_LOCATOR_LEN as usize];
        src.read_at(loc_pos, &mut loc)?;
        if u32_at(&loc, 0) == ZIP64_LOCATOR_SIG {
            let rec_disk = u32_at(&loc, 4);
            let rec_offset = u64_at(&loc, 8);
            let disks = u32_at(&loc, 16);
            if rec_disk != 0 || disks > 1 {
                return invalid(SPLIT);
            }
            // Where the locator says, or (prepended data) directly before it.
            let mut candidates = vec![rec_offset];
            if let Some(p) = loc_pos.checked_sub(ZIP64_EOCD_LEN) {
                if p != rec_offset {
                    candidates.push(p);
                }
            }
            for pos in candidates {
                if pos.checked_add(ZIP64_EOCD_LEN).is_none_or(|e| e > loc_pos) {
                    continue;
                }
                let mut r = [0u8; ZIP64_EOCD_LEN as usize];
                src.read_at(pos, &mut r)?;
                if u32_at(&r, 0) != ZIP64_EOCD_SIG {
                    continue;
                }
                let (disk, cd_disk) = (u32_at(&r, 16), u32_at(&r, 20));
                let (here, total) = (u64_at(&r, 24), u64_at(&r, 32));
                if disk != 0 || cd_disk != 0 || here != total {
                    return invalid(SPLIT);
                }
                return Ok(CdLocation {
                    entries: total,
                    size: u64_at(&r, 40),
                    offset: u64_at(&r, 48),
                    actual_end: pos,
                });
            }
            return invalid("the ZIP64 end record is missing or damaged");
        }
    }
    if eocd.disk != 0 || eocd.cd_disk != 0 || eocd.entries_here != eocd.entries {
        return invalid(SPLIT);
    }
    // Saturated fields without a ZIP64 record are taken literally; the
    // caller's bounds checks catch nonsense.
    Ok(CdLocation {
        entries: u64::from(eocd.entries),
        size: u64::from(eocd.cd_size),
        offset: u64::from(eocd.cd_offset),
        actual_end: eocd.pos,
    })
}

/// Read and parse the central directory.
pub(crate) fn read_directory(src: &mut Source) -> Result<Directory, ReadError> {
    let mut problems = Vec::new();
    let eocd = find_eocd(src, &mut problems)?;
    let cd = locate_cd(src, &eocd)?;

    let Some(declared_end) = cd.offset.checked_add(cd.size) else {
        return invalid("the central directory offset and size are out of range");
    };
    if declared_end > cd.actual_end {
        return invalid(format!(
            "the central directory ({} bytes at offset {}) extends past the end record at {}: \
             the archive is truncated or damaged",
            cd.size, cd.offset, cd.actual_end
        ));
    }
    let min_bytes = u128::from(cd.entries) * u128::from(CENTRAL_FIXED_LEN);
    if min_bytes > u128::from(cd.size) {
        return invalid(format!(
            "the end record lists {} entries but the central directory is only {} bytes",
            cd.entries, cd.size
        ));
    }

    // Data before the archive (a self-extractor stub) shifts every offset.
    let gap = cd.actual_end - declared_end;
    let mut base = 0u64;
    if gap > 0 {
        let sig_at = |src: &mut Source, pos: u64| -> Result<bool, ReadError> {
            if pos.checked_add(4).is_none_or(|e| e > src.len) {
                return Ok(false);
            }
            let mut b = [0u8; 4];
            src.read_at(pos, &mut b)?;
            Ok(u32::from_le_bytes(b) == CENTRAL_SIG)
        };
        if cd.entries == 0 || sig_at(src, cd.offset + gap)? {
            base = gap;
            problems.push(format!(
                "{gap} extra bytes before the archive (e.g. a self-extractor); offsets adjusted"
            ));
        } else if sig_at(src, cd.offset)? {
            problems.push(format!(
                "{gap} unexpected bytes between the central directory and the end record"
            ));
        } else {
            return invalid(format!(
                "no central directory at offset {} (or {})",
                cd.offset,
                cd.offset + gap
            ));
        }
    }
    let cd_start = cd.offset + base;

    src.file.seek(SeekFrom::Start(cd_start))?;
    let mut rd = BufReader::with_capacity(64 * 1024, (&src.file).take(cd.size));
    // Never reserve from the declared count beyond what the bytes can hold.
    let mut entries = Vec::with_capacity(cd.entries.min(4096) as usize);
    for index in 0..cd.entries {
        let entry = match read_central(&mut rd, base) {
            Ok(e) => e,
            Err(ReadError::Io(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                return invalid(format!(
                    "the central directory ends after {index} of its {} entries",
                    cd.entries
                ));
            }
            Err(ReadError::Invalid(m)) => {
                return invalid(format!("central directory record {}: {m}", index + 1));
            }
            Err(e) => return Err(e),
        };
        entries.push(entry);
    }
    let left = rd.get_ref().limit() + rd.buffer().len() as u64;
    if left > 0 {
        problems.push(format!(
            "{left} unexpected bytes at the end of the central directory"
        ));
    }
    Ok(Directory {
        entries,
        cd_start,
        problems,
    })
}

fn read_vec(rd: &mut impl Read, len: usize) -> Result<Vec<u8>, ReadError> {
    // `len` comes from a 16-bit field.
    let mut v = vec![0u8; len];
    rd.read_exact(&mut v)?;
    Ok(v)
}

fn read_central(rd: &mut impl Read, base: u64) -> Result<CentralEntry, ReadError> {
    let mut h = [0u8; CENTRAL_FIXED_LEN as usize];
    rd.read_exact(&mut h)?;
    if u32_at(&h, 0) != CENTRAL_SIG {
        return invalid("bad signature (damaged central directory)");
    }
    let flags = u16_at(&h, 8);
    let method = u16_at(&h, 10);
    let crc = u32_at(&h, 16);
    let csize32 = u32_at(&h, 20);
    let usize32 = u32_at(&h, 24);
    let name_len = usize::from(u16_at(&h, 28));
    let extra_len = usize::from(u16_at(&h, 30));
    let comment_len = usize::from(u16_at(&h, 32));
    let disk16 = u16_at(&h, 34);
    let offset32 = u32_at(&h, 42);
    let raw_name = read_vec(rd, name_len)?;
    let extra = read_vec(rd, extra_len)?;
    let _comment = read_vec(rd, comment_len)?;

    let mut problems = Vec::new();
    let need = Zip64Need {
        uncompressed: usize32 == U32_SAT,
        compressed: csize32 == U32_SAT,
        offset: offset32 == U32_SAT,
        disk: disk16 == U16_SAT,
    };
    let z = if need.any() {
        let z = parse_zip64(&extra, need);
        if z.is_none() {
            problems.push("ZIP64 extra field is missing or too short".to_string());
        }
        z.unwrap_or_default()
    } else {
        Zip64Values::default()
    };
    let uncompressed = z.uncompressed.unwrap_or(u64::from(usize32));
    let compressed = z.compressed.unwrap_or(u64::from(csize32));
    let offset = z.offset.unwrap_or(u64::from(offset32));
    let disk = z.disk.unwrap_or(u32::from(disk16));
    if disk != 0 {
        problems.push(format!("starts on disk {disk} ({SPLIT})"));
    }
    let local_offset = match offset.checked_add(base) {
        Some(o) => o,
        None => {
            problems.push("local header offset is out of range".to_string());
            u64::MAX
        }
    };
    let name = display_name(&raw_name, flags, &extra);
    Ok(CentralEntry {
        name,
        raw_name,
        flags,
        method,
        crc,
        compressed,
        uncompressed,
        local_offset,
        problems,
    })
}

/// Which 32-bit (or 16-bit) slots overflowed into the ZIP64 extra field.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Zip64Need {
    pub uncompressed: bool,
    pub compressed: bool,
    pub offset: bool,
    pub disk: bool,
}

impl Zip64Need {
    pub fn any(self) -> bool {
        self.uncompressed || self.compressed || self.offset || self.disk
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Zip64Values {
    pub uncompressed: Option<u64>,
    pub compressed: Option<u64>,
    pub offset: Option<u64>,
    pub disk: Option<u32>,
}

/// Iterate the (id, data) pairs of an extra field block; stops at the first
/// malformed field.
pub(crate) fn extra_fields(extra: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    let mut i = 0usize;
    std::iter::from_fn(move || {
        if i + 4 > extra.len() {
            return None;
        }
        let id = u16_at(extra, i);
        let len = usize::from(u16_at(extra, i + 2));
        let data = extra.get(i + 4..i + 4 + len)?;
        i += 4 + len;
        Some((id, data))
    })
}

/// The ZIP64 extra field: only the overflowing fields are present, in the
/// fixed order uncompressed size, compressed size, offset, disk.
pub(crate) fn parse_zip64(extra: &[u8], need: Zip64Need) -> Option<Zip64Values> {
    let (_, data) = extra_fields(extra).find(|(id, _)| *id == ZIP64_EXTRA_ID)?;
    let mut v = Zip64Values::default();
    let mut i = 0usize;
    let take8 = |i: &mut usize| -> Option<u64> {
        let b = data.get(*i..*i + 8)?;
        *i += 8;
        Some(u64_at(b, 0))
    };
    if need.uncompressed {
        v.uncompressed = Some(take8(&mut i)?);
    }
    if need.compressed {
        v.compressed = Some(take8(&mut i)?);
    }
    if need.offset {
        v.offset = Some(take8(&mut i)?);
    }
    if need.disk {
        let b = data.get(i..i + 4)?;
        v.disk = Some(u32_at(b, 0));
    }
    Some(v)
}

pub(crate) fn has_zip64_extra(extra: &[u8]) -> bool {
    extra_fields(extra).any(|(id, _)| id == ZIP64_EXTRA_ID)
}

/// The entry's name for display: the Info-ZIP Unicode Path extra if it is
/// present and current, else UTF-8 (flag bit 11, or valid UTF-8 anyway), else CP437.
fn display_name(raw: &[u8], flags: u16, extra: &[u8]) -> String {
    for (id, data) in extra_fields(extra) {
        let current = id == UNICODE_PATH_EXTRA_ID
            && data.len() >= 5
            && data[0] == 1
            && u32_at(data, 1) == crc32fast::hash(raw);
        if let (true, Ok(s)) = (current, std::str::from_utf8(data.get(5..).unwrap_or(&[]))) {
            return s.to_string();
        }
    }
    if flags & FLAG_UTF8 != 0 {
        return String::from_utf8_lossy(raw).into_owned();
    }
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => raw.iter().map(|&b| cp437(b)).collect(),
    }
}

fn cp437(b: u8) -> char {
    const HIGH: [char; 128] = [
        'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ',
        'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', 'á', 'í', 'ó', 'ú',
        'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡',
        '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', '└', '┴', '┬', '├', '─', '┼', '╞', '╟',
        '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘',
        '┌', '█', '▄', '▌', '▐', '▀', 'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ',
        '∞', 'φ', 'ε', '∩', '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²',
        '■', '\u{a0}',
    ];
    if b < 0x80 {
        char::from(b)
    } else {
        HIGH[usize::from(b - 0x80)]
    }
}

/// A local file header, ZIP64 values resolved where present.
#[derive(Debug, Clone)]
pub(crate) struct LocalHeader {
    pub raw_name: Vec<u8>,
    pub flags: u16,
    pub method: u16,
    pub crc: u32,
    /// `None` when the 32-bit slot is saturated and no ZIP64 extra resolves it.
    pub compressed: Option<u64>,
    pub uncompressed: Option<u64>,
    pub zip64_extra: bool,
    /// Absolute offset of the entry's data.
    pub data_start: u64,
}

/// Read the local header at `offset`. `Err` is a problem description; the
/// header must lie entirely before `limit` (the central directory).
pub(crate) fn read_local(
    src: &mut Source,
    offset: u64,
    limit: u64,
) -> Result<Result<LocalHeader, String>, io::Error> {
    let fixed_end = match offset.checked_add(LOCAL_FIXED_LEN) {
        Some(e) if e <= limit => e,
        _ => {
            return Ok(Err(format!(
                "local header offset {offset} is outside the archive data"
            )))
        }
    };
    let mut h = [0u8; LOCAL_FIXED_LEN as usize];
    src.read_at(offset, &mut h)?;
    if u32_at(&h, 0) != LOCAL_SIG {
        return Ok(Err(format!(
            "no local header at offset {offset} (bad signature)"
        )));
    }
    let name_len = u64::from(u16_at(&h, 26));
    let extra_len = u64::from(u16_at(&h, 28));
    let data_start = fixed_end + name_len + extra_len;
    if data_start > limit {
        return Ok(Err(
            "local header runs into the central directory".to_string()
        ));
    }
    // At most 128 KiB, and within the file.
    let mut ne = vec![0u8; (name_len + extra_len) as usize];
    src.read_at(fixed_end, &mut ne)?;
    let extra = ne.split_off(name_len as usize);
    let raw_name = ne;

    let csize32 = u32_at(&h, 18);
    let usize32 = u32_at(&h, 22);
    let need = Zip64Need {
        uncompressed: usize32 == U32_SAT,
        compressed: csize32 == U32_SAT,
        ..Default::default()
    };
    let z = if need.any() {
        parse_zip64(&extra, need)
    } else {
        Some(Zip64Values::default())
    };
    let (compressed, uncompressed) = match z {
        Some(z) => (
            Some(z.compressed.unwrap_or(u64::from(csize32))),
            Some(z.uncompressed.unwrap_or(u64::from(usize32))),
        ),
        None => (None, None),
    };
    Ok(Ok(LocalHeader {
        raw_name,
        flags: u16_at(&h, 6),
        method: u16_at(&h, 8),
        crc: u32_at(&h, 14),
        compressed,
        uncompressed,
        zip64_extra: has_zip64_extra(&extra),
        data_start,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip64_fields_in_order() {
        let mut extra = vec![0x99, 0x99, 0, 0]; // unrelated empty field first
        extra.extend_from_slice(&1u16.to_le_bytes());
        extra.extend_from_slice(&20u16.to_le_bytes());
        extra.extend_from_slice(&7u64.to_le_bytes());
        extra.extend_from_slice(&9u64.to_le_bytes());
        extra.extend_from_slice(&3u32.to_le_bytes());
        let need = Zip64Need {
            compressed: true,
            offset: true,
            disk: true,
            ..Default::default()
        };
        let v = parse_zip64(&extra, need).unwrap();
        assert_eq!(v.uncompressed, None);
        assert_eq!(v.compressed, Some(7));
        assert_eq!(v.offset, Some(9));
        assert_eq!(v.disk, Some(3));
        let all = Zip64Need {
            uncompressed: true,
            compressed: true,
            offset: true,
            disk: true,
        };
        assert!(parse_zip64(&extra, all).is_none(), "too short");
        assert!(
            parse_zip64(&[1, 0, 200, 0], all).is_none(),
            "truncated field"
        );
    }

    #[test]
    fn names() {
        assert_eq!(display_name(b"a\x81b", 0, &[]), "aüb");
        assert_eq!(display_name("é".as_bytes(), 0, &[]), "é");
        let mut up = vec![0x75, 0x70];
        let uni = "ü.txt".as_bytes();
        up.extend_from_slice(&((5 + uni.len()) as u16).to_le_bytes());
        up.push(1);
        up.extend_from_slice(&crc32fast::hash(b"u.txt").to_le_bytes());
        up.extend_from_slice(uni);
        assert_eq!(display_name(b"u.txt", 0, &up), "ü.txt");
        assert_eq!(display_name(b"v.txt", 0, &up), "v.txt", "stale CRC");
    }
}
