//! Gitignore-style path filtering: `CompressOptions::exclude`,
//! `CompressOptions::respect_gitignore` and `DiffOptions::exclude`.
//!
//! Pattern syntax and semantics are git's (`man gitignore`), implemented by
//! the `ignore` crate's gitignore matcher (the one ripgrep uses): `*`, `?`,
//! `[a-z]`, `**`, a leading `/` anchors to the root, a trailing `/` matches
//! directories only, a pattern containing a `/` elsewhere is anchored too,
//! `!` re-includes, `#` starts a comment, `\` escapes. Patterns use `/` on
//! every platform. Matching is case-insensitive on Windows (as git is there by
//! default, `core.ignorecase`) and case-sensitive elsewhere.
//!
//! As in git, a path inside an excluded directory is excluded whatever later
//! patterns say: the directory is never looked into.

use std::io;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::Match;

use crate::{Error, Result};

/// Whether patterns ignore case on this platform.
const CASE_INSENSITIVE: bool = cfg!(windows);

fn builder() -> GitignoreBuilder {
    // Root "." means "match the paths as given": callers always pass paths
    // relative to the directory the patterns belong to.
    let mut b = GitignoreBuilder::new(".");
    // Infallible despite the signature (it only sets a flag).
    let _ = b.case_insensitive(CASE_INSENSITIVE);
    b
}

/// A compiled list of gitignore-style patterns, in order (later patterns win,
/// so `!keep.log` after `*.log` re-includes `keep.log`).
#[derive(Debug, Clone)]
pub(crate) struct Patterns {
    gi: Gitignore,
}

impl Patterns {
    /// Compile `patterns`. Blank lines and `#` comments are allowed and
    /// ignored; a pattern that isn't a valid glob (e.g. a trailing lone `\`) is an
    /// [`Error::Io`] with kind `InvalidInput` that names the pattern.
    pub(crate) fn new(patterns: &[String]) -> Result<Patterns> {
        let mut b = builder();
        for p in patterns {
            b.add_line(None, p).map_err(|e| invalid_pattern(p, &e))?;
        }
        let gi = b.build().map_err(|e| invalid_pattern("", &e))?;
        Ok(Patterns { gi })
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.gi.is_empty()
    }

    /// How the patterns judge `rel` itself (relative to the patterns' root,
    /// `/` or native separators). Does not look at parent directories; for
    /// top-down walks that never descend into excluded directories.
    fn judge(&self, rel: &Path, is_dir: bool) -> Match<()> {
        match self.gi.matched(rel, is_dir) {
            Match::None => Match::None,
            Match::Ignore(_) => Match::Ignore(()),
            Match::Whitelist(_) => Match::Whitelist(()),
        }
    }

    /// Whether `rel` itself is excluded (parents not considered).
    pub(crate) fn excludes(&self, rel: &Path, is_dir: bool) -> bool {
        matches!(self.judge(rel, is_dir), Match::Ignore(())) && !rel.as_os_str().is_empty()
    }

    /// Whether `rel` or any of its parent directories is excluded: the same
    /// answer a walk that skips excluded directories gives, for a flat list
    /// of paths (e.g. archive entries).
    pub(crate) fn excludes_path_or_parent(&self, rel: &Path, is_dir: bool) -> bool {
        if self.is_empty() {
            return false;
        }
        let ancestors: Vec<&Path> = rel
            .ancestors()
            .skip(1)
            .filter(|a| !a.as_os_str().is_empty())
            .collect();
        ancestors.iter().rev().any(|a| self.excludes(a, true)) || self.excludes(rel, is_dir)
    }

    /// [`Patterns::excludes_path_or_parent`] for an archive entry name
    /// (forward slashes; directories end in `/`), relative to the root the
    /// patterns apply to.
    pub(crate) fn excludes_archive_name(&self, name: &str) -> bool {
        let is_dir = name.ends_with('/');
        let rel = name.trim_end_matches('/');
        !rel.is_empty() && self.excludes_path_or_parent(Path::new(rel), is_dir)
    }
}

fn invalid_pattern(pattern: &str, e: &ignore::Error) -> Error {
    Error::Io {
        path: PathBuf::from(pattern),
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid exclude pattern: {e}"),
        ),
    }
}

/// Parse the contents of a `.gitignore` file. Lines that aren't valid globs
/// are skipped, as git skips them.
fn parse_gitignore(text: &str) -> Gitignore {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut b = builder();
    for line in text.lines() {
        let _ = b.add_line(None, line);
    }
    b.build().unwrap_or_else(|_| Gitignore::empty())
}

/// One directory's `.gitignore` on the walk's stack.
struct Level {
    /// Depth of the directory holding the `.gitignore` (the input root is 0).
    depth: usize,
    /// That directory, relative to the input root.
    dir: PathBuf,
    gi: Gitignore,
}

/// Decides, entry by entry, what a top-down, depth-first walk of one input
/// keeps: `exclude_junk`, `exclude` and (optionally) `.gitignore` files.
///
/// Call [`TreeFilter::keep`] for every entry in walk order (pre-order, as
/// walkdir yields them), and [`TreeFilter::enter_dir`] for every directory
/// that was kept before its children are visited. Skip the contents of a
/// directory that isn't kept.
pub(crate) struct TreeFilter<'a> {
    exclude: &'a Patterns,
    exclude_junk: bool,
    gitignore: bool,
    stack: Vec<Level>,
}

impl<'a> TreeFilter<'a> {
    pub(crate) fn new(exclude: &'a Patterns, exclude_junk: bool, gitignore: bool) -> Self {
        TreeFilter {
            exclude,
            exclude_junk,
            gitignore,
            stack: Vec::new(),
        }
    }

    /// Whether to keep the entry at `rel` (relative to the input root;
    /// `depth` is its number of components). The root itself (depth 0) is
    /// always kept: an input named explicitly is never filtered.
    pub(crate) fn keep(&mut self, rel: &Path, depth: usize, is_dir: bool) -> bool {
        // Drop the .gitignore files of directories we have left. What remains
        // are this entry's ancestors (at most one level per depth).
        while self.stack.last().is_some_and(|l| l.depth >= depth) {
            self.stack.pop();
        }
        if depth == 0 {
            return true;
        }
        let name = rel.file_name().unwrap_or(rel.as_os_str());
        if self.exclude_junk && crate::walk::is_junk(name) {
            return false;
        }
        if self.gitignore && name.to_string_lossy().eq_ignore_ascii_case(".git") {
            return false;
        }
        if self.exclude.excludes(rel, is_dir) {
            return false;
        }
        if self.gitignore {
            // The deepest .gitignore with an opinion decides, as in git.
            for level in self.stack.iter().rev() {
                let sub = rel.strip_prefix(&level.dir).unwrap_or(rel);
                match level.gi.matched(sub, is_dir) {
                    Match::None => continue,
                    Match::Ignore(_) => return false,
                    Match::Whitelist(_) => return true,
                }
            }
        }
        true
    }

    /// Load the `.gitignore` of a kept directory (`abs` on disk, `rel`
    /// relative to the input root) so it applies to the directory's subtree.
    /// A missing file is fine; one that exists but can't be read is an error.
    pub(crate) fn enter_dir(&mut self, abs: &Path, rel: &Path, depth: usize) -> Result<()> {
        if !self.gitignore {
            return Ok(());
        }
        let path = abs.join(".gitignore");
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::IsADirectory
                ) || path.is_dir() =>
            {
                return Ok(())
            }
            Err(source) => return Err(Error::Io { path, source }),
        };
        let gi = parse_gitignore(&String::from_utf8_lossy(&bytes));
        if !gi.is_empty() {
            self.stack.push(Level {
                depth,
                dir: rel.to_path_buf(),
                gi,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pats(p: &[&str]) -> Patterns {
        Patterns::new(&p.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn basic_patterns() {
        let p = pats(&["*.log", "target/", "/build", "docs/**/*.tmp", "!keep.log"]);
        assert!(p.excludes(Path::new("a.log"), false));
        assert!(p.excludes(Path::new("x/y/a.log"), false));
        assert!(!p.excludes(Path::new("keep.log"), false));
        assert!(!p.excludes(Path::new("x/keep.log"), false));
        assert!(p.excludes(Path::new("target"), true));
        assert!(!p.excludes(Path::new("target"), false));
        assert!(p.excludes(Path::new("sub/target"), true));
        assert!(p.excludes(Path::new("build"), true));
        assert!(!p.excludes(Path::new("sub/build"), true));
        assert!(p.excludes(Path::new("docs/a/b/c.tmp"), false));
        assert!(p.excludes(Path::new("docs/c.tmp"), false));
        assert!(!p.excludes(Path::new("other/c.tmp"), false));
        assert!(!p.excludes(Path::new(""), true));
    }

    #[test]
    fn parents_and_archive_names() {
        let p = pats(&["node_modules/", "!node_modules/keep.js"]);
        assert!(p.excludes_archive_name("node_modules/"));
        // Git can't re-include a file inside an excluded directory.
        assert!(p.excludes_archive_name("node_modules/keep.js"));
        assert!(p.excludes_archive_name("a/node_modules/x/y.js"));
        assert!(!p.excludes_archive_name("a/node_modules_x/y.js"));
        assert!(!p.excludes_archive_name("src/"));
        assert!(!p.excludes_archive_name(""));
        assert!(!pats(&[]).excludes_archive_name("a/b"));
    }

    #[test]
    fn invalid_pattern_is_an_error() {
        let err = Patterns::new(&["a\\".to_string()]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("a\\") && msg.contains("invalid exclude pattern"),
            "{msg}"
        );
        // As in git, an unclosed `[` is literal rather than an error.
        assert!(pats(&["a["]).excludes(Path::new("a["), false));
    }

    #[test]
    fn comments_and_blanks_are_ignored() {
        let p = pats(&["", "# *.txt", "\\#x"]);
        assert!(!p.excludes(Path::new("a.txt"), false));
        assert!(p.excludes(Path::new("#x"), false));
    }

    #[test]
    fn case_follows_the_platform() {
        let p = pats(&["*.LOG"]);
        assert_eq!(p.excludes(Path::new("a.log"), false), cfg!(windows));
    }

    #[test]
    fn gitignore_text() {
        let gi = parse_gitignore("\u{feff}*.o\r\nbad[\r\n!keep.o\n");
        assert!(gi.matched("a.o", false).is_ignore());
        assert!(gi.matched("keep.o", false).is_whitelist());
    }
}
