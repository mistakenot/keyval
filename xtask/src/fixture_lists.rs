//! D-78's fixture lists (br-f97j). The deterministic crates cannot list `config/fixtures/`, so
//! the parser's acceptance test (`types`, CONFIG-RULES) and the `RawNode` construction test
//! (`raft_adapter`, RAFT-ADAPTER) name every fixture by hand. These tests fail when the
//! directory holds a fixture that either list lacks, so a new fixture can never go untested
//! (P10).
#![cfg(test)]

// check: FIXTURE-LISTS

use std::fs;
use std::path::Path;

use crate::test_support::workspace_root;

/// The sources whose `include_str!` lists must name every committed fixture.
const LISTS: [&str; 2] = [
    "crates/types/src/config/tests.rs",
    "crates/raft_adapter/src/test_support.rs",
];

/// The fixtures in `names` that `source` never includes as `fixtures/<name>`.
fn unlisted<'a>(names: &'a [String], source: &str) -> Vec<&'a str> {
    names
        .iter()
        .filter(|name| !source.contains(&format!("fixtures/{name}\"")))
        .map(String::as_str)
        .collect()
}

/// The `.toml` files in `<root>/config/fixtures/`, sorted.
fn fixture_files(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root.join("config/fixtures"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| {
            Path::new(name)
                .extension()
                .is_some_and(|extension| extension == "toml")
        })
        .collect();
    names.sort();
    names
}

#[test]
fn every_committed_fixture_is_in_both_d78_lists() {
    let root = workspace_root();
    let names = fixture_files(&root);
    assert!(
        names.len() >= 5,
        "fixture: the corpus holds its committed fixtures; found {names:?}"
    );
    for list in LISTS {
        let source = fs::read_to_string(root.join(list)).unwrap();
        assert_eq!(
            unlisted(&names, &source),
            Vec::<&str>::new(),
            "rule: every config/fixtures/ file is named in {list}, so the D-78 tests cover it \
             (P10); fixtures={names:?}"
        );
    }
}

#[test]
fn a_fixture_missing_from_a_list_is_reported() {
    let names = vec!["a.toml".to_owned(), "b.toml".to_owned()];
    let source = r#"include_str!("../../../config/fixtures/a.toml")"#;
    assert_eq!(
        unlisted(&names, source),
        vec!["b.toml"],
        "rule: a fixture the list lacks is reported, by file name"
    );
}
