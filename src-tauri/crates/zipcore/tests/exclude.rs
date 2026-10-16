//! `CompressOptions::exclude` and `CompressOptions::respect_gitignore`.

mod common;

use std::path::{Path, PathBuf};

use common::{compress_mem, compress_ok, names, read_entries, write_file};
use zipcore::{CompressOptions, Error};

fn opts(exclude: &[&str], gitignore: bool) -> CompressOptions {
    CompressOptions {
        exclude: exclude.iter().map(|s| s.to_string()).collect(),
        respect_gitignore: gitignore,
        ..CompressOptions::default()
    }
}

fn names_of(inputs: &[PathBuf], o: &CompressOptions) -> Vec<String> {
    let (bytes, _) = compress_ok(inputs, o);
    names(&read_entries(&bytes))
}

fn touch(root: &Path, rels: &[&str]) {
    for r in rels {
        if let Some(dir) = r.strip_suffix('/') {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        } else {
            write_file(&root.join(r), r.as_bytes());
        }
    }
}

/// `names` minus the given ones, for "everything except" assertions.
fn without(all: &[&str], gone: &[&str]) -> Vec<String> {
    all.iter()
        .filter(|n| !gone.contains(n))
        .map(|s| s.to_string())
        .collect()
}

const TREE: &[&str] = &[
    "p/",
    "p/a.log",
    "p/a.txt",
    "p/b.dat",
    "p/build/",
    "p/build/out.o",
    "p/cache",
    "p/docs/",
    "p/docs/x/",
    "p/docs/x/y.tmp",
    "p/docs/z.tmp",
    "p/keep.log",
    "p/logs/",
    "p/logs/l1",
    "p/src/",
    "p/src/build/",
    "p/src/build/gen.rs",
    "p/src/cache/",
    "p/src/cache/c",
    "p/src/main.rs",
    "p/src/node_modules/",
    "p/src/node_modules/m.js",
    "p/src/x.log",
    "p/z.tmp",
];

fn tree() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let rels: Vec<&str> = TREE.iter().map(|n| n.strip_prefix("p/").unwrap()).collect();
    let p = tmp.path().join("p");
    std::fs::create_dir(&p).unwrap();
    touch(
        &p,
        &rels
            .into_iter()
            .filter(|r| !r.is_empty())
            .collect::<Vec<_>>(),
    );
    (tmp, p)
}

#[test]
fn no_patterns_keeps_everything() {
    let (_t, p) = tree();
    assert_eq!(names_of(&[p], &opts(&[], false)), TREE);
}

#[test]
fn globs_match_at_any_depth() {
    let (_t, p) = tree();
    assert_eq!(
        names_of(std::slice::from_ref(&p), &opts(&["*.log"], false)),
        without(TREE, &["p/a.log", "p/keep.log", "p/src/x.log"])
    );
    assert_eq!(
        names_of(
            std::slice::from_ref(&p),
            &opts(&["?.txt", "[ab].dat"], false)
        ),
        without(TREE, &["p/a.txt", "p/b.dat"])
    );
    // A bare name matches files and directories alike, anywhere.
    assert_eq!(
        names_of(&[p], &opts(&["cache"], false)),
        without(TREE, &["p/cache", "p/src/cache/", "p/src/cache/c"])
    );
}

#[test]
fn trailing_slash_matches_directories_only() {
    let (_t, p) = tree();
    assert_eq!(
        names_of(&[p], &opts(&["cache/"], false)),
        without(TREE, &["p/src/cache/", "p/src/cache/c"])
    );
}

#[test]
fn leading_or_inner_slash_anchors_to_the_input_root() {
    let (_t, p) = tree();
    assert_eq!(
        names_of(std::slice::from_ref(&p), &opts(&["/build"], false)),
        without(TREE, &["p/build/", "p/build/out.o"])
    );
    assert_eq!(
        names_of(std::slice::from_ref(&p), &opts(&["build/"], false)),
        without(
            TREE,
            &[
                "p/build/",
                "p/build/out.o",
                "p/src/build/",
                "p/src/build/gen.rs"
            ]
        )
    );
    assert_eq!(
        names_of(std::slice::from_ref(&p), &opts(&["src/main.rs"], false)),
        without(TREE, &["p/src/main.rs"])
    );
    // The input's own name is not part of the matched path.
    assert_eq!(names_of(&[p], &opts(&["p/a.txt", "/p"], false)), TREE);
}

#[test]
fn double_star() {
    let (_t, p) = tree();
    assert_eq!(
        names_of(std::slice::from_ref(&p), &opts(&["**/node_modules"], false)),
        without(TREE, &["p/src/node_modules/", "p/src/node_modules/m.js"])
    );
    assert_eq!(
        names_of(std::slice::from_ref(&p), &opts(&["docs/**/*.tmp"], false)),
        without(TREE, &["p/docs/x/y.tmp", "p/docs/z.tmp"])
    );
    // `dir/**` leaves out the contents but keeps the directory.
    assert_eq!(
        names_of(&[p], &opts(&["logs/**"], false)),
        without(TREE, &["p/logs/l1"])
    );
}

#[test]
fn negation_reincludes_but_not_inside_an_excluded_dir() {
    let (_t, p) = tree();
    assert_eq!(
        names_of(
            std::slice::from_ref(&p),
            &opts(&["*.log", "!keep.log"], false)
        ),
        without(TREE, &["p/a.log", "p/src/x.log"])
    );
    // Order matters: a later pattern wins.
    assert_eq!(
        names_of(
            std::slice::from_ref(&p),
            &opts(&["!keep.log", "*.log"], false)
        ),
        without(TREE, &["p/a.log", "p/keep.log", "p/src/x.log"])
    );
    // As in git, nothing inside an excluded directory can be re-included.
    assert_eq!(
        names_of(&[p], &opts(&["src/", "!src/main.rs"], false)),
        TREE.iter()
            .filter(|n| !n.starts_with("p/src/"))
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    );
}

#[test]
fn explicit_inputs_are_never_excluded() {
    let tmp = tempfile::tempdir().unwrap();
    touch(tmp.path(), &["a.log", "build/build/x", "build/y"]);
    let inputs = [tmp.path().join("a.log"), tmp.path().join("build")];
    assert_eq!(
        names_of(&inputs, &opts(&["*.log", "build", "/a.log"], false)),
        ["a.log", "build/", "build/y"]
    );
    // An input named `.git` is kept too, even with gitignore handling on.
    touch(tmp.path(), &["repo/.git/HEAD"]);
    assert_eq!(
        names_of(&[tmp.path().join("repo/.git")], &opts(&[], true)),
        [".git/", ".git/HEAD"]
    );
}

#[test]
fn patterns_apply_to_each_input_separately() {
    let tmp = tempfile::tempdir().unwrap();
    touch(tmp.path(), &["one/x", "one/y", "two/x", "two/sub/x"]);
    let inputs = [tmp.path().join("one"), tmp.path().join("two")];
    assert_eq!(
        names_of(&inputs, &opts(&["/x"], false)),
        ["one/", "one/y", "two/", "two/sub/", "two/sub/x"]
    );
}

#[test]
fn invalid_pattern_is_an_error() {
    let (_t, p) = tree();
    match compress_mem(&[p], &opts(&["ok", "bad\\"], false)) {
        Err(Error::Io { path, source }) => {
            assert_eq!(path, Path::new("bad\\"));
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
        }
        other => panic!("expected an invalid-pattern error, got {other:?}"),
    }
}

#[cfg(windows)]
#[test]
fn patterns_ignore_case_on_windows() {
    let (_t, p) = tree();
    assert_eq!(
        names_of(&[p], &opts(&["*.LOG", "SRC/"], false)),
        TREE.iter()
            .filter(|n| !n.ends_with(".log") && !n.starts_with("p/src/"))
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    );
}

#[test]
fn excludes_combine_with_junk_filtering() {
    let tmp = tempfile::tempdir().unwrap();
    touch(tmp.path(), &["d/.DS_Store", "d/a.log", "d/b"]);
    let d = tmp.path().join("d");
    assert_eq!(
        names_of(std::slice::from_ref(&d), &opts(&["*.log"], false)),
        ["d/", "d/b"]
    );
    let keep_junk = CompressOptions {
        exclude_junk: false,
        ..opts(&["*.log"], false)
    };
    assert_eq!(names_of(&[d], &keep_junk), ["d/", "d/.DS_Store", "d/b"]);
}

/// A repository-like tree with nested `.gitignore` files.
fn repo() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let r = tmp.path().join("r");
    touch(
        &r,
        &[
            ".git/HEAD",
            ".git/objects/ab",
            "a.tmp",
            "important.tmp",
            "only_here",
            "other/a.tmp",
            "other/only_here",
            "other/secret",
            "sub/a.tmp",
            "sub/deeper/a.tmp",
            "sub/deeper/special.tmp",
            "sub/only_here",
            "sub/x/only_here",
            "sub/vendor/lib.rs",
            "target/debug/out",
            "vendored/.git",
            "vendored/code.rs",
        ],
    );
    std::fs::write(
        r.join(".gitignore"),
        "# build output\n*.tmp\n!important.tmp\n/target/\n",
    )
    .unwrap();
    std::fs::write(r.join("sub/.gitignore"), "!*.tmp\r\n/only_here\nvendor/\n").unwrap();
    std::fs::write(r.join("sub/deeper/.gitignore"), "special.tmp\n").unwrap();
    std::fs::write(r.join("other/.gitignore"), "secret\n").unwrap();
    (tmp, r)
}

#[test]
fn nested_gitignore_files_apply_to_their_subtree() {
    let (_t, r) = repo();
    assert_eq!(
        names_of(&[r], &opts(&[], true)),
        [
            "r/",
            "r/.gitignore",
            "r/important.tmp",
            "r/only_here",
            "r/other/",
            "r/other/.gitignore",
            "r/other/only_here",
            "r/sub/",
            "r/sub/.gitignore",
            "r/sub/a.tmp",
            "r/sub/deeper/",
            "r/sub/deeper/.gitignore",
            "r/sub/deeper/a.tmp",
            "r/sub/x/",
            "r/sub/x/only_here",
            "r/vendored/",
            "r/vendored/code.rs",
        ]
    );
}

#[test]
fn gitignore_is_off_by_default() {
    let (_t, r) = repo();
    let all = names_of(&[r], &opts(&[], false));
    for n in [
        "r/.git/HEAD",
        "r/a.tmp",
        "r/target/debug/out",
        "r/vendored/.git",
    ] {
        assert!(all.contains(&n.to_string()), "{n} missing from {all:?}");
    }
}

#[test]
fn exclude_wins_over_gitignore_negation() {
    let (_t, r) = repo();
    let n = names_of(&[r], &opts(&["important.tmp", "deeper/"], true));
    assert!(!n.contains(&"r/important.tmp".to_string()), "{n:?}");
    assert!(!n.iter().any(|x| x.starts_with("r/sub/deeper")), "{n:?}");
    assert!(n.contains(&"r/sub/a.tmp".to_string()), "{n:?}");
}

#[test]
fn gitignore_of_an_input_file_parent_is_not_read() {
    // Only .gitignore files inside an input count, not ones above it.
    let (_t, r) = repo();
    assert_eq!(
        names_of(&[r.join("sub/deeper")], &opts(&[], true)),
        ["deeper/", "deeper/.gitignore", "deeper/a.tmp"]
    );
    assert_eq!(names_of(&[r.join("a.tmp")], &opts(&[], true)), ["a.tmp"]);
}

#[cfg(unix)]
fn can_read_dir(p: &Path) -> bool {
    std::fs::read_dir(p).is_ok()
}

#[cfg(unix)]
#[test]
fn excluded_directories_are_not_walked() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("d");
    touch(&d, &["keep", "skip/locked/x", "ignored/locked/x"]);
    std::fs::write(d.join(".gitignore"), "ignored/\n").unwrap();
    let locked = [d.join("skip/locked"), d.join("ignored/locked")];
    for l in &locked {
        std::fs::set_permissions(l, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    let restore = || {
        for l in &locked {
            let _ = std::fs::set_permissions(l, std::fs::Permissions::from_mode(0o755));
        }
    };

    let result = std::panic::catch_unwind(|| {
        if !can_read_dir(&locked[0]) {
            // Sanity check: without the patterns the unreadable dir is an error.
            assert!(matches!(
                compress_mem(std::slice::from_ref(&d), &opts(&[], false)),
                Err(Error::Io { .. })
            ));
        }
        assert_eq!(
            names_of(std::slice::from_ref(&d), &opts(&["skip"], true)),
            ["d/", "d/.gitignore", "d/keep"]
        );
    });
    restore();
    result.unwrap();
}

#[cfg(unix)]
#[test]
fn unreadable_gitignore_is_an_error_only_when_gitignore_is_on() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("d");
    touch(&d, &["a"]);
    let gi = d.join(".gitignore");
    std::fs::write(&gi, "a\n").unwrap();
    std::fs::set_permissions(&gi, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read(&gi).is_ok() {
        return; // running as root
    }
    match compress_mem(std::slice::from_ref(&d), &opts(&[], true)) {
        Err(Error::Io { path, .. }) => assert_eq!(path, gi),
        other => panic!("expected an I/O error, got {other:?}"),
    }
    // Without --gitignore the file is just data, and unreadable data fails
    // at write time; with it excluded nothing reads it at all.
    assert_eq!(names_of(&[d], &opts(&[".gitignore"], false)), ["d/", "d/a"]);
}

#[cfg(unix)]
#[test]
fn dir_only_patterns_do_not_match_a_stored_symlink_to_a_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("d");
    touch(&d, &["real/x"]);
    std::os::unix::fs::symlink("real", d.join("link")).unwrap();
    assert_eq!(
        names_of(std::slice::from_ref(&d), &opts(&["link/", "real/"], false)),
        ["d/", "d/link"]
    );
    assert_eq!(
        names_of(&[d], &opts(&["link"], false)),
        ["d/", "d/real/", "d/real/x"]
    );
}

#[test]
fn filtering_is_deterministic() {
    let (_t, r) = repo();
    let o = opts(&["*.rs", "!code.rs", "other/"], true);
    let (a, _) = compress_ok(std::slice::from_ref(&r), &o);
    let (b, _) = compress_ok(
        std::slice::from_ref(&r),
        &CompressOptions {
            threads: 1,
            ..o.clone()
        },
    );
    let (c, _) = compress_ok(
        std::slice::from_ref(&r),
        &CompressOptions { threads: 3, ..o },
    );
    assert_eq!(a, b);
    assert_eq!(a, c);
    let n = names(&read_entries(&a));
    let mut sorted = n.clone();
    sorted.sort();
    assert_eq!(n, sorted);
    assert!(n.contains(&"r/vendored/code.rs".to_string()), "{n:?}");
    assert!(!n.contains(&"r/sub/vendor/lib.rs".to_string()), "{n:?}");
}
