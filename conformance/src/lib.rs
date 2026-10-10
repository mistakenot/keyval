//! The API conformance harness (PLAN.md §10.8; W31.1). It is the machinery the conformance
//! tests (W31.2 onwards) stand on, and it depends on no first-party crate: the suite
//! speaks HTTP to base URLs and reads `api/openapi.yaml`, nothing else (T-12).
//!
//! - [`spec`]: the operation IDs and error codes of the `OpenAPI` document.
//! - [`target`]: the five cluster base URLs plus the model server and single node, and the
//!   per-operation rule that decides which targets a test runs against.
//! - [`tag`]: a test's class and the operations and error codes it exercises, judged
//!   against the spec. The registry gate reads the matching `// api-test: <name>` markers.
//! - [`http`]: a strict, bounded HTTP/1.1 client over `std::net`.
//! - [`poll`]: the bounded stale-read poll (C-7): wait until
//!   `served_by.applied_index >= index`, and fail on timeout.

pub mod http;
pub mod poll;
pub mod spec;
pub mod tag;
pub mod target;

#[cfg(test)]
mod independence {
    // check: CONF-HARNESS
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Every first-party crate name (Appendix A) the suite must never name (T-12).
    const FIRST_PARTY: [&str; 11] = [
        "types",
        "statemachine",
        "node",
        "journal",
        "raft_adapter",
        "fs_storage",
        "server",
        "sim",
        "model",
        "checker",
        "cli",
    ];

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
    }

    #[test]
    fn manifest_names_no_first_party_or_path_dependency() {
        let manifest = fs::read_to_string(root().join("Cargo.toml")).unwrap();
        let deps: Vec<&str> = manifest
            .lines()
            .map(str::trim)
            .skip_while(|l| *l != "[dependencies]")
            .skip(1)
            .take_while(|l| !l.starts_with('['))
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert!(
            !deps.is_empty(),
            "rule: the dependency table is found; manifest={manifest}"
        );
        for line in deps {
            let name = line.split(['=', ' ']).next().unwrap_or_default();
            assert!(
                !FIRST_PARTY.contains(&name) && !line.contains("path"),
                "rule: T-12 — conformance depends on no first-party crate; offending line={line:?}"
            );
        }
        assert!(
            !manifest.contains("[dev-dependencies]") && !manifest.contains("[build-dependencies]"),
            "rule: dev and build dependencies are held to the same rule; manifest={manifest}"
        );
    }

    #[test]
    fn sources_import_no_first_party_crate() {
        let mut sources = Vec::new();
        collect(&root().join("src"), &mut sources);
        assert!(
            sources.len() >= 6,
            "rule: the scan sees the sources; found={sources:?}"
        );
        for path in sources {
            let text = fs::read_to_string(&path).unwrap();
            for name in FIRST_PARTY {
                for needle in [
                    format!("use {name}::"),
                    format!("{name}::{{"),
                    format!("extern crate {name}"),
                ] {
                    // This test's own needles are the only legitimate occurrences.
                    if path.ends_with("lib.rs") {
                        continue;
                    }
                    assert!(
                        !text.contains(&needle),
                        "rule: T-12 — {} imports first-party crate {name}; needle={needle:?}",
                        path.display()
                    );
                }
            }
        }
    }

    fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
}
