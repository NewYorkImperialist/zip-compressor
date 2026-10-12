//! Pre-scan of the inputs: turns the input list into a flat, sorted list of
//! archive entries (with sizes, so progress totals are known up front).

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use walkdir::WalkDir;

use crate::{CompressOptions, Error, Result, SymlinkPolicy};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    Dir,
    File { size: u64 },
    Symlink { target: String },
}

#[derive(Debug, Clone)]
pub(crate) struct Entry {
    /// Archive path, forward slashes; directories end in `/`.
    pub name: String,
    /// Where the data comes from on disk.
    pub path: PathBuf,
    pub kind: Kind,
    /// Unix mode bits (permission bits are what we keep), if available.
    pub mode: Option<u32>,
    pub mtime: Option<SystemTime>,
}

/// Names that `exclude_junk` leaves out. `.DS_Store`, `__MACOSX` and AppleDouble
/// `._*` are matched exactly (they're macOS names on case-sensitive-or-not
/// volumes alike); the Windows ones are matched case-insensitively, as Windows
/// itself treats them.
pub(crate) fn is_junk(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name == ".DS_Store"
        || name == "__MACOSX"
        || (name.starts_with("._") && name.len() > 2)
        || name.eq_ignore_ascii_case("Thumbs.db")
        || name.eq_ignore_ascii_case("desktop.ini")
}

/// Name an input appears under at the top of the archive: its own final path
/// component. Paths without one (`.`, `..`, `foo/..`) are canonicalized first.
pub(crate) fn top_level_name(input: &Path) -> Result<String> {
    let ends_in_normal = matches!(input.components().next_back(), Some(Component::Normal(_)));
    if ends_in_normal {
        if let Some(n) = input.file_name() {
            return Ok(n.to_string_lossy().into_owned());
        }
    }
    let canon = input.canonicalize().map_err(|source| Error::Io {
        path: input.to_path_buf(),
        source,
    })?;
    match canon.file_name() {
        Some(n) => Ok(n.to_string_lossy().into_owned()),
        None => Err(Error::Io {
            path: input.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "input has no name to store it under (filesystem root?)",
            ),
        }),
    }
}

/// Archive path for `rel` (a path relative to the input root, as yielded by
/// walkdir) under `top`. Only normal components are kept, so the result never
/// contains `..`, `.`, a root, or a drive prefix; components are joined with `/`.
/// Non-UTF-8 components are converted lossily.
pub(crate) fn archive_name(top: &str, rel: &Path) -> String {
    let mut name = String::from(top);
    for c in rel.components() {
        if let Component::Normal(part) = c {
            name.push('/');
            name.push_str(&part.to_string_lossy());
        }
    }
    name
}

fn io_err(path: &Path, source: io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn walk_err(root: &Path, e: walkdir::Error) -> Error {
    let path = e.path().unwrap_or(root).to_path_buf();
    let not_found = e
        .io_error()
        .is_some_and(|io| io.kind() == io::ErrorKind::NotFound);
    let source = if not_found {
        io::Error::new(
            io::ErrorKind::NotFound,
            "not found (dangling symlink, or removed while scanning)",
        )
    } else {
        io::Error::from(e)
    };
    Error::Io { path, source }
}

#[cfg(unix)]
fn mode_of(md: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(md.permissions().mode())
}

#[cfg(not(unix))]
fn mode_of(_md: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Whether symlinks are followed during the walk. On non-Unix platforms
/// symlink entries aren't representable in a meaningful way, so `Store`
/// behaves like `Follow` there.
fn follow_links(policy: SymlinkPolicy) -> bool {
    match policy {
        SymlinkPolicy::Follow => true,
        SymlinkPolicy::Skip => false,
        SymlinkPolicy::Store => cfg!(not(unix)),
    }
}

/// Walk every input and return the entries in archive order.
///
/// - Each input is listed under its own name; children are sorted by file name.
/// - An input that is itself a symlink is always resolved (the user named it
///   explicitly); the policy applies to links found inside directories.
/// - `exclude_junk` applies to descendants only, never to an input named explicitly.
/// - Sockets, FIFOs and device nodes are skipped (reading a FIFO would block).
/// - With `Follow`, links that would re-enter an ancestor directory (cycles) are
///   skipped; a dangling link is an error.
pub(crate) fn scan(
    inputs: &[PathBuf],
    opts: &CompressOptions,
    cancel: &AtomicBool,
) -> Result<Vec<Entry>> {
    let follow = follow_links(opts.symlinks);
    let mut seen: HashSet<String> = HashSet::new();
    let mut entries = Vec::new();

    for input in inputs {
        match std::fs::symlink_metadata(input) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Error::NotFound(input.clone()))
            }
            Err(e) => return Err(io_err(input, e)),
        }
        // Resolve a top-level symlink now so a dangling one reports NotFound.
        if let Err(e) = std::fs::metadata(input) {
            return Err(if e.kind() == io::ErrorKind::NotFound {
                Error::NotFound(input.clone())
            } else {
                io_err(input, e)
            });
        }

        let top = top_level_name(input)?;
        if !seen.insert(top.clone()) {
            return Err(Error::DuplicateName(top));
        }

        let exclude_junk = opts.exclude_junk;
        let walker = WalkDir::new(input)
            .follow_links(follow)
            .follow_root_links(true)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(move |e| !(exclude_junk && e.depth() > 0 && is_junk(e.file_name())));

        for item in walker {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let dent = match item {
                Ok(d) => d,
                Err(e) if e.loop_ancestor().is_some() => continue,
                Err(e) => return Err(walk_err(input, e)),
            };
            let ft = dent.file_type();
            let rel = dent.path().strip_prefix(input).unwrap_or(Path::new(""));
            let name = archive_name(&top, rel);

            let kind = if ft.is_dir() {
                Kind::Dir
            } else if ft.is_symlink() {
                // Only reachable when not following links.
                match opts.symlinks {
                    SymlinkPolicy::Skip => continue,
                    _ => {
                        let target =
                            std::fs::read_link(dent.path()).map_err(|e| io_err(dent.path(), e))?;
                        Kind::Symlink {
                            target: target.to_string_lossy().into_owned(),
                        }
                    }
                }
            } else if ft.is_file() {
                Kind::File { size: 0 }
            } else {
                continue;
            };

            let name = if kind == Kind::Dir { name + "/" } else { name };
            let md = dent.metadata().map_err(|e| walk_err(dent.path(), e))?;
            let kind = match kind {
                Kind::File { .. } => Kind::File { size: md.len() },
                k => k,
            };
            entries.push(Entry {
                name,
                path: dent.path().to_path_buf(),
                kind,
                mode: mode_of(&md),
                mtime: md.modified().ok(),
            });
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junk_names() {
        for j in [
            ".DS_Store",
            "__MACOSX",
            "._foo",
            "._.DS_Store",
            "Thumbs.db",
            "thumbs.DB",
            "desktop.ini",
            "Desktop.ini",
        ] {
            assert!(is_junk(OsStr::new(j)), "{j} should be junk");
        }
        for ok in [
            "DS_Store",
            ".DS_Store.txt",
            "__MACOSX2",
            "._",
            "_foo",
            ".foo",
            "Thumbs.db.bak",
            "notes.txt",
        ] {
            assert!(!is_junk(OsStr::new(ok)), "{ok} should not be junk");
        }
    }

    #[test]
    fn archive_names_use_forward_slashes_and_drop_odd_components() {
        assert_eq!(archive_name("photos", Path::new("")), "photos");
        assert_eq!(
            archive_name("photos", &Path::new("2024").join("a.jpg")),
            "photos/2024/a.jpg"
        );
        assert_eq!(archive_name("t", Path::new("./a/../b")), "t/a/b");
        assert_eq!(archive_name("t", Path::new("/abs/x")), "t/abs/x");
        assert_eq!(archive_name("ünï", Path::new("ç.txt")), "ünï/ç.txt");
    }

    #[test]
    fn top_level_names() {
        assert_eq!(
            top_level_name(Path::new("/a/photos")).unwrap(),
            "photos".to_string()
        );
        assert_eq!(
            top_level_name(Path::new("/b/notes.txt")).unwrap(),
            "notes.txt".to_string()
        );
        assert_eq!(
            top_level_name(Path::new("rel/dir/")).unwrap(),
            "dir".to_string()
        );
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            top_level_name(Path::new(".")).unwrap(),
            cwd.file_name().unwrap().to_string_lossy()
        );
        #[cfg(unix)]
        assert!(top_level_name(Path::new("/")).is_err());
    }

    #[test]
    fn follow_policy() {
        assert!(follow_links(SymlinkPolicy::Follow));
        assert!(!follow_links(SymlinkPolicy::Skip));
        assert_eq!(follow_links(SymlinkPolicy::Store), cfg!(not(unix)));
    }
}
