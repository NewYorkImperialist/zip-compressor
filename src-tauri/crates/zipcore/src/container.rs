//! ZIP container encoding (PKWARE APPNOTE 6.3.x): local file headers,
//! central directory records, ZIP64 extra fields and end records.
//!
//! Layout written by this crate:
//!
//! ```text
//! [local header + data] ... [central directory] [ZIP64 EOCD + locator]? [EOCD]
//! ```
//!
//! No data descriptors are used: every local header carries the real CRC and
//! sizes, either because they were known before it was written (buffered
//! entries) or because it was patched in place afterwards (streamed entries).
//!
//! ZIP64 rules implemented here (APPNOTE 4.4, 4.5.3):
//! - In a **local** header the ZIP64 extra (0x0001) holds *both* the
//!   uncompressed and the compressed size, and both 32-bit fields are set to
//!   0xFFFFFFFF.
//! - In a **central** record only the fields whose 32-bit slot overflowed
//!   (value >= 0xFFFFFFFF; the slot is then set to 0xFFFFFFFF) appear, in the
//!   fixed order uncompressed size, compressed size, local header offset.
//!   (The disk number never overflows: archives are single-disk.)
//! - The ZIP64 end record + locator are written when the entry count doesn't
//!   fit in 16 bits or the central directory's size or offset don't fit in 32.

use crate::timeconv;

pub(crate) const LOCAL_SIG: u32 = 0x0403_4b50;
pub(crate) const CENTRAL_SIG: u32 = 0x0201_4b50;
pub(crate) const EOCD_SIG: u32 = 0x0605_4b50;
pub(crate) const ZIP64_EOCD_SIG: u32 = 0x0606_4b50;
pub(crate) const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
pub(crate) const ZIP64_EXTRA_ID: u16 = 0x0001;

/// Fixed part of a local header.
pub(crate) const LOCAL_FIXED_LEN: usize = 30;

/// "Version made by": host system 3 (Unix) in the high byte, spec 6.3.
const VERSION_MADE_BY: u16 = (3 << 8) | 63;
const VERSION_STORED: u16 = 10;
const VERSION_DEFLATE_OR_DIR: u16 = 20;
const VERSION_ZIP64: u16 = 45;
/// General-purpose flag bit 11: name is UTF-8.
const FLAG_UTF8: u16 = 1 << 11;

const U32_MAX: u64 = 0xFFFF_FFFF;

const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;
/// MS-DOS directory attribute (low byte of the external attributes).
const DOS_DIR_ATTR: u32 = 0x10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryType {
    File,
    Dir,
    Symlink,
}

/// Compression method as stored in the headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CMethod {
    Stored = 0,
    Deflated = 8,
}

/// Per-entry metadata that doesn't depend on the data.
#[derive(Debug, Clone)]
pub(crate) struct Meta<'a> {
    pub name: &'a str,
    pub kind: EntryType,
    /// Unix mode as reported by the filesystem; only the 0o777 permission bits
    /// are kept (the type bits come from `kind`). `None` uses 644/755/777.
    pub mode: Option<u32>,
    /// DOS (time, date).
    pub dos: (u16, u16),
    /// 0x5455 payload (flags + mtime), identical in local and central headers.
    pub ext_time: Option<[u8; 5]>,
}

impl<'a> Meta<'a> {
    pub fn new(
        name: &'a str,
        kind: EntryType,
        mode: Option<u32>,
        mtime: Option<std::time::SystemTime>,
    ) -> Self {
        let (dos, ext_time) = match mtime {
            Some(t) => (
                timeconv::dos_datetime(t),
                timeconv::ext_timestamp_payload(timeconv::unix_secs(t)),
            ),
            None => (timeconv::DOS_EPOCH, None),
        };
        Meta {
            name,
            kind,
            mode,
            dos,
            ext_time,
        }
    }

    fn flags(&self) -> u16 {
        if self.name.is_ascii() {
            0
        } else {
            FLAG_UTF8
        }
    }

    pub fn external_attributes(&self) -> u32 {
        let (ty, default) = match self.kind {
            EntryType::File => (S_IFREG, 0o644),
            EntryType::Dir => (S_IFDIR, 0o755),
            EntryType::Symlink => (S_IFLNK, 0o777),
        };
        let perm = self.mode.map_or(default, |m| m & 0o777);
        let dos = if self.kind == EntryType::Dir {
            DOS_DIR_ATTR
        } else {
            0
        };
        ((ty | perm) << 16) | dos
    }
}

/// CRC and sizes of an entry's data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Sizes {
    pub crc: u32,
    pub compressed: u64,
    pub uncompressed: u64,
}

impl Sizes {
    /// True if either size needs ZIP64 in an exact local header.
    pub fn overflow(&self) -> bool {
        self.compressed >= U32_MAX || self.uncompressed >= U32_MAX
    }
}

/// True if the central record for these values needs a ZIP64 extra.
pub(crate) fn central_needs_zip64(s: &Sizes, offset: u64) -> bool {
    s.overflow() || offset >= U32_MAX
}

/// "Version needed to extract" for an entry.
pub(crate) fn version_needed(kind: EntryType, method: CMethod, zip64: bool) -> u16 {
    if zip64 {
        VERSION_ZIP64
    } else if kind == EntryType::Dir || method == CMethod::Deflated {
        VERSION_DEFLATE_OR_DIR
    } else {
        VERSION_STORED
    }
}

fn put16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// 32-bit slot for a value that may overflow into ZIP64.
fn slot32(v: u64) -> u32 {
    if v >= U32_MAX {
        u32::MAX
    } else {
        v as u32
    }
}

fn ext_time_extra(meta: &Meta<'_>, out: &mut Vec<u8>) {
    if let Some(p) = meta.ext_time {
        put16(out, timeconv::EXT_TIMESTAMP_ID);
        put16(out, p.len() as u16);
        out.extend_from_slice(&p);
    }
}

/// Encoded length of the local header for `meta`.
pub(crate) fn local_header_len(meta: &Meta<'_>, zip64: bool) -> usize {
    LOCAL_FIXED_LEN
        + meta.name.len()
        + if zip64 { 4 + 16 } else { 0 }
        + if meta.ext_time.is_some() { 4 + 5 } else { 0 }
}

/// Append a local file header.
///
/// With `zip64`, both 32-bit size fields are 0xFFFFFFFF and the sizes go in
/// a ZIP64 extra (uncompressed, then compressed). The header's length depends
/// only on `meta` and `zip64`, so a header written with placeholder sizes can
/// be overwritten in place with the real ones.
pub(crate) fn local_header(
    meta: &Meta<'_>,
    method: CMethod,
    version_needed: u16,
    s: &Sizes,
    zip64: bool,
    out: &mut Vec<u8>,
) {
    let start = out.len();
    put32(out, LOCAL_SIG);
    put16(out, version_needed);
    put16(out, meta.flags());
    put16(out, method as u16);
    put16(out, meta.dos.0);
    put16(out, meta.dos.1);
    put32(out, s.crc);
    if zip64 {
        put32(out, u32::MAX);
        put32(out, u32::MAX);
    } else {
        debug_assert!(!s.overflow(), "sizes need ZIP64 but none was reserved");
        put32(out, s.compressed as u32);
        put32(out, s.uncompressed as u32);
    }
    put16(out, meta.name.len() as u16);
    let extra_len = local_header_len(meta, zip64) - LOCAL_FIXED_LEN - meta.name.len();
    put16(out, extra_len as u16);
    out.extend_from_slice(meta.name.as_bytes());
    if zip64 {
        put16(out, ZIP64_EXTRA_ID);
        put16(out, 16);
        put64(out, s.uncompressed);
        put64(out, s.compressed);
    }
    ext_time_extra(meta, out);
    debug_assert_eq!(out.len() - start, local_header_len(meta, zip64));
}

/// Append a central directory record.
pub(crate) fn central_header(
    meta: &Meta<'_>,
    method: CMethod,
    version_needed: u16,
    s: &Sizes,
    offset: u64,
    out: &mut Vec<u8>,
) {
    let mut z64 = Vec::new();
    for v in [s.uncompressed, s.compressed, offset] {
        if v >= U32_MAX {
            put64(&mut z64, v);
        }
    }
    let mut extra_len = meta.ext_time.map_or(0, |p| 4 + p.len());
    if !z64.is_empty() {
        extra_len += 4 + z64.len();
    }
    put32(out, CENTRAL_SIG);
    put16(out, VERSION_MADE_BY);
    put16(out, version_needed);
    put16(out, meta.flags());
    put16(out, method as u16);
    put16(out, meta.dos.0);
    put16(out, meta.dos.1);
    put32(out, s.crc);
    put32(out, slot32(s.compressed));
    put32(out, slot32(s.uncompressed));
    put16(out, meta.name.len() as u16);
    put16(out, extra_len as u16);
    put16(out, 0); // comment length
    put16(out, 0); // disk number start
    put16(out, 0); // internal attributes
    put32(out, meta.external_attributes());
    put32(out, slot32(offset));
    out.extend_from_slice(meta.name.as_bytes());
    if !z64.is_empty() {
        put16(out, ZIP64_EXTRA_ID);
        put16(out, z64.len() as u16);
        out.extend_from_slice(&z64);
    }
    ext_time_extra(meta, out);
}

/// Append the end records for a central directory of `entries` records,
/// `cd_size` bytes long, starting at `cd_offset`.
pub(crate) fn end_records(entries: u64, cd_offset: u64, cd_size: u64, out: &mut Vec<u8>) {
    let zip64 = entries >= 0xFFFF || cd_offset >= U32_MAX || cd_size >= U32_MAX;
    if zip64 {
        let z64_offset = cd_offset + cd_size;
        put32(out, ZIP64_EOCD_SIG);
        put64(out, 44); // size of the remaining record
        put16(out, (3 << 8) | VERSION_ZIP64);
        put16(out, VERSION_ZIP64);
        put32(out, 0); // this disk
        put32(out, 0); // disk with the central directory
        put64(out, entries);
        put64(out, entries);
        put64(out, cd_size);
        put64(out, cd_offset);

        put32(out, ZIP64_LOCATOR_SIG);
        put32(out, 0); // disk with the ZIP64 end record
        put64(out, z64_offset);
        put32(out, 1); // total disks
    }
    let n16 = if entries >= 0xFFFF {
        0xFFFF
    } else {
        entries as u16
    };
    put32(out, EOCD_SIG);
    put16(out, 0);
    put16(out, 0);
    put16(out, n16);
    put16(out, n16);
    put32(out, slot32(cd_size));
    put32(out, slot32(cd_offset));
    put16(out, 0); // comment length
}

/// Name length limit of the 16-bit header field.
pub(crate) fn name_fits(name: &str) -> bool {
    name.len() <= usize::from(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(b: &[u8], i: usize) -> u16 {
        u16::from_le_bytes([b[i], b[i + 1]])
    }
    fn u32_at(b: &[u8], i: usize) -> u32 {
        u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
    }
    fn u64_at(b: &[u8], i: usize) -> u64 {
        u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
    }

    fn meta(name: &str, kind: EntryType) -> Meta<'_> {
        Meta {
            name,
            kind,
            mode: Some(0o104755),
            dos: (0x1234, 0x5678),
            ext_time: Some([1, 2, 3, 4, 5]),
        }
    }

    #[test]
    fn local_header_fields() {
        let m = meta("a/b.txt", EntryType::File);
        let s = Sizes {
            crc: 0xDEAD_BEEF,
            compressed: 10,
            uncompressed: 20,
        };
        let mut b = Vec::new();
        local_header(&m, CMethod::Deflated, 20, &s, false, &mut b);
        assert_eq!(b.len(), local_header_len(&m, false));
        assert_eq!(u32_at(&b, 0), LOCAL_SIG);
        assert_eq!(u16_at(&b, 4), 20);
        assert_eq!(u16_at(&b, 6), 0, "ASCII name: no UTF-8 flag, no descriptor");
        assert_eq!(u16_at(&b, 8), 8);
        assert_eq!((u16_at(&b, 10), u16_at(&b, 12)), (0x1234, 0x5678));
        assert_eq!(u32_at(&b, 14), 0xDEAD_BEEF);
        assert_eq!((u32_at(&b, 18), u32_at(&b, 22)), (10, 20));
        assert_eq!(u16_at(&b, 26), 7);
        assert_eq!(u16_at(&b, 28), 9);
        assert_eq!(&b[30..37], b"a/b.txt");
        assert_eq!(u16_at(&b, 37), 0x5455);
        assert_eq!(u16_at(&b, 39), 5);
        assert_eq!(&b[41..46], &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn local_zip64_extra_holds_both_sizes() {
        let m = meta("big", EntryType::File);
        let s = Sizes {
            crc: 1,
            compressed: 5,
            uncompressed: 0x1_2345_6789,
        };
        let mut b = Vec::new();
        local_header(&m, CMethod::Deflated, 45, &s, true, &mut b);
        assert_eq!(b.len(), local_header_len(&m, true));
        assert_eq!((u32_at(&b, 18), u32_at(&b, 22)), (u32::MAX, u32::MAX));
        assert_eq!(u16_at(&b, 28), 20 + 9);
        let x = 30 + 3;
        assert_eq!(u16_at(&b, x), ZIP64_EXTRA_ID);
        assert_eq!(u16_at(&b, x + 2), 16);
        assert_eq!(u64_at(&b, x + 4), 0x1_2345_6789, "uncompressed first");
        assert_eq!(u64_at(&b, x + 12), 5, "then compressed");
        assert_eq!(u16_at(&b, x + 20), 0x5455);
        // Same length with placeholder sizes: patchable in place.
        let mut p = Vec::new();
        local_header(&m, CMethod::Deflated, 45, &Sizes::default(), true, &mut p);
        assert_eq!(p.len(), b.len());
    }

    /// Parse the ZIP64 extra of a central record and return its u64 fields.
    fn central_zip64_fields(b: &[u8]) -> Option<Vec<u64>> {
        let name_len = usize::from(u16_at(b, 28));
        let extra_len = usize::from(u16_at(b, 30));
        let mut i = 46 + name_len;
        let end = i + extra_len;
        while i < end {
            let (id, len) = (u16_at(b, i), usize::from(u16_at(b, i + 2)));
            if id == ZIP64_EXTRA_ID {
                return Some((0..len / 8).map(|k| u64_at(b, i + 4 + 8 * k)).collect());
            }
            i += 4 + len;
        }
        None
    }

    #[test]
    fn central_zip64_only_overflowing_fields_in_order() {
        let m = meta("x", EntryType::File);
        let cases: [(u64, u64, u64, Option<Vec<u64>>); 5] = [
            (1, 2, 3, None),
            (0x1_0000_0000, 2, 3, Some(vec![0x1_0000_0000])),
            (1, 2, 0x2_0000_0000, Some(vec![0x2_0000_0000])),
            (
                0x1_0000_0000,
                0xFFFF_FFFF,
                3,
                Some(vec![0x1_0000_0000, 0xFFFF_FFFF]),
            ),
            (
                0x5_0000_0000,
                0x4_0000_0000,
                0x3_0000_0000,
                Some(vec![0x5_0000_0000, 0x4_0000_0000, 0x3_0000_0000]),
            ),
        ];
        for (usz, csz, off, want) in cases {
            let s = Sizes {
                crc: 7,
                compressed: csz,
                uncompressed: usz,
            };
            let mut b = Vec::new();
            central_header(&m, CMethod::Stored, 45, &s, off, &mut b);
            assert_eq!(u32_at(&b, 0), CENTRAL_SIG);
            assert_eq!(central_zip64_fields(&b), want, "{usz:x} {csz:x} {off:x}");
            let slot = |v: u64| if v >= U32_MAX { u32::MAX } else { v as u32 };
            assert_eq!(u32_at(&b, 20), slot(csz));
            assert_eq!(u32_at(&b, 24), slot(usz));
            assert_eq!(u32_at(&b, 42), slot(off));
            assert_eq!(central_needs_zip64(&s, off), want.is_some());
        }
    }

    #[test]
    fn central_record_fields() {
        let m = meta("ü/", EntryType::Dir);
        let mut b = Vec::new();
        central_header(&m, CMethod::Stored, 20, &Sizes::default(), 99, &mut b);
        assert_eq!(u16_at(&b, 4), (3 << 8) | 63, "made by Unix");
        assert_eq!(u16_at(&b, 8), FLAG_UTF8);
        let ext = u32_at(&b, 38);
        assert_eq!(ext >> 16, 0o040755, "dir type + permission bits only");
        assert_eq!(ext & 0xff, 0x10, "DOS directory bit");
        assert_eq!(u32_at(&b, 42), 99);
        assert_eq!(central_zip64_fields(&b), None);
    }

    #[test]
    fn external_attribute_defaults() {
        let mk = |kind, mode| Meta {
            name: "n",
            kind,
            mode,
            dos: (0, 0),
            ext_time: None,
        };
        assert_eq!(
            mk(EntryType::File, None).external_attributes() >> 16,
            0o100644
        );
        assert_eq!(
            mk(EntryType::Dir, None).external_attributes() >> 16,
            0o040755
        );
        assert_eq!(
            mk(EntryType::Symlink, None).external_attributes() >> 16,
            0o120777
        );
        assert_eq!(
            mk(EntryType::File, Some(0o100600)).external_attributes(),
            0o100600 << 16
        );
    }

    #[test]
    fn end_records_plain_and_zip64() {
        let mut b = Vec::new();
        end_records(3, 100, 50, &mut b);
        assert_eq!(b.len(), 22);
        assert_eq!(u32_at(&b, 0), EOCD_SIG);
        assert_eq!((u16_at(&b, 8), u16_at(&b, 10)), (3, 3));
        assert_eq!((u32_at(&b, 12), u32_at(&b, 16)), (50, 100));

        for (n, off, size) in [
            (70_000u64, 100u64, 50u64),
            (3, 0x1_0000_0000, 50),
            (0xFFFF, 5, 5),
        ] {
            let mut b = Vec::new();
            end_records(n, off, size, &mut b);
            assert_eq!(b.len(), 56 + 20 + 22);
            assert_eq!(u32_at(&b, 0), ZIP64_EOCD_SIG);
            assert_eq!(u64_at(&b, 4), 44);
            assert_eq!((u64_at(&b, 24), u64_at(&b, 32)), (n, n));
            assert_eq!((u64_at(&b, 40), u64_at(&b, 48)), (size, off));
            assert_eq!(u32_at(&b, 56), ZIP64_LOCATOR_SIG);
            assert_eq!(u64_at(&b, 64), off + size, "locator points at ZIP64 EOCD");
            assert_eq!(u32_at(&b, 72), 1);
            let e = &b[76..];
            assert_eq!(u32_at(e, 0), EOCD_SIG);
            let n16 = if n >= 0xFFFF { 0xFFFF } else { n as u16 };
            assert_eq!(u16_at(e, 10), n16);
            let o32 = if off >= U32_MAX { u32::MAX } else { off as u32 };
            assert_eq!(u32_at(e, 16), o32);
        }
    }

    #[test]
    fn versions() {
        assert_eq!(version_needed(EntryType::File, CMethod::Stored, false), 10);
        assert_eq!(
            version_needed(EntryType::File, CMethod::Deflated, false),
            20
        );
        assert_eq!(version_needed(EntryType::Dir, CMethod::Stored, false), 20);
        assert_eq!(version_needed(EntryType::File, CMethod::Stored, true), 45);
    }
}
