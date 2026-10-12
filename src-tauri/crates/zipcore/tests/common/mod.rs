//! Shared helpers for the zipcore integration tests.
//!
//! Each test binary pulls this in with `mod common;`, and not every binary uses
//! every helper.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::AtomicBool;

use zip::{CompressionMethod, ExtraField, ZipArchive};
use zipcore::{CompressOptions, Method, Progress, Summary, SymlinkPolicy};

/// Options with the given method and the defaults for everything else.
pub fn opts(method: Method) -> CompressOptions {
    CompressOptions {
        method,
        ..CompressOptions::default()
    }
}

pub fn opts_full(method: Method, symlinks: SymlinkPolicy, exclude_junk: bool) -> CompressOptions {
    CompressOptions {
        method,
        symlinks,
        exclude_junk,
        ..Default::default()
    }
}

/// Compress into memory, ignoring progress.
pub fn compress_mem(
    inputs: &[PathBuf],
    opts: &CompressOptions,
) -> zipcore::Result<(Vec<u8>, Summary)> {
    let mut progress = |_: &Progress| {};
    let cancel = AtomicBool::new(false);
    let mut cur = Cursor::new(Vec::new());
    let summary = zipcore::compress(inputs, &mut cur, opts, &mut progress, &cancel)?;
    Ok((cur.into_inner(), summary))
}

/// Compress into memory and panic with a useful message on error.
pub fn compress_ok(inputs: &[PathBuf], opts: &CompressOptions) -> (Vec<u8>, Summary) {
    match compress_mem(inputs, opts) {
        Ok(v) => v,
        Err(e) => panic!("compress failed for {inputs:?}: {e:?}"),
    }
}

/// Everything we care about for one archive entry. `data` is the fully read
/// (and therefore CRC-checked by the zip crate) uncompressed content.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub method: CompressionMethod,
    pub size: u64,
    pub compressed_size: u64,
    pub crc32: u32,
    pub unix_mode: Option<u32>,
    pub dos_time: Option<zip::DateTime>,
    /// Modification time from the extended-timestamp (0x5455) extra field in
    /// the central directory, if present.
    pub ext_mtime: Option<u32>,
    pub data: Vec<u8>,
}

/// Read every entry in order. Reading each entry to EOF makes the zip crate
/// verify its CRC-32; a mismatch is surfaced as a panic here.
pub fn read_entries(bytes: &[u8]) -> Vec<Entry> {
    let mut ar = ZipArchive::new(Cursor::new(bytes)).expect("zip crate could not open archive");
    let mut out = Vec::with_capacity(ar.len());
    for i in 0..ar.len() {
        let mut f = ar.by_index(i).expect("by_index");
        let name = f.name().expect("entry name is not valid").to_string();
        let mut ext_mtime = None;
        for field in f.extra_data_fields() {
            if let ExtraField::ExtendedTimestamp(ts) = field {
                ext_mtime = ts.mod_time();
            }
        }
        let mut data = Vec::new();
        f.read_to_end(&mut data)
            .unwrap_or_else(|e| panic!("reading entry {name:?} failed (CRC?): {e}"));
        out.push(Entry {
            is_dir: f.is_dir(),
            is_symlink: f.is_symlink(),
            method: f.compression(),
            size: f.size(),
            compressed_size: f.compressed_size(),
            crc32: f.crc32(),
            unix_mode: f.unix_mode(),
            dos_time: f.last_modified(),
            ext_mtime,
            data,
            name,
        });
    }
    out
}

pub fn names(entries: &[Entry]) -> Vec<String> {
    entries.iter().map(|e| e.name.clone()).collect()
}

pub fn by_name(entries: &[Entry]) -> BTreeMap<String, Entry> {
    entries
        .iter()
        .map(|e| (e.name.clone(), e.clone()))
        .collect()
}

pub fn find<'a>(entries: &'a [Entry], name: &str) -> &'a Entry {
    entries
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("entry {name:?} not found; have {:?}", names(entries)))
}

/// Check that every archive name is a safe, relative, forward-slash path.
pub fn assert_safe_names(entries: &[Entry]) {
    for e in entries {
        let n = &e.name;
        assert!(!n.is_empty(), "empty entry name");
        assert!(!n.starts_with('/'), "absolute entry name {n:?}");
        assert!(!n.contains('\\'), "backslash in entry name {n:?}");
        assert!(!n.contains(':'), "drive/colon in entry name {n:?}");
        let trimmed = n.strip_suffix('/').unwrap_or(n);
        for comp in trimmed.split('/') {
            assert!(!comp.is_empty(), "empty path component in {n:?}");
            assert_ne!(comp, "..", "parent component in {n:?}");
            assert_ne!(comp, ".", "current-dir component in {n:?}");
        }
        assert_eq!(
            e.is_dir,
            n.ends_with('/'),
            "dir flag/trailing slash mismatch for {n:?}"
        );
    }
    let mut seen = std::collections::HashSet::new();
    for e in entries {
        assert!(seen.insert(&e.name), "duplicate entry name {:?}", e.name);
    }
}

pub fn write_file(path: &Path, data: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, data).unwrap();
}

/// Deterministic pseudo-random bytes (xorshift64*), incompressible in practice.
pub fn random_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        s ^= s >> 12;
        s ^= s << 25;
        s ^= s >> 27;
        out.extend_from_slice(&s.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Highly compressible text.
pub fn compressible_bytes(len: usize) -> Vec<u8> {
    let pat = b"The quick brown fox jumps over the lazy dog. 0123456789\n";
    pat.iter().copied().cycle().take(len).collect()
}

/// A node of a filesystem snapshot, used to compare a source tree with an
/// extracted tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Dir,
    File(Vec<u8>),
    Symlink(PathBuf),
}

/// Snapshot `root` (not following symlinks) as relative forward-slash path -> node.
pub fn snapshot(root: &Path) -> BTreeMap<String, Node> {
    let mut map = BTreeMap::new();
    fn walk(base: &Path, dir: &Path, map: &mut BTreeMap<String, Node>) {
        for ent in fs::read_dir(dir).unwrap() {
            let ent = ent.unwrap();
            let p = ent.path();
            let rel = p
                .strip_prefix(base)
                .unwrap()
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let md = fs::symlink_metadata(&p).unwrap();
            if md.file_type().is_symlink() {
                map.insert(rel, Node::Symlink(fs::read_link(&p).unwrap()));
            } else if md.is_dir() {
                map.insert(rel, Node::Dir);
                walk(base, &p, map);
            } else {
                map.insert(rel, Node::File(fs::read(&p).unwrap()));
            }
        }
    }
    walk(root, root, &mut map);
    map
}

/// Snapshot `root` the way the archive should lay it out: the input's own
/// name is the first component, directories get a trailing slash.
pub fn expected_entries(root: &Path) -> BTreeMap<String, Node> {
    let top = root.file_name().unwrap().to_string_lossy().into_owned();
    let mut out = BTreeMap::new();
    let md = fs::symlink_metadata(root).unwrap();
    if md.is_dir() {
        out.insert(format!("{top}/"), Node::Dir);
        for (k, v) in snapshot(root) {
            let key = if v == Node::Dir {
                format!("{top}/{k}/")
            } else {
                format!("{top}/{k}")
            };
            out.insert(key, v);
        }
    } else {
        out.insert(top, Node::File(fs::read(root).unwrap()));
    }
    out
}

/// Archive contents in the same shape as [`expected_entries`].
pub fn archive_nodes(entries: &[Entry]) -> BTreeMap<String, Node> {
    entries
        .iter()
        .map(|e| {
            let node = if e.is_dir {
                Node::Dir
            } else if e.is_symlink {
                Node::Symlink(PathBuf::from(String::from_utf8(e.data.clone()).unwrap()))
            } else {
                Node::File(e.data.clone())
            };
            (e.name.clone(), node)
        })
        .collect()
}

/// Compare two node maps with a readable diff on failure (file contents are
/// summarized rather than dumped).
pub fn assert_nodes_eq(actual: &BTreeMap<String, Node>, expected: &BTreeMap<String, Node>) {
    let missing: Vec<_> = expected
        .keys()
        .filter(|k| !actual.contains_key(*k))
        .collect();
    let extra: Vec<_> = actual
        .keys()
        .filter(|k| !expected.contains_key(*k))
        .collect();
    let mut differing = Vec::new();
    for (k, v) in expected {
        if let Some(a) = actual.get(k) {
            if a != v {
                differing.push(format!("{k}: got {}, want {}", describe(a), describe(v)));
            }
        }
    }
    assert!(
        missing.is_empty() && extra.is_empty() && differing.is_empty(),
        "tree mismatch\n  missing: {missing:?}\n  unexpected: {extra:?}\n  differing: {differing:?}"
    );
}

fn describe(n: &Node) -> String {
    match n {
        Node::Dir => "dir".into(),
        Node::File(d) => format!("file({} bytes, crc-ish {:08x})", d.len(), fnv(d)),
        Node::Symlink(t) => format!("symlink -> {t:?}"),
    }
}

fn fnv(d: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in d {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Find an executable by name on PATH (or an absolute path that exists).
pub fn which(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.is_absolute() {
        return p.exists().then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for cand in [dir.join(name), dir.join(format!("{name}.exe"))] {
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// First available tool from the candidates, or print a skip note.
pub fn tool(candidates: &[&str], test: &str) -> Option<PathBuf> {
    for c in candidates {
        if let Some(p) = which(c) {
            return Some(p);
        }
    }
    eprintln!("NOTE: skipping {test}: none of {candidates:?} found on PATH");
    None
}

/// A Python 3 interpreter, verified to actually run (the Windows Store stub
/// `python3.exe` exists but does not).
pub fn python(test: &str) -> Option<PathBuf> {
    for c in ["python3", "python"] {
        if let Some(p) = which(c) {
            let ok = Command::new(&p)
                .args([
                    "-I",
                    "-c",
                    "import sys, zipfile; assert sys.version_info >= (3, 6)",
                ])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                return Some(p);
            }
        }
    }
    eprintln!("NOTE: skipping {test}: no working python3 found");
    None
}

pub fn run(cmd: &mut Command) -> Output {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {cmd:?}: {e}"));
    out
}

pub fn run_ok(cmd: &mut Command) -> Output {
    let out = run(cmd);
    assert!(
        out.status.success(),
        "{cmd:?} failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Run a Python snippet with `args` in `sys.argv[1:]`, returning stdout.
pub fn run_python(py: &Path, script: &str, args: &[&Path]) -> String {
    let mut cmd = Command::new(py);
    let script = format!("import sys\nsys.stdout.reconfigure(encoding='utf-8')\n{script}");
    cmd.arg("-I").arg("-c").arg(script);
    for a in args {
        cmd.arg(a);
    }
    let out = run_ok(&mut cmd);
    String::from_utf8(out.stdout).expect("python output not utf-8")
}

/// List the names in a directory (non-recursive), sorted.
pub fn list_dir(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// Seconds since the Unix epoch for a UTC civil date/time (proleptic Gregorian).
pub fn unix_secs(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    // Howard Hinnant's days_from_civil.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days * 86_400 + h * 3_600 + mi * 60 + s
}

/// Naive seconds for a DOS date/time (interpreted as if UTC).
pub fn dos_naive_secs(dt: &zip::DateTime) -> i64 {
    unix_secs(
        dt.year() as i64,
        dt.month() as i64,
        dt.day() as i64,
        dt.hour() as i64,
        dt.minute() as i64,
        dt.second() as i64,
    )
}

/// A small mixed tree used by several tests:
///
/// ```text
/// root/
///   a.txt            compressible text
///   empty.bin        0 bytes
///   bin/all_bytes    0..=255 repeated
///   bin/random.dat   incompressible
///   deep/x/y/z.txt
///   empty_dir/
///   with space/file name.txt
/// ```
pub fn mixed_tree(parent: &Path) -> PathBuf {
    let root = parent.join("root");
    write_file(&root.join("a.txt"), &compressible_bytes(10_000));
    write_file(&root.join("empty.bin"), b"");
    let all: Vec<u8> = (0..=255u8).cycle().take(256 * 17).collect();
    write_file(&root.join("bin/all_bytes"), &all);
    write_file(&root.join("bin/random.dat"), &random_bytes(70_001, 7));
    write_file(&root.join("deep/x/y/z.txt"), b"deep file\n");
    fs::create_dir_all(root.join("empty_dir")).unwrap();
    write_file(&root.join("with space/file name.txt"), b"spaces ok\r\n");
    root
}

/// Walk the local file headers from the start of the archive, as a streaming
/// unzipper (no central directory) would, returning each entry's raw name and
/// data. Fails if the local entries are not a contiguous sequence that ends
/// at the central directory.
pub fn stream_entries(bytes: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut r = Cursor::new(bytes);
    let mut out = Vec::new();
    loop {
        match zip::read::read_zipfile_from_stream(&mut r) {
            Ok(Some(mut f)) => {
                let name = f.name_raw().to_vec();
                let mut data = Vec::new();
                f.read_to_end(&mut data).unwrap_or_else(|e| {
                    panic!(
                        "stream read of {:?} failed: {e}",
                        String::from_utf8_lossy(&name)
                    )
                });
                out.push((name, data));
            }
            Ok(None) => return out,
            Err(e) => panic!(
                "streaming reader failed after {:?}: {e}",
                out.iter()
                    .map(|(n, _)| String::from_utf8_lossy(n).into_owned())
                    .collect::<Vec<_>>()
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 2 helpers: thread options, path output, timeouts, external validators,
// and an independent raw ZIP structure parser.
// ---------------------------------------------------------------------------

pub const MIB: usize = 1 << 20;

/// Options with the given method and thread count, defaults otherwise.
pub fn opts_t(method: Method, threads: usize) -> CompressOptions {
    CompressOptions {
        method,
        threads,
        ..CompressOptions::default()
    }
}

/// `compress_to_path` into `out`, returning the archive bytes. Panics on error.
pub fn compress_path_ok(
    inputs: &[PathBuf],
    out: &Path,
    opts: &CompressOptions,
) -> (Vec<u8>, Summary) {
    let cancel = AtomicBool::new(false);
    let summary = zipcore::compress_to_path(inputs, out, opts, &mut |_| {}, &cancel)
        .unwrap_or_else(|e| panic!("compress_to_path failed for {inputs:?}: {e:?}"));
    let bytes = fs::read(out).unwrap();
    assert_eq!(summary.bytes_out, bytes.len() as u64, "Summary.bytes_out");
    (bytes, summary)
}

/// Run `f` on a thread and fail if it does not finish within `secs`.
pub fn with_timeout<T: Send + 'static>(
    secs: u64,
    what: &str,
    f: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
        Ok(v) => v,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("{what}: did not finish within {secs}s (hang/deadlock?)")
        }
        Err(e) => panic!("{what}: worker thread died (panicked?): {e}"),
    }
}

/// `unzip -t` and Python `zipfile.testzip()` over an archive file, each skipped
/// with a note when the tool is missing. `expected_entries` is checked against
/// Python's entry count.
pub fn external_validate(zip_path: &Path, expected_entries: usize, test: &str) {
    if let Some(unzip) = tool(&["unzip"], test) {
        run_ok(Command::new(unzip).arg("-tq").arg(zip_path));
    }
    if let Some(py) = python(test) {
        let out = run_python(
            &py,
            "import zipfile\nwith zipfile.ZipFile(sys.argv[1]) as z:\n    print(len(z.infolist()), z.testzip())\n",
            &[zip_path],
        );
        assert_eq!(
            out.trim(),
            format!("{expected_entries} None"),
            "python zipfile on {zip_path:?}"
        );
    }
}

/// Streaming reader (local headers only) must see exactly the central
/// directory's entries, in order, with identical contents.
pub fn assert_stream_matches(bytes: &[u8], entries: &[Entry], how: &str) {
    let streamed = stream_entries(bytes);
    let s_names: Vec<String> = streamed
        .iter()
        .map(|(n, _)| String::from_utf8_lossy(n).into_owned())
        .collect();
    assert_eq!(
        s_names,
        names(entries),
        "{how}: streamed names != central directory"
    );
    for ((n, d), e) in streamed.iter().zip(entries) {
        if !e.is_dir {
            assert!(
                *d == e.data,
                "{how}: streamed data of {:?} differs ({} vs {} bytes)",
                String::from_utf8_lossy(n),
                d.len(),
                e.data.len()
            );
        }
    }
}

/// One central directory record with ZIP64 values resolved.
#[derive(Debug, Clone)]
pub struct RawCd {
    pub version_needed: u16,
    pub flags: u16,
    pub method: u16,
    pub time: u16,
    pub date: u16,
    pub crc: u32,
    pub csize: u64,
    pub usize: u64,
    pub offset: u64,
    pub name: Vec<u8>,
    pub extra: Vec<u8>,
    pub comment_len: u16,
}

/// One local file header with ZIP64 values resolved.
#[derive(Debug, Clone)]
pub struct RawLocal {
    pub flags: u16,
    pub method: u16,
    pub time: u16,
    pub date: u16,
    pub crc: u32,
    pub csize: u64,
    pub usize: u64,
    pub name: Vec<u8>,
    pub extra: Vec<u8>,
    /// Length of header + name + extra.
    pub header_len: u64,
}

#[derive(Debug, Clone)]
pub struct RawArchive {
    pub len: u64,
    pub comment: Vec<u8>,
    pub cd_offset: u64,
    pub cd_size: u64,
    /// Entry count from the (ZIP64, if present) end of central directory.
    pub total_entries: u64,
    /// 16-bit entry count in the classic EOCD record.
    pub eocd_entries16: u16,
    pub has_zip64_eocd: bool,
    /// Where the end records begin (ZIP64 EOCD if present, else EOCD).
    pub end_records_at: u64,
    pub entries: Vec<(RawCd, RawLocal)>,
}

fn u16le(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64le(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn read_at<R: std::io::Read + std::io::Seek>(r: &mut R, at: u64, len: usize) -> Vec<u8> {
    use std::io::SeekFrom;
    r.seek(SeekFrom::Start(at)).unwrap();
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)
        .unwrap_or_else(|e| panic!("raw read of {len} bytes at {at} failed: {e}"));
    buf
}

/// Find the body of extra field `id`.
pub fn extra_field(extra: &[u8], id: u16) -> Option<&[u8]> {
    let mut i = 0;
    while i + 4 <= extra.len() {
        let hid = u16le(extra, i);
        let len = u16le(extra, i + 2) as usize;
        let end = (i + 4 + len).min(extra.len());
        if hid == id {
            return Some(&extra[i + 4..end]);
        }
        i += 4 + len;
    }
    None
}

/// Parse an archive from scratch (EOCD, ZIP64 EOCD, central directory, and
/// every local header), independently of the zip crate.
pub fn parse_raw<R: std::io::Read + std::io::Seek>(r: &mut R) -> RawArchive {
    use std::io::SeekFrom;
    let len = r.seek(SeekFrom::End(0)).unwrap();
    assert!(len >= 22, "archive too short ({len} bytes)");
    let tail_len = len.min(22 + 65535) as usize;
    let tail_start = len - tail_len as u64;
    let tail = read_at(r, tail_start, tail_len);
    let mut eocd_rel = None;
    for i in (0..=tail_len - 22).rev() {
        if tail[i..i + 4] == *b"PK\x05\x06" {
            let clen = u16le(&tail, i + 20) as usize;
            if i + 22 + clen == tail_len {
                eocd_rel = Some(i);
                break;
            }
        }
    }
    let er = eocd_rel.expect("no end-of-central-directory record ending exactly at EOF");
    let eocd_pos = tail_start + er as u64;
    let e = &tail[er..];
    let entries16 = u16le(e, 10);
    let mut total = entries16 as u64;
    let mut cd_size = u32le(e, 12) as u64;
    let mut cd_offset = u32le(e, 16) as u64;
    let comment = e[22..22 + u16le(e, 20) as usize].to_vec();
    let mut has_zip64 = false;
    let mut end_records_at = eocd_pos;
    if eocd_pos >= 20 {
        let loc = read_at(r, eocd_pos - 20, 20);
        if loc[0..4] == *b"PK\x06\x07" {
            let z64_pos = u64le(&loc, 8);
            let z = read_at(r, z64_pos, 56);
            assert_eq!(
                &z[0..4],
                b"PK\x06\x06",
                "ZIP64 locator points at a non-ZIP64-EOCD"
            );
            let rec_size = u64le(&z, 4);
            assert_eq!(
                z64_pos + 12 + rec_size,
                eocd_pos - 20,
                "ZIP64 EOCD record does not end at the locator"
            );
            has_zip64 = true;
            end_records_at = z64_pos;
            total = u64le(&z, 32);
            assert_eq!(u64le(&z, 24), total, "ZIP64 EOCD: entries on disk != total");
            cd_size = u64le(&z, 40);
            cd_offset = u64le(&z, 48);
        }
    }
    if !has_zip64 {
        assert!(
            entries16 != 0xFFFF && cd_size != 0xFFFF_FFFF && cd_offset != 0xFFFF_FFFF,
            "EOCD has ZIP64 sentinels but there is no ZIP64 locator"
        );
    }
    assert_eq!(
        cd_offset + cd_size,
        end_records_at,
        "central directory does not end where the end records begin"
    );

    let cd = read_at(r, cd_offset, cd_size as usize);
    let mut p = 0usize;
    let mut entries = Vec::new();
    for idx in 0..total {
        assert!(
            p + 46 <= cd.len(),
            "central directory truncated at entry {idx}"
        );
        assert_eq!(
            &cd[p..p + 4],
            b"PK\x01\x02",
            "bad CD signature at entry {idx}"
        );
        let nlen = u16le(&cd, p + 28) as usize;
        let xlen = u16le(&cd, p + 30) as usize;
        let clen = u16le(&cd, p + 32);
        let name = cd[p + 46..p + 46 + nlen].to_vec();
        let extra = cd[p + 46 + nlen..p + 46 + nlen + xlen].to_vec();
        let mut usize_ = u32le(&cd, p + 24) as u64;
        let mut csize = u32le(&cd, p + 20) as u64;
        let mut offset = u32le(&cd, p + 42) as u64;
        let any_sentinel = usize_ == 0xFFFF_FFFF || csize == 0xFFFF_FFFF || offset == 0xFFFF_FFFF;
        if any_sentinel {
            let z = extra_field(&extra, 0x0001).unwrap_or_else(|| {
                panic!(
                    "CD entry {:?} has 0xFFFFFFFF fields but no ZIP64 extra",
                    String::from_utf8_lossy(&name)
                )
            });
            let mut q = 0;
            for v in [&mut usize_, &mut csize, &mut offset] {
                if *v == 0xFFFF_FFFF {
                    assert!(
                        q + 8 <= z.len(),
                        "ZIP64 extra of {:?} too short",
                        String::from_utf8_lossy(&name)
                    );
                    *v = u64le(z, q);
                    q += 8;
                }
            }
        }
        let c = RawCd {
            version_needed: u16le(&cd, p + 6),
            flags: u16le(&cd, p + 8),
            method: u16le(&cd, p + 10),
            time: u16le(&cd, p + 12),
            date: u16le(&cd, p + 14),
            crc: u32le(&cd, p + 16),
            csize,
            usize: usize_,
            offset,
            name,
            extra,
            comment_len: clen,
        };
        p += 46 + nlen + xlen + clen as usize;

        let h = read_at(r, c.offset, 30);
        assert_eq!(
            &h[0..4],
            b"PK\x03\x04",
            "no local header at the offset of {:?}",
            String::from_utf8_lossy(&c.name)
        );
        let lnlen = u16le(&h, 26) as usize;
        let lxlen = u16le(&h, 28) as usize;
        let ne = read_at(r, c.offset + 30, lnlen + lxlen);
        let lname = ne[..lnlen].to_vec();
        let lextra = ne[lnlen..].to_vec();
        let mut lc = u32le(&h, 18) as u64;
        let mut lu = u32le(&h, 22) as u64;
        if lc == 0xFFFF_FFFF || lu == 0xFFFF_FFFF {
            let z = extra_field(&lextra, 0x0001).unwrap_or_else(|| {
                panic!(
                    "local header of {:?} has 0xFFFFFFFF sizes but no ZIP64 extra",
                    String::from_utf8_lossy(&lname)
                )
            });
            if z.len() >= 16 {
                if lu == 0xFFFF_FFFF {
                    lu = u64le(z, 0);
                }
                if lc == 0xFFFF_FFFF {
                    lc = u64le(z, 8);
                }
            } else {
                assert!(z.len() >= 8, "local ZIP64 extra too short");
                if lu == 0xFFFF_FFFF {
                    lu = u64le(z, 0);
                } else {
                    lc = u64le(z, 0);
                }
            }
        }
        let l = RawLocal {
            flags: u16le(&h, 6),
            method: u16le(&h, 8),
            time: u16le(&h, 10),
            date: u16le(&h, 12),
            crc: u32le(&h, 14),
            csize: lc,
            usize: lu,
            name: lname,
            extra: lextra,
            header_len: (30 + lnlen + lxlen) as u64,
        };
        entries.push((c, l));
    }
    assert_eq!(
        p as u64, cd_size,
        "CD records do not fill the CD size exactly"
    );
    RawArchive {
        len,
        comment,
        cd_offset,
        cd_size,
        total_entries: total,
        eocd_entries16: entries16,
        has_zip64_eocd: has_zip64,
        end_records_at,
        entries,
    }
}

pub fn parse_raw_bytes(bytes: &[u8]) -> RawArchive {
    parse_raw(&mut Cursor::new(bytes))
}

/// Every structural invariant the Phase 2 writer promises, as a list of
/// problems (empty = fine):
/// - empty archive comment;
/// - each local header agrees with its CD record (name, method, flags, CRC,
///   sizes, time/date), and general-purpose bit 3 (data descriptor) is clear;
/// - stored entries have compressed == uncompressed size;
/// - local entries tile the file from offset 0 to the central directory with
///   no gaps, overlaps or unlisted bytes.
pub fn layout_problems(a: &RawArchive) -> Vec<String> {
    let mut probs = Vec::new();
    if !a.comment.is_empty() {
        probs.push(format!(
            "archive comment is not empty: {:?}",
            String::from_utf8_lossy(&a.comment)
        ));
    }
    if a.total_entries != a.entries.len() as u64 {
        probs.push("entry count mismatch".into());
    }
    let mut spans = Vec::new();
    for (c, l) in &a.entries {
        let n = String::from_utf8_lossy(&c.name).into_owned();
        if c.name != l.name {
            probs.push(format!(
                "{n}: local name {:?} != CD name",
                String::from_utf8_lossy(&l.name)
            ));
        }
        if c.method != l.method {
            probs.push(format!("{n}: method local {} != CD {}", l.method, c.method));
        }
        if c.flags != l.flags {
            probs.push(format!(
                "{n}: flags local {:#06x} != CD {:#06x}",
                l.flags, c.flags
            ));
        }
        if c.flags & 0x0008 != 0 || l.flags & 0x0008 != 0 {
            probs.push(format!("{n}: data-descriptor flag (bit 3) is set"));
        }
        if c.flags & 0x0001 != 0 {
            probs.push(format!("{n}: encrypted flag set"));
        }
        if c.crc != l.crc {
            probs.push(format!("{n}: CRC local {:08x} != CD {:08x}", l.crc, c.crc));
        }
        if c.csize != l.csize || c.usize != l.usize {
            probs.push(format!(
                "{n}: sizes local {}/{} != CD {}/{} (compressed/uncompressed)",
                l.csize, l.usize, c.csize, c.usize
            ));
        }
        if (c.time, c.date) != (l.time, l.date) {
            probs.push(format!("{n}: DOS time/date local != CD"));
        }
        if c.method == 0 && c.csize != c.usize {
            probs.push(format!(
                "{n}: stored but compressed {} != size {}",
                c.csize, c.usize
            ));
        }
        if c.method != 0 && c.method != 8 {
            probs.push(format!("{n}: unexpected method {}", c.method));
        }
        if c.comment_len != 0 {
            probs.push(format!("{n}: has an entry comment"));
        }
        if c.name.ends_with(b"/") && (c.usize != 0 || c.csize != 0) {
            probs.push(format!("{n}: directory entry with data"));
        }
        spans.push((c.offset, c.offset + l.header_len + c.csize, n));
    }
    spans.sort();
    let mut pos = 0u64;
    for (start, end, n) in &spans {
        if *start != pos {
            probs.push(format!(
                "{n}: local header at {start}, but previous entry ended at {pos} ({} unlisted/overlapping bytes)",
                *start as i64 - pos as i64
            ));
        }
        pos = *end;
    }
    if pos != a.cd_offset {
        probs.push(format!(
            "last entry ends at {pos} but the central directory starts at {}",
            a.cd_offset
        ));
    }
    probs
}

pub fn assert_layout(bytes: &[u8], how: &str) -> RawArchive {
    let a = parse_raw_bytes(bytes);
    let p = layout_problems(&a);
    assert!(
        p.is_empty(),
        "{how}: layout problems:\n  {}",
        p.join("\n  ")
    );
    a
}

/// Full in-process verification: zip crate read (CRC-checked) matches the
/// expected tree, streaming reader agrees, and the raw layout is sound.
pub fn verify_archive(bytes: &[u8], expected: &BTreeMap<String, Node>, how: &str) -> Vec<Entry> {
    let entries = read_entries(bytes);
    assert_safe_names(&entries);
    assert_nodes_eq(&archive_nodes(&entries), expected);
    assert_stream_matches(bytes, &entries, how);
    assert_layout(bytes, how);
    entries
}

/// Upper bound on the compressed size of a >= 4 MiB entry under the spec.
pub fn large_bound(size: u64) -> u64 {
    size + size / 13000 + 64
}

/// The engine's store-vs-deflate rule, checked per entry:
/// < 4 MiB: compressed < size when deflated, == size when stored;
/// >= 4 MiB: within [`large_bound`].
pub fn size_rule_problems(entries: &[Entry]) -> Vec<String> {
    let mut v = Vec::new();
    for e in entries.iter().filter(|e| !e.is_dir && !e.is_symlink) {
        let ok = if e.size < 4 * MIB as u64 {
            match e.method {
                CompressionMethod::Stored => e.compressed_size == e.size,
                CompressionMethod::Deflated => e.compressed_size < e.size,
                _ => false,
            }
        } else {
            e.compressed_size <= large_bound(e.size)
                && (e.method != CompressionMethod::Stored || e.compressed_size == e.size)
        };
        if !ok {
            v.push(format!(
                "{}: {} -> {} ({:?})",
                e.name, e.size, e.compressed_size, e.method
            ));
        }
    }
    v
}

// ---------------------------------------------------------------------------
// Phase 3 helpers: independent deflate sizing, realistic text, method checks,
// and extraction by external tools (unzip, bsdtar, ditto).
// ---------------------------------------------------------------------------

/// Raw deflate size of `data` at `level` with an independent encoder (flate2,
/// zlib-rs backend, default strategy / window / memLevel).
pub fn indep_deflate_len(data: &[u8], level: u32) -> usize {
    use std::io::Write;
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(level));
    e.write_all(data).unwrap();
    e.finish().unwrap().len()
}

/// Smallest independent raw deflate over levels 6..=9.
pub fn indep_best_6_to_9(data: &[u8]) -> usize {
    (6..=9).map(|l| indep_deflate_len(data, l)).min().unwrap()
}

/// Deterministic pseudo-English text (words drawn from a small vocabulary with
/// punctuation and line breaks): compressible like real prose, not a trivially
/// repeating pattern.
pub fn text_corpus(len: usize, seed: u64) -> Vec<u8> {
    const WORDS: &[&str] = &[
        "the",
        "of",
        "and",
        "to",
        "in",
        "a",
        "is",
        "that",
        "for",
        "it",
        "as",
        "was",
        "with",
        "be",
        "by",
        "on",
        "not",
        "he",
        "this",
        "are",
        "or",
        "his",
        "from",
        "at",
        "which",
        "but",
        "have",
        "an",
        "had",
        "they",
        "you",
        "were",
        "their",
        "one",
        "all",
        "we",
        "can",
        "her",
        "has",
        "there",
        "been",
        "if",
        "more",
        "when",
        "will",
        "would",
        "who",
        "so",
        "no",
        "compression",
        "archive",
        "deflate",
        "window",
        "huffman",
        "literal",
        "distance",
        "length",
        "block",
        "encoder",
        "decoder",
        "stream",
        "header",
        "central",
        "directory",
        "entry",
        "file",
        "folder",
        "byte",
        "table",
        "symbol",
        "match",
        "lazy",
        "greedy",
        "optimal",
        "parser",
        "squeeze",
        "iteration",
        "probability",
        "entropy",
    ];
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = move || {
        s ^= s >> 12;
        s ^= s << 25;
        s ^= s >> 27;
        s.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    let mut out = Vec::with_capacity(len + 32);
    let mut col = 0usize;
    let mut cap = true;
    while out.len() < len {
        let r = next();
        let w = WORDS[(r % WORDS.len() as u64) as usize];
        if cap {
            let mut b = w.as_bytes().to_vec();
            b[0] = b[0].to_ascii_uppercase();
            out.extend_from_slice(&b);
            cap = false;
        } else {
            out.extend_from_slice(w.as_bytes());
        }
        col += w.len() + 1;
        match (r >> 32) % 23 {
            0 => {
                out.extend_from_slice(b". ");
                cap = true;
            }
            1 => out.extend_from_slice(b", "),
            2 => {
                let n = (r >> 40) % 10_000;
                out.extend_from_slice(format!(" {n} ").as_bytes());
            }
            _ => out.push(b' '),
        }
        if col > 72 {
            out.push(b'\n');
            col = 0;
        }
    }
    out.truncate(len);
    out
}

/// Source-code-like bytes (repetitive structure with varying identifiers).
pub fn source_like(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 128);
    let mut i = seed;
    while out.len() < len {
        i = i.wrapping_add(1);
        let name = format!("item_{}", i.wrapping_mul(2_654_435_761) % 997);
        out.extend_from_slice(
            format!(
                "pub fn {name}(x: u32) -> u32 {{\n    // compute {name}\n    let y = x.wrapping_mul({});\n    if y > {} {{ y - 1 }} else {{ y + {} }}\n}}\n\n",
                i % 31 + 1,
                i % 1000,
                i % 7
            )
            .as_bytes(),
        );
    }
    out.truncate(len);
    out
}

/// Every entry (central and local header) uses method 0 (Stored) or 8
/// (Deflate): the product rule.
pub fn assert_methods_store_or_deflate(bytes: &[u8], how: &str) {
    let a = parse_raw_bytes(bytes);
    for (cd, local) in &a.entries {
        let name = String::from_utf8_lossy(&cd.name);
        assert!(
            cd.method == 0 || cd.method == 8,
            "{how}: {name}: central method {} is not Stored/Deflate",
            cd.method
        );
        assert_eq!(
            cd.method, local.method,
            "{how}: {name}: central/local method mismatch"
        );
    }
}

/// Builds an extraction command into the given directory.
type ExtractCmd = Box<dyn Fn(&Path) -> Command>;

/// Extract `zip_path` with every available external extractor (unzip, bsdtar,
/// and ditto on macOS) into fresh directories and compare each result with
/// the snapshot of `src_parent` (the directory containing the inputs).
pub fn assert_external_extracts(zip_path: &Path, src_parent: &Path, work: &Path, test: &str) {
    let expected = snapshot(src_parent);
    let mut tools: Vec<(&str, ExtractCmd)> = Vec::new();
    if let Some(u) = tool(&["unzip"], test) {
        let zp = zip_path.to_path_buf();
        tools.push((
            "unzip",
            Box::new(move |d: &Path| {
                let mut c = Command::new(&u);
                c.arg("-q").arg(&zp).arg("-d").arg(d);
                c
            }),
        ));
    }
    if let Some(b) = tool(&["bsdtar"], test) {
        let zp = zip_path.to_path_buf();
        tools.push((
            "bsdtar",
            Box::new(move |d: &Path| {
                let mut c = Command::new(&b);
                c.arg("-xf").arg(&zp).arg("-C").arg(d);
                c
            }),
        ));
    }
    if cfg!(target_os = "macos") {
        if let Some(dt) = tool(&["ditto"], test) {
            let zp = zip_path.to_path_buf();
            tools.push((
                "ditto",
                Box::new(move |d: &Path| {
                    let mut c = Command::new(&dt);
                    c.arg("-x").arg("-k").arg(&zp).arg(d);
                    c
                }),
            ));
        }
    }
    for (i, (name, mk)) in tools.iter().enumerate() {
        let dir = work.join(format!("x{i}_{name}"));
        fs::create_dir_all(&dir).unwrap();
        run_ok(&mut mk(&dir));
        let got = snapshot(&dir);
        let missing: Vec<_> = expected.keys().filter(|k| !got.contains_key(*k)).collect();
        let extra: Vec<_> = got.keys().filter(|k| !expected.contains_key(*k)).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{test}: {name} extraction differs: missing {missing:?}, extra {extra:?}"
        );
        for (k, v) in &expected {
            assert!(
                got[k] == *v,
                "{test}: {name} extracted {k:?} with different content"
            );
        }
    }
}

/// Total compressed size of the file entries.
pub fn total_compressed(entries: &[Entry]) -> u64 {
    entries
        .iter()
        .filter(|e| !e.is_dir && !e.is_symlink)
        .map(|e| e.compressed_size)
        .sum()
}
