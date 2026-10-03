//! The exact-candidate directory (§11.10 step 3): a plain directory holding exactly one
//! git tree. It is not a branch, not a linked worktree, and has no `.git`.
//!
//! [`materialise`] refreshes the directory in place. Unchanged files keep their mtime, so
//! cargo's fingerprints stay warm; changed files are rewritten; files the tree lacks are
//! removed. [`verify`] then checks the directory against the tree independently: the same
//! path set (any leftover or missing file refuses the run, because cargo auto-discovers
//! stray `tests/*.rs`), the same bytes (by git blob ID), and the same modes.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use crate::git::{Git, TreeEntry};

/// What [`materialise`] changed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Sync {
    /// Files written because they were new or different.
    pub written: usize,
    /// Files left alone because they already matched.
    pub unchanged: usize,
    /// Files removed because the tree lacks them.
    pub removed: usize,
}

/// Refreshes `dir` in place so that it holds exactly `tree`.
pub fn materialise(git: &Git, tree: &str, dir: &Path) -> Result<Sync, String> {
    // This removes files the tree lacks, so it must never point at a working tree.
    if dir.join(".git").exists() || dir == git.root() {
        return Err(format!(
            "refusing to materialise into {}: it is a git working tree",
            dir.display()
        ));
    }
    let entries = git.ls_tree(tree)?;
    let oids: Vec<String> = entries.iter().map(|e| e.oid.clone()).collect();
    let blobs = git.read_blobs(&oids)?;
    fs::create_dir_all(dir).map_err(|err| io_error(dir, &err))?;
    let wanted: BTreeSet<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    let mut sync = Sync::default();
    for path in existing_files(dir)? {
        if !wanted.contains(path.as_str()) {
            let full = dir.join(&path);
            fs::remove_file(&full).map_err(|err| io_error(&full, &err))?;
            sync.removed += 1;
        }
    }
    remove_empty_dirs(dir)?;
    for (entry, content) in entries.iter().zip(&blobs) {
        if write_entry(dir, entry, content)? {
            sync.written += 1;
        } else {
            sync.unchanged += 1;
        }
    }
    Ok(sync)
}

/// Writes one tree entry unless the file already matches; returns whether it wrote.
fn write_entry(dir: &Path, entry: &TreeEntry, content: &[u8]) -> Result<bool, String> {
    let full = dir.join(&entry.path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).map_err(|err| io_error(parent, &err))?;
    }
    if entry.mode == "120000" {
        let target = PathBuf::from(String::from_utf8_lossy(content).into_owned());
        if fs::read_link(&full).ok().as_ref() == Some(&target) {
            return Ok(false);
        }
        let _ = fs::remove_file(&full);
        symlink(&target, &full).map_err(|err| io_error(&full, &err))?;
        return Ok(true);
    }
    let mode = if entry.mode == "100755" { 0o755 } else { 0o644 };
    let is_symlink = fs::symlink_metadata(&full).is_ok_and(|m| m.file_type().is_symlink());
    let same_bytes = !is_symlink && fs::read(&full).ok().as_deref() == Some(content);
    if !same_bytes {
        if is_symlink {
            fs::remove_file(&full).map_err(|err| io_error(&full, &err))?;
        }
        fs::write(&full, content).map_err(|err| io_error(&full, &err))?;
    }
    let current = fs::metadata(&full).map_err(|err| io_error(&full, &err))?;
    if current.permissions().mode() & 0o777 != mode {
        fs::set_permissions(&full, fs::Permissions::from_mode(mode))
            .map_err(|err| io_error(&full, &err))?;
    }
    Ok(!same_bytes)
}

/// Why a candidate directory does not hold its tree exactly.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Mismatch {
    /// Files present but not in the tree.
    pub leftover: Vec<String>,
    /// Files in the tree but absent.
    pub missing: Vec<String>,
    /// Files whose bytes, mode or link target differ from the tree.
    pub different: Vec<String>,
}

impl Mismatch {
    const fn is_empty(&self) -> bool {
        self.leftover.is_empty() && self.missing.is_empty() && self.different.is_empty()
    }
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "candidate does not match its tree: leftover {:?}, missing {:?}, different {:?}",
            self.leftover, self.missing, self.different
        )
    }
}

/// Checks that `dir` holds exactly `tree`: same paths, same bytes, same modes.
/// Returns the number of files verified.
pub fn verify(git: &Git, tree: &str, dir: &Path) -> Result<usize, String> {
    let entries = git.ls_tree(tree)?;
    let expected: BTreeMap<&str, &TreeEntry> =
        entries.iter().map(|e| (e.path.as_str(), e)).collect();
    let present: BTreeSet<String> = existing_files(dir)?.into_iter().collect();
    let mut mismatch = Mismatch {
        leftover: present
            .iter()
            .filter(|p| !expected.contains_key(p.as_str()))
            .cloned()
            .collect(),
        missing: expected
            .keys()
            .filter(|p| !present.contains(**p))
            .map(|p| (*p).to_owned())
            .collect(),
        different: Vec::new(),
    };
    let mut regular: Vec<(&TreeEntry, PathBuf)> = Vec::new();
    for entry in entries.iter().filter(|e| present.contains(&e.path)) {
        let full = dir.join(&entry.path);
        let meta = fs::symlink_metadata(&full).map_err(|err| io_error(&full, &err))?;
        if entry.mode == "120000" {
            let target_ok = meta.file_type().is_symlink()
                && git
                    .read_blobs(std::slice::from_ref(&entry.oid))
                    .ok()
                    .and_then(|blobs| blobs.into_iter().next())
                    .map(|blob| PathBuf::from(String::from_utf8_lossy(&blob).into_owned()))
                    == fs::read_link(&full).ok();
            if !target_ok {
                mismatch.different.push(entry.path.clone());
            }
            continue;
        }
        let executable = meta.permissions().mode() & 0o111 != 0;
        if meta.file_type().is_symlink() || executable != (entry.mode == "100755") {
            mismatch.different.push(entry.path.clone());
            continue;
        }
        regular.push((entry, full));
    }
    let paths: Vec<PathBuf> = regular.iter().map(|(_, full)| full.clone()).collect();
    let hashes = git.hash_files(&paths)?;
    for ((entry, _), hash) in regular.iter().zip(hashes) {
        if hash != entry.oid {
            mismatch.different.push(entry.path.clone());
        }
    }
    if mismatch.is_empty() {
        Ok(entries.len())
    } else {
        mismatch.different.sort();
        Err(mismatch.to_string())
    }
}

/// Every file and symlink under `dir`, as `/`-separated relative paths, sorted.
fn existing_files(dir: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    if dir.exists() {
        walk(dir, dir, &mut out)?;
    }
    out.sort();
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|err| io_error(dir, &err))? {
        let entry = entry.map_err(|err| io_error(dir, &err))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|err| io_error(&path, &err))?;
        if kind.is_dir() {
            walk(root, &path, out)?;
        } else {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            out.push(relative.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

fn remove_empty_dirs(dir: &Path) -> Result<bool, String> {
    let mut empty = true;
    for entry in fs::read_dir(dir).map_err(|err| io_error(dir, &err))? {
        let entry = entry.map_err(|err| io_error(dir, &err))?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|err| io_error(&path, &err))?;
        if kind.is_dir() && remove_empty_dirs(&path)? {
            fs::remove_dir(&path).map_err(|err| io_error(&path, &err))?;
        } else {
            empty = false;
        }
    }
    Ok(empty)
}

fn io_error(path: &Path, err: &io::Error) -> String {
    format!("{}: {err}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture;
    use crate::test_support::scratch_dir;
    use std::time::SystemTime;

    fn mtime(path: &Path) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    #[test]
    fn a_fresh_candidate_holds_exactly_the_tree() {
        let git = fixture::repo(
            "cand-fresh",
            &[
                ("Cargo.toml", "[workspace]\n"),
                ("src/lib.rs", "//! x\n"),
                ("dev", "#!/bin/sh\n"),
            ],
        );
        std::fs::set_permissions(git.root().join("dev"), fs::Permissions::from_mode(0o755))
            .unwrap();
        git.stage(&["dev".to_owned()]).unwrap();
        let tree = git.write_tree().unwrap();
        let dir = scratch_dir("cand-fresh-dir").join("candidate");
        let sync = materialise(&git, &tree, &dir).unwrap();
        assert_eq!(
            sync,
            Sync {
                written: 3,
                unchanged: 0,
                removed: 0
            }
        );
        assert_eq!(verify(&git, &tree, &dir), Ok(3));
        let mode = fs::metadata(dir.join("dev")).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o111,
            0o111,
            "rule: executable bits follow the tree mode"
        );
        assert!(
            !dir.join(".git").exists(),
            "rule: a candidate is never a repository"
        );
    }

    #[test]
    fn a_refresh_keeps_unchanged_files_and_removes_leftovers() {
        let git = fixture::repo(
            "cand-refresh",
            &[("a.rs", "a\n"), ("tests/old.rs", "old\n")],
        );
        let first = git.write_tree().unwrap();
        let dir = scratch_dir("cand-refresh-dir").join("candidate");
        materialise(&git, &first, &dir).unwrap();
        let a_before = mtime(&dir.join("a.rs"));

        fs::remove_file(git.root().join("tests/old.rs")).unwrap();
        fixture::write(git.root(), "b.rs", "b\n");
        git.run(&["add", "--all"]).unwrap();
        let second = git.write_tree().unwrap();
        let sync = materialise(&git, &second, &dir).unwrap();
        assert_eq!(
            sync,
            Sync {
                written: 1,
                unchanged: 1,
                removed: 1
            }
        );
        assert_eq!(
            mtime(&dir.join("a.rs")),
            a_before,
            "rule: unchanged files keep their mtime (warm cargo cache)"
        );
        assert!(
            !dir.join("tests").exists(),
            "rule: emptied directories are removed"
        );
        assert_eq!(verify(&git, &second, &dir), Ok(2));
    }

    #[test]
    fn verify_refuses_leftover_missing_and_altered_files() {
        let git = fixture::repo("cand-verify", &[("a.rs", "a\n"), ("b.rs", "b\n")]);
        let tree = git.write_tree().unwrap();
        let dir = scratch_dir("cand-verify-dir").join("candidate");
        materialise(&git, &tree, &dir).unwrap();
        fixture::write(&dir, "tests/stray.rs", "#[test] fn stray() {}\n");
        fs::remove_file(dir.join("b.rs")).unwrap();
        fixture::write(&dir, "a.rs", "tampered\n");
        let observed = verify(&git, &tree, &dir).unwrap_err();
        let expected = Mismatch {
            leftover: vec!["tests/stray.rs".to_owned()],
            missing: vec!["b.rs".to_owned()],
            different: vec!["a.rs".to_owned()],
        }
        .to_string();
        assert_eq!(
            observed, expected,
            "rule: leftover (cargo would run tests/stray.rs), missing and altered files all \
             refuse the run (§11.10 step 3)"
        );
    }

    #[test]
    fn materialise_refuses_a_working_tree() {
        let git = fixture::repo("cand-guard", &[("a.rs", "a\n")]);
        let tree = git.write_tree().unwrap();
        let refused = materialise(&git, &tree, git.root());
        assert!(
            refused.is_err_and(|e| e.contains("git working tree")),
            "rule: the candidate sync never deletes files from a checkout"
        );
    }

    #[test]
    fn verify_refuses_a_changed_mode() {
        let git = fixture::repo("cand-mode", &[("run.sh", "#!/bin/sh\n")]);
        let tree = git.write_tree().unwrap();
        let dir = scratch_dir("cand-mode-dir").join("candidate");
        materialise(&git, &tree, &dir).unwrap();
        fs::set_permissions(dir.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify(&git, &tree, &dir).is_err_and(|e| e.contains("run.sh")));
    }
}
