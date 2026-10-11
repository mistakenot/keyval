//! The `failpoints` feature is absent from production builds (PLAN.md T-2, T-6, §10.17;
//! W24.1).
//!
//! Three facts, each checked here rather than assumed:
//! - no manifest but this crate's own mentions the feature, so no production crate can
//!   enable it, and this crate does not enable it by default;
//! - a release build of this crate with default features contains no failpoint code (the
//!   announcement string is the marker);
//! - the same build with the feature does contain it, so the previous fact is not vacuous.
//!
//! The builds run in their own target directory: the outer `cargo test` keeps its own
//! locked while this runs.
#![cfg(test)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// The string `failpoints::hit` prints: present in a build exactly when the code is.
const MARKER: &[u8] = b"keyval-failpoint-reached";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .to_path_buf()
}

/// Manifests of every workspace member and the root.
fn manifests() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut found = vec![root.join("Cargo.toml")];
    for dir in ["crates", "conformance", "xtask", "fuzz"] {
        let base = root.join(dir);
        if base.join("Cargo.toml").is_file() {
            found.push(base.join("Cargo.toml"));
        }
        if let Ok(entries) = std::fs::read_dir(&base) {
            for entry in entries.flatten() {
                let manifest = entry.path().join("Cargo.toml");
                if manifest.is_file() {
                    found.push(manifest);
                }
            }
        }
    }
    found
}

// check: FAILPOINTS-ABSENT
#[test]
fn only_this_crate_names_the_feature_and_it_is_not_a_default() {
    let own = workspace_root().join("crates/fs_storage/Cargo.toml");
    let mut named = Vec::new();
    for manifest in manifests() {
        let text = std::fs::read_to_string(&manifest).unwrap();
        // Comments may talk about the feature; declarations and requests may not.
        let code = text
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .any(|line| line.contains("failpoints"));
        if code {
            named.push(manifest);
        }
    }
    assert_eq!(
        named,
        vec![own.clone()],
        "rule: no crate but fs_storage declares or enables `failpoints` (T-6)"
    );
    let text = std::fs::read_to_string(&own).unwrap();
    assert!(
        !text
            .lines()
            .any(|line| line.trim_start().starts_with("default")),
        "rule: the feature is not among fs_storage's default features (T-6)"
    );
}

/// Builds `fs_storage` for release in `target`, with `features`, and returns its rlib.
fn release_rlib(target: &Path, features: &[&str]) -> Vec<u8> {
    let cargo = std::env::var_os("CARGO").unwrap();
    let mut command = Command::new(cargo);
    command
        .args(["build", "--release", "--frozen", "-p", "fs_storage"])
        .current_dir(workspace_root())
        .env("CARGO_TARGET_DIR", target)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS");
    if !features.is_empty() {
        command.args(["--features", &features.join(",")]);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "fixture: the release build succeeds; features={features:?} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read(target.join("release/libfs_storage.rlib")).unwrap()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[test]
fn a_release_build_has_no_failpoint_code_and_the_feature_build_has() {
    let exe = std::env::current_exe().unwrap();
    // <target>/<profile>/deps/<test binary> -> <target>
    let base = exe
        .ancestors()
        .nth(3)
        .unwrap()
        .join(format!("fs-storage-release-{}", std::process::id()));
    let production = release_rlib(&base.join("production"), &[]);
    assert!(
        !contains(&production, MARKER),
        "rule: a default-features release build of fs_storage has no failpoint code (T-6)"
    );
    let with_feature = release_rlib(&base.join("failpoints"), &["failpoints"]);
    assert!(
        contains(&with_feature, MARKER),
        "rule: the marker is in the build that has the feature, so its absence above means \
         something (T-6)"
    );
}
