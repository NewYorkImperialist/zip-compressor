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
