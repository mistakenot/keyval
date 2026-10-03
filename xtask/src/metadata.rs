//! The workspace as cargo describes it (`cargo metadata --format-version 1`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use crate::process;

const METADATA_LIMIT: Duration = Duration::from_mins(5);

/// The parts of `cargo metadata` output that xtask uses.
#[derive(Debug, Deserialize)]
pub struct Metadata {
    /// Every package in the dependency graph (only workspace members with `--no-deps`).
    pub packages: Vec<Package>,
    /// Package IDs of the workspace members.
    pub workspace_members: Vec<String>,
    /// Absolute path of the workspace root.
    pub workspace_root: PathBuf,
}

/// One package.
#[derive(Debug, Deserialize)]
pub struct Package {
    /// Package name, e.g. `types`.
    pub name: String,
    /// Opaque package ID, as listed in `workspace_members`.
    pub id: String,
    /// Absolute path of its `Cargo.toml`.
    pub manifest_path: PathBuf,
    /// Its build targets.
    pub targets: Vec<Target>,
    /// Its direct dependencies, as declared.
    pub dependencies: Vec<Dependency>,
}

/// One build target of a package.
#[derive(Debug, Deserialize)]
pub struct Target {
    /// Target kinds, e.g. `["lib"]`, `["bin"]`, `["test"]`.
    pub kind: Vec<String>,
    /// Absolute path of the target's root source file.
    pub src_path: PathBuf,
}

/// One declared dependency.
#[derive(Debug, Deserialize)]
pub struct Dependency {
    /// The depended-on package's name. Every kind (normal, dev, build) is listed.
    pub name: String,
    /// The local path, for path dependencies; `None` for registry and git ones.
    pub path: Option<PathBuf>,
}

impl Metadata {
    /// The workspace member packages, in `workspace_members` order.
    pub fn members(&self) -> Vec<&Package> {
        self.workspace_members
            .iter()
            .filter_map(|id| self.packages.iter().find(|p| &p.id == id))
            .collect()
    }
}

impl Package {
    /// The directory holding the package's `Cargo.toml`.
    pub fn dir(&self) -> &Path {
        self.manifest_path.parent().unwrap_or(&self.manifest_path)
    }

    /// The root source file of the package's library target, if it has one.
    pub fn lib_src(&self) -> Option<&Path> {
        self.targets
            .iter()
            .find(|t| t.kind.iter().any(|k| k == "lib"))
            .map(|t| t.src_path.as_path())
    }
}

/// Runs `cargo metadata --format-version 1 --no-deps --locked` in `root`.
///
/// Always plain local cargo: this reads manifests and builds nothing, so there is
/// nothing for rch to offload.
pub fn load(root: &Path) -> Result<Metadata, String> {
    let mut cmd = Command::new("cargo");
    cmd.args(["metadata", "--format-version", "1", "--no-deps", "--locked"])
        .current_dir(root);
    let captured =
        process::run(&mut cmd, METADATA_LIMIT).map_err(|err| format!("cargo metadata: {err}"))?;
    if !captured.status.success() {
        return Err(format!(
            "cargo metadata exited with {}: {}",
            captured.status,
            captured.stderr.trim()
        ));
    }
    parse(&captured.stdout)
}

/// Parses `cargo metadata --format-version 1` JSON.
pub fn parse(json: &str) -> Result<Metadata, String> {
    serde_json::from_str(json).map_err(|err| format!("unreadable cargo metadata: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_workspace_lists_its_members() {
        let root = crate::test_support::workspace_root();
        let metadata = load(&root).unwrap();
        let names: Vec<&str> = metadata.members().iter().map(|p| p.name.as_str()).collect();
        for expected in ["xtask", "types"] {
            assert!(
                names.contains(&expected),
                "expected member {expected}; members={names:?}"
            );
        }
        let types = metadata
            .members()
            .into_iter()
            .find(|p| p.name == "types")
            .unwrap();
        assert!(
            types
                .lib_src()
                .is_some_and(|p| p.ends_with("crates/types/src/lib.rs")),
            "observed={types:?}"
        );
    }

    #[test]
    fn parse_reads_members_and_targets() {
        let json = r#"{
            "packages": [{
                "name": "xtask", "id": "path+file:///w/xtask#0.1.0",
                "manifest_path": "/w/xtask/Cargo.toml",
                "targets": [{"kind": ["bin"], "src_path": "/w/xtask/src/main.rs"}],
                "dependencies": []
            }],
            "workspace_members": ["path+file:///w/xtask#0.1.0"],
            "workspace_root": "/w"
        }"#;
        let metadata = parse(json).unwrap();
        let members = metadata.members();
        assert_eq!(members.len(), 1, "observed={metadata:?}");
        assert_eq!(members[0].dir(), Path::new("/w/xtask"));
        assert_eq!(
            members[0].lib_src(),
            None,
            "a bin-only package has no lib target"
        );
    }
}
