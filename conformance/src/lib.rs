//! The API conformance harness (PLAN.md §10.8; W31.1). It is the machinery the conformance
//! tests (W31.2 onwards) stand on, and it depends on no first-party crate: the suite
//! speaks HTTP to base URLs and reads `api/openapi.yaml`, and depends on no production crate
//! (T-12). PLAN.md §10.8 lets its tests use the reference model and the K-10 witness from the
//! oracle crates, as dev-dependencies of the test modules only.
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
mod suite;

#[cfg(test)]
mod independence {
    // check: CONF-HARNESS
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Every production crate name (Appendix A) the suite must never name (T-12).
    const PRODUCTION: [&str; 9] = [
        "types",
        "statemachine",
        "node",
        "journal",
        "raft_adapter",
        "fs_storage",
        "server",
        "sim",
        "cli",
    ];

    /// The oracle crates PLAN.md §10.8 lets the suite's tests use: the reference model and the
    /// K-10 witness. Test modules only, never the harness library.
    const ORACLES: [&str; 2] = ["model", "checker"];

    fn root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
    }

    /// The non-comment lines of manifest table `[table]`.
    fn table<'a>(manifest: &'a str, table: &str) -> Vec<&'a str> {
        manifest
            .lines()
            .map(str::trim)
            .skip_while(|l| *l != table)
            .skip(1)
            .take_while(|l| !l.starts_with('['))
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect()
    }

    fn name_of(line: &str) -> &str {
        line.split(['=', ' ']).next().unwrap_or_default()
    }

    #[test]
    fn manifest_names_no_production_or_path_dependency() {
        let manifest = fs::read_to_string(root().join("Cargo.toml")).unwrap();
        let deps = table(&manifest, "[dependencies]");
        assert!(
            !deps.is_empty(),
            "rule: the dependency table is found; manifest={manifest}"
        );
        for line in deps {
            let name = name_of(line);
            assert!(
                !PRODUCTION.contains(&name) && !ORACLES.contains(&name) && !line.contains("path"),
                "rule: T-12 — the harness library depends on no first-party crate; \
                 offending line={line:?}"
            );
        }
        assert!(
            !manifest.contains("[build-dependencies]"),
            "rule: build dependencies are held to the same rule; manifest={manifest}"
        );
    }

    #[test]
    fn dev_dependencies_are_oracles_only() {
        let manifest = fs::read_to_string(root().join("Cargo.toml")).unwrap();
        let dev = table(&manifest, "[dev-dependencies]");
        let names: Vec<&str> = dev.iter().map(|l| name_of(l)).collect();
        assert_eq!(
            names, ORACLES,
            "rule: PLAN.md §10.8 — the tests may use the reference model and the K-10 witness \
             and nothing else; dev-dependencies={dev:?}"
        );
    }

    #[test]
    fn sources_import_no_production_crate_and_only_the_suite_imports_oracles() {
        let mut sources = Vec::new();
        collect(&root().join("src"), &mut sources);
        assert!(
            sources.len() >= 6,
            "rule: the scan sees the sources; found={sources:?}"
        );
        for path in sources {
            // This test's own needles are the only legitimate occurrences.
            if path.ends_with("lib.rs") {
                continue;
            }
            let in_suite = path.strip_prefix(root().join("src/suite")).is_ok();
            let text = fs::read_to_string(&path).unwrap();
            let banned: Vec<&str> = if in_suite {
                PRODUCTION.to_vec()
            } else {
                PRODUCTION.iter().chain(ORACLES.iter()).copied().collect()
            };
            for name in banned {
                for needle in [
                    format!("use {name}::"),
                    format!("{name}::{{"),
                    format!("extern crate {name}"),
                ] {
                    assert!(
                        !text.contains(&needle),
                        "rule: T-12 — {} imports crate {name}; needle={needle:?}",
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
