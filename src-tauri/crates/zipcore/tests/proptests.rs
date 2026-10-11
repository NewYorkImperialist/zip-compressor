//! Property test: random small trees round-trip exactly through the zip crate.

mod common;

use std::collections::HashSet;
use std::fs;

use common::*;
use proptest::prelude::*;
use zipcore::Method;

fn name_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 _\\-éüßñ日本語한🎉]{1,10}".prop_filter_map("blank or space-padded", |s| {
        let t = s.trim();
        (t == s && !t.is_empty()).then(|| t.to_string())
    })
}

fn content_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        (0usize..65536, any::<u64>()).prop_map(|(n, seed)| random_bytes(n, seed)),
        (0usize..65536, any::<u8>()).prop_map(|(n, b)| vec![b; n]),
        (0usize..65536).prop_map(compressible_bytes),
        Just(Vec::new()),
    ]
}

#[derive(Debug, Clone)]
enum Item {
    File(Vec<String>, Vec<u8>),
    Dir(Vec<String>),
}

fn item_strategy() -> impl Strategy<Value = Item> {
    let path = || prop::collection::vec(name_strategy(), 1..5);
    prop_oneof![
        4 => (path(), content_strategy()).prop_map(|(p, c)| Item::File(p, c)),
        1 => path().prop_map(Item::Dir),
    ]
}

fn method_strategy() -> impl Strategy<Value = Method> {
    prop_oneof![
        Just(Method::Store),
        (0u8..=9).prop_map(|level| Method::Deflate { level })
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    #[test]
    fn random_trees_round_trip(items in prop::collection::vec(item_strategy(), 0..12), method in method_strategy()) {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join("tree");
        fs::create_dir(&root).unwrap();
        // Keys are lowercased so the tree is valid on case-insensitive filesystems.
        let mut files: HashSet<String> = HashSet::new();
        let mut dirs: HashSet<String> = HashSet::new();
        for item in items {
            let (comps, content) = match item {
                Item::File(p, c) => (p, Some(c)),
                Item::Dir(p) => (p, None),
            };
            let key = |n: usize| comps[..n].join("/").to_lowercase();
            let prefixes_ok = (1..comps.len()).all(|n| !files.contains(&key(n)));
            let full = key(comps.len());
            let ok = prefixes_ok && !files.contains(&full) && (content.is_none() || !dirs.contains(&full));
            if !ok {
                continue;
            }
            for n in 1..comps.len() {
                dirs.insert(key(n));
            }
            let path = comps.iter().fold(root.clone(), |p, c| p.join(c));
            match content {
                Some(c) => {
                    files.insert(full);
                    write_file(&path, &c);
                }
                None => {
                    dirs.insert(full);
                    fs::create_dir_all(&path).unwrap();
                }
            }
        }

        let (zip, summary) = compress_ok(std::slice::from_ref(&root), &opts(method));
        let entries = read_entries(&zip);
        assert_safe_names(&entries);
        assert_nodes_eq(&archive_nodes(&entries), &expected_entries(&root));
        prop_assert_eq!(summary.files, files.len() as u64);
        prop_assert_eq!(summary.bytes_out, zip.len() as u64);
    }
}
