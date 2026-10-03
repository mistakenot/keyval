//! `./dev prune [--dry-run]`: the `.local/` retention rule (PLAN.md §10.10, Q9, D-74), in
//! the subset that covers acceptance trees (br-accept-tree-retention-gaj9). W46 extends it
//! to gate runs.
//!
//! **Accept trees** (`.local/accept/<tree-id>/`, §11.3 step 7). The newest
//! [`KEEP_ACCEPT_TREES`] are kept, and so is every tree with a failing run: one with a
//! receipt that did not pass, or one with no receipt at all (an interrupted run). A tree
//! whose accept lock another process holds is in use and kept too. Every other tree is
//! removed together with its per-checkout target directory
//! ([`cargo::checkout_target_dir`]). The zero-byte lock files stay: removing one that a
//! waiting `./dev accept` already holds open would let a second accept lock a fresh file
//! and share the directory.
//!
//! **Orphaned target directories.** A `keyval-checkout-<hash>` directory under the target
//! base goes when the checkout it belongs to no longer exists. That checkout is known only
//! from the marker the `./dev` launcher writes into it ([`cargo::CHECKOUT_MARKER`]), and
//! only when the marker's path hashes to the directory's own name. A directory without
//! such a marker is reported, never removed.
//!
//! Every removal, or with `--dry-run` every removal it would make, is logged in a
//! `keyval.dev.prune/v1` receipt under `artifacts/receipts/`. `./dev prune` is the only
//! sanctioned deleter of generated `.local/` artifacts (Q9), and it deletes nothing else.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, TryLockError};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;

use crate::accept;
use crate::cargo;
use crate::clock::Utc;
use crate::output::Report;

/// Schema identifier of the `--json` report and of the prune receipt.
pub const SCHEMA: &str = "keyval.dev.prune/v1";

/// How many of the newest accept trees are always kept (Q9).
pub const KEEP_ACCEPT_TREES: usize = 5;

/// What happened to a directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Left in place.
    Kept,
    /// Deleted.
    Removed,
    /// Would be deleted without `--dry-run`.
    WouldRemove,
}

/// Why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// One of the newest [`KEEP_ACCEPT_TREES`] accept trees.
    Newest,
    /// A run on this tree failed; its artifacts are evidence.
    Failing,
    /// No receipt names this tree, so its run was interrupted; kept as evidence.
    NoReceipt,
    /// Another process holds the tree's accept lock.
    InUse,
    /// Older than the newest trees, and every run on it passed.
    Superseded,
    /// The checkout this target directory belongs to still exists.
    CheckoutExists,
    /// The checkout this target directory belongs to no longer exists.
    CheckoutGone,
    /// No marker names the checkout, so the directory cannot be attributed.
    Unattributed,
    /// The marker's path does not hash to the directory's name.
    MarkerMismatch,
}

/// One accept tree and what happened to it.
#[derive(Debug, Serialize)]
pub struct AcceptTree {
    /// The tree ID (the directory's name).
    pub tree: String,
    /// What happened.
    pub action: Action,
    /// Why.
    pub reason: Reason,
    /// The tree's per-checkout target directory.
    pub target_dir: Option<String>,
    /// Bytes freed (or that would be), counting the target directory; removals only.
    pub bytes: Option<u64>,
}

/// One per-checkout target directory that belongs to no accept tree.
#[derive(Debug, Serialize)]
pub struct TargetDir {
    /// The directory.
    pub dir: String,
    /// The checkout its marker names, if it has a valid marker.
    pub checkout: Option<String>,
    /// What happened.
    pub action: Action,
    /// Why.
    pub reason: Reason,
    /// Bytes freed (or that would be); removals only.
    pub bytes: Option<u64>,
}

/// The `./dev prune` report and receipt (`keyval.dev.prune/v1`).
#[derive(Debug, Serialize)]
pub struct PruneReport {
    /// Schema identifier.
    pub schema: &'static str,
    /// True when every intended removal succeeded.
    pub ok: bool,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// How many of the newest accept trees are kept.
    pub keep_accept_trees: usize,
    /// Every accept tree, newest first.
    pub accept_trees: Vec<AcceptTree>,
    /// Every other per-checkout target directory under the base.
    pub target_dirs: Vec<TargetDir>,
    /// Bytes freed (or that would be).
    pub freed_bytes: u64,
    /// Removals that failed.
    pub errors: Vec<String>,
    /// Start time (UTC).
    pub started_at: String,
    /// End time (UTC).
    pub finished_at: String,
    /// Where this receipt was written.
    pub receipt_path: Option<String>,
}

/// Where prune looks.
#[derive(Clone, Debug)]
pub struct Layout {
    /// The workspace root (`.local/accept/` lives under it).
    pub root: PathBuf,
    /// The shared base of the per-checkout target directories.
    pub base: PathBuf,
    /// The artifacts directory (`receipts/` lives under it).
    pub artifacts: PathBuf,
}

/// Applies the retention rule (or, with `dry_run`, reports what it would remove) and
/// writes the receipt.
pub fn run(layout: &Layout, dry_run: bool) -> PruneReport {
    let mut report = PruneReport {
        schema: SCHEMA,
        ok: false,
        dry_run,
        keep_accept_trees: KEEP_ACCEPT_TREES,
        accept_trees: Vec::new(),
        target_dirs: Vec::new(),
        freed_bytes: 0,
        errors: Vec::new(),
        started_at: Utc::now().rfc3339(),
        finished_at: String::new(),
        receipt_path: None,
    };
    let accounted = prune_accept_trees(layout, dry_run, &mut report);
    prune_target_dirs(layout, dry_run, &accounted, &mut report);
    report.ok = report.errors.is_empty();
    report.finished_at = Utc::now().rfc3339();
    write_receipt(&mut report, &layout.artifacts);
    report
}

/// What the accept receipts say about one tree.
#[derive(Default)]
struct Runs {
    /// The newest receipt's modification time.
    newest: Option<SystemTime>,
    /// Whether any run on the tree did not pass.
    failed: bool,
}

/// Applies the accept-tree rule; returns every target directory that belongs to an accept
/// tree, kept or removed, so the orphan scan leaves them to this rule.
fn prune_accept_trees(
    layout: &Layout,
    dry_run: bool,
    report: &mut PruneReport,
) -> BTreeSet<PathBuf> {
    let accept_root = layout.root.join(accept::ACCEPT_ROOT);
    let runs = accept_runs(&layout.artifacts);
    let mut trees: Vec<(SystemTime, String, PathBuf)> = Vec::new();
    for (name, dir) in tree_dirs(&accept_root) {
        let recency = runs
            .get(&name)
            .and_then(|r| r.newest)
            .or_else(|| fs::metadata(&dir).and_then(|m| m.modified()).ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        trees.push((recency, name, dir));
    }
    // Newest first; equal times fall back to the tree ID so the order is stable.
    trees.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut accounted = BTreeSet::new();
    for (rank, (_, tree, dir)) in trees.into_iter().enumerate() {
        let target = cargo::checkout_target_dir(&layout.base, &dir).ok();
        if let Some(target) = &target {
            accounted.insert(target.clone());
        }
        let reason = match runs.get(&tree) {
            _ if rank < KEEP_ACCEPT_TREES => Reason::Newest,
            None => Reason::NoReceipt,
            Some(r) if r.failed => Reason::Failing,
            Some(_) => Reason::Superseded,
        };
        let mut entry = AcceptTree {
            tree,
            action: Action::Kept,
            reason,
            target_dir: target.as_ref().map(|t| t.display().to_string()),
            bytes: None,
        };
        if reason == Reason::Superseded {
            remove_accept_tree(&dir, target.as_deref(), dry_run, &mut entry, report);
        }
        report.accept_trees.push(entry);
    }
    accounted
}

/// Removes one superseded accept tree and its target directory under the tree's lock.
fn remove_accept_tree(
    dir: &Path,
    target: Option<&Path>,
    dry_run: bool,
    entry: &mut AcceptTree,
    report: &mut PruneReport,
) {
    let _lock = match try_lock(&dir.with_extension("lock")) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            entry.reason = Reason::InUse;
            return;
        }
        Err(why) => {
            report.errors.push(why);
            return;
        }
    };
    let target = target.filter(|t| t.is_dir());
    let bytes = dir_size(dir) + target.map_or(0, dir_size);
    entry.bytes = Some(bytes);
    if dry_run {
        entry.action = Action::WouldRemove;
        report.freed_bytes += bytes;
        return;
    }
    let mut removed = remove_tree(dir).map_err(|err| io_error(dir, &err));
    if let (Ok(()), Some(target)) = (&removed, target) {
        removed = remove_tree(target).map_err(|err| io_error(target, &err));
    }
    match removed {
        Ok(()) => {
            entry.action = Action::Removed;
            report.freed_bytes += bytes;
        }
        Err(why) => report.errors.push(why),
    }
}

/// Applies the orphan rule to every per-checkout target directory not in `accounted`.
fn prune_target_dirs(
    layout: &Layout,
    dry_run: bool,
    accounted: &BTreeSet<PathBuf>,
    report: &mut PruneReport,
) {
    for dir in checkout_dirs(&layout.base) {
        if accounted.contains(&dir) {
            continue;
        }
        let (checkout, reason) = read_marker(&dir).map_or((None, Reason::Unattributed), |path| {
            let name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
            if name.as_deref() != Some(cargo::checkout_dir_name(&path).as_str()) {
                (None, Reason::MarkerMismatch)
            } else if path.exists() {
                (Some(path), Reason::CheckoutExists)
            } else {
                (Some(path), Reason::CheckoutGone)
            }
        });
        let mut entry = TargetDir {
            dir: dir.display().to_string(),
            checkout: checkout.map(|p| p.display().to_string()),
            action: Action::Kept,
            reason,
            bytes: None,
        };
        if reason == Reason::CheckoutGone {
            let bytes = dir_size(&dir);
            entry.bytes = Some(bytes);
            if dry_run {
                entry.action = Action::WouldRemove;
                report.freed_bytes += bytes;
            } else {
                match remove_tree(&dir) {
                    Ok(()) => {
                        entry.action = Action::Removed;
                        report.freed_bytes += bytes;
                    }
                    Err(err) => report.errors.push(io_error(&dir, &err)),
                }
            }
        }
        report.target_dirs.push(entry);
    }
}

/// Per tree ID, the accept receipts' verdicts and the newest receipt's time.
fn accept_runs(artifacts: &Path) -> BTreeMap<String, Runs> {
    let mut runs: BTreeMap<String, Runs> = BTreeMap::new();
    let Ok(entries) = fs::read_dir(artifacts.join("receipts")) else {
        return runs;
    };
    for path in entries.filter_map(|e| e.ok().map(|e| e.path())) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let json = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        if !name.starts_with("accept-") || !json {
            continue;
        }
        let Some(value) = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        else {
            continue;
        };
        let Some(tree) = value["tree"]
            .as_str()
            .filter(|_| value["schema"] == accept::SCHEMA)
        else {
            continue;
        };
        let run = runs.entry(tree.to_owned()).or_default();
        run.failed |= value["ok"] != true;
        let modified = fs::metadata(&path).and_then(|m| m.modified()).ok();
        run.newest = run.newest.max(modified);
    }
    runs
}

/// The accept-tree directories under `accept_root`: real directories (never symlinks)
/// named by a full git tree ID.
fn tree_dirs(accept_root: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(accept_root) else {
        return Vec::new();
    };
    let mut dirs: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_owned();
            is_tree_id(&name).then(|| (name, e.path()))
        })
        .collect();
    dirs.sort();
    dirs
}

/// The per-checkout target directories under `base`: real directories named
/// `keyval-checkout-` plus 16 hex digits.
fn checkout_dirs(base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| {
            e.file_name()
                .to_str()
                .and_then(|name| name.strip_prefix("keyval-checkout-"))
                .is_some_and(|hash| hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        })
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

/// The checkout path a target directory's marker names, if it has one.
fn read_marker(dir: &Path) -> Option<PathBuf> {
    let bytes = fs::read(dir.join(cargo::CHECKOUT_MARKER)).ok()?;
    let path = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    (!path.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(path)))
}

fn is_tree_id(name: &str) -> bool {
    matches!(name.len(), 40 | 64) && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The accept lock of a tree, if no other process holds it.
fn try_lock(path: &Path) -> Result<Option<File>, String> {
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|err| io_error(path, &err))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(io_error(path, &err)),
    }
}

/// Deletes a directory tree. Symlinks inside are removed, never followed. A tree that is
/// already gone (a concurrent prune got there first) counts as removed.
fn remove_tree(dir: &Path) -> io::Result<()> {
    match fs::remove_dir_all(dir) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed,
    }
}

/// The total size of the files under `dir`, without following symlinks.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(entries) = fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(entry.path()),
                Ok(_) => total += entry.metadata().map_or(0, |m| m.len()),
                Err(_) => {}
            }
        }
    }
    total
}

fn io_error(path: &Path, err: &io::Error) -> String {
    format!("{}: {err}", path.display())
}

fn write_receipt(report: &mut PruneReport, artifacts: &Path) {
    let receipts = artifacts.join("receipts");
    let path = receipts.join(format!(
        "prune-{}-{}.json",
        Utc::now().compact(),
        std::process::id()
    ));
    report.receipt_path = Some(path.display().to_string());
    let written = fs::create_dir_all(&receipts)
        .map_err(|err| err.to_string())
        .and_then(|()| serde_json::to_string_pretty(&*report).map_err(|err| err.to_string()))
        .and_then(|json| fs::write(&path, json + "\n").map_err(|err| err.to_string()));
    if let Err(why) = written {
        report.receipt_path = None;
        report.ok = false;
        report
            .errors
            .push(format!("cannot write the prune receipt: {why}"));
    }
}

impl Report for PruneReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        use std::fmt::Write as _;
        let verb = if self.dry_run { "would free" } else { "freed" };
        let mut out = format!(
            "prune{}: {verb} {} MB; keeping the newest {} accept trees and every failing run\n",
            if self.dry_run { " (dry run)" } else { "" },
            self.freed_bytes / 1_000_000,
            self.keep_accept_trees
        );
        for tree in &self.accept_trees {
            let _ = writeln!(
                out,
                "  {:?} accept tree {} ({:?})",
                tree.action, tree.tree, tree.reason
            );
        }
        for dir in &self.target_dirs {
            let _ = writeln!(
                out,
                "  {:?} target dir {} ({:?})",
                dir.action, dir.dir, dir.reason
            );
        }
        for error in &self.errors {
            let _ = writeln!(out, "  ERROR {error}");
        }
        if let Some(path) = &self.receipt_path {
            let _ = write!(out, "  receipt: {path}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    // check: DEV-PRUNE
    use super::*;
    use crate::test_support::scratch_dir;
    use std::time::Duration;

    /// A scratch workspace: root, target base and artifacts.
    fn layout(name: &str) -> Layout {
        let scratch = scratch_dir(name);
        let layout = Layout {
            root: scratch.join("root"),
            base: scratch.join("target-base"),
            artifacts: scratch.join("root/artifacts"),
        };
        fs::create_dir_all(layout.root.join(accept::ACCEPT_ROOT)).unwrap();
        fs::create_dir_all(&layout.base).unwrap();
        layout
    }

    fn tree_id(n: u32) -> String {
        format!("{n:040x}")
    }

    /// An accept tree with a file, its target dir, and (unless `ok` is `None`) one receipt
    /// whose modification time is `age_minutes` ago.
    fn accept_tree(
        layout: &Layout,
        n: u32,
        ok: Option<bool>,
        age_minutes: u64,
    ) -> (PathBuf, PathBuf) {
        let tree = tree_id(n);
        let dir = layout.root.join(accept::ACCEPT_ROOT).join(&tree);
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/lib.rs"), "//! tree\n").unwrap();
        let target = cargo::checkout_target_dir(&layout.base, &dir).unwrap();
        fs::create_dir_all(target.join("debug")).unwrap();
        fs::write(target.join("debug/blob"), vec![0_u8; 1000]).unwrap();
        let when = SystemTime::now() - Duration::from_secs(age_minutes * 60);
        if let Some(ok) = ok {
            let receipts = layout.artifacts.join("receipts");
            fs::create_dir_all(&receipts).unwrap();
            let path = receipts.join(format!("accept-br-x-{}-{n}.json", &tree[..12]));
            let receipt = serde_json::json!({
                "schema": accept::SCHEMA, "ok": ok, "bead": "br-x", "tree": tree,
                "finished_at": "2026-10-03T00:00:00Z"
            });
            fs::write(&path, receipt.to_string()).unwrap();
            File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(when)
                .unwrap();
        } else {
            File::open(&dir).unwrap().set_modified(when).unwrap();
        }
        (dir, target)
    }

    fn actions(report: &PruneReport) -> Vec<(String, Action, Reason)> {
        report
            .accept_trees
            .iter()
            .map(|t| (t.tree.clone(), t.action, t.reason))
            .collect()
    }

    #[test]
    fn the_newest_five_are_kept_and_older_passing_trees_go_with_their_target_dirs() {
        // N-1, N and N+1 passing trees around the retention boundary (Q9).
        for count in [
            KEEP_ACCEPT_TREES - 1,
            KEEP_ACCEPT_TREES,
            KEEP_ACCEPT_TREES + 1,
        ] {
            let layout = layout(&format!("prune-boundary-{count}"));
            let trees: Vec<(PathBuf, PathBuf)> = (0..count)
                .map(|n| {
                    accept_tree(
                        &layout,
                        u32::try_from(n).unwrap(),
                        Some(true),
                        10 * u64::try_from(n).unwrap(),
                    )
                })
                .collect();
            let report = run(&layout, false);
            assert!(report.ok, "count={count} report={report:#?}");
            let removed: Vec<&AcceptTree> = report
                .accept_trees
                .iter()
                .filter(|t| t.action == Action::Removed)
                .collect();
            assert_eq!(
                removed.len(),
                count.saturating_sub(KEEP_ACCEPT_TREES),
                "rule: keep the newest {KEEP_ACCEPT_TREES} accept trees (Q9); count={count} \
                 report={report:#?}"
            );
            for (n, (dir, target)) in trees.iter().enumerate() {
                let oldest_beyond_five = n >= KEEP_ACCEPT_TREES;
                assert_eq!(
                    (dir.exists(), target.exists()),
                    (!oldest_beyond_five, !oldest_beyond_five),
                    "rule: a removed tree takes its target dir with it; count={count} tree={n}"
                );
            }
            if let Some(gone) = removed.first() {
                assert_eq!(
                    gone.tree,
                    tree_id(u32::try_from(count - 1).unwrap()),
                    "the oldest goes"
                );
                assert!(gone.bytes.is_some_and(|b| b >= 1000), "gone={gone:#?}");
            }
            let receipt: serde_json::Value = serde_json::from_str(
                &fs::read_to_string(report.receipt_path.as_ref().unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(receipt["schema"], SCHEMA);
            let logged = receipt["accept_trees"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|t| t["action"] == "removed")
                .count();
            assert_eq!(
                logged,
                removed.len(),
                "rule: every removal is logged in the receipt"
            );
        }
    }

    #[test]
    fn failing_interrupted_and_in_use_trees_are_kept_past_the_newest_five() {
        let layout = layout("prune-kept-reasons");
        for n in 0..5 {
            accept_tree(&layout, n, Some(true), u64::from(n));
        }
        let (failing, _) = accept_tree(&layout, 10, Some(false), 100);
        let (interrupted, _) = accept_tree(&layout, 11, None, 101);
        let (locked, _) = accept_tree(&layout, 12, Some(true), 102);
        let (superseded, superseded_target) = accept_tree(&layout, 13, Some(true), 103);
        let held = try_lock(&locked.with_extension("lock")).unwrap().unwrap();
        let report = run(&layout, false);
        drop(held);
        let by_tree: BTreeMap<String, (Action, Reason)> = actions(&report)
            .into_iter()
            .map(|(tree, action, reason)| (tree, (action, reason)))
            .collect();
        assert_eq!(by_tree[&tree_id(10)], (Action::Kept, Reason::Failing));
        assert_eq!(by_tree[&tree_id(11)], (Action::Kept, Reason::NoReceipt));
        assert_eq!(by_tree[&tree_id(12)], (Action::Kept, Reason::InUse));
        assert_eq!(by_tree[&tree_id(13)], (Action::Removed, Reason::Superseded));
        assert!(
            failing.exists() && interrupted.exists() && locked.exists(),
            "rule: every failing run's artifacts are kept (Q9); report={report:#?}"
        );
        assert!(!superseded.exists() && !superseded_target.exists());
        assert!(
            superseded.with_extension("lock").exists(),
            "rule: lock files stay, so a waiting accept never shares a directory"
        );
    }

    #[test]
    fn a_dry_run_removes_nothing_and_reports_what_it_would() {
        let layout = layout("prune-dry-run");
        let trees: Vec<(PathBuf, PathBuf)> = (0..7)
            .map(|n| accept_tree(&layout, n, Some(true), u64::from(n)))
            .collect();
        let report = run(&layout, true);
        assert!(report.ok && report.dry_run, "report={report:#?}");
        let would: Vec<String> = report
            .accept_trees
            .iter()
            .filter(|t| t.action == Action::WouldRemove)
            .map(|t| t.tree.clone())
            .collect();
        assert_eq!(would, [tree_id(5), tree_id(6)]);
        assert!(report.freed_bytes >= 2000, "report={report:#?}");
        assert!(
            trees
                .iter()
                .all(|(dir, target)| dir.exists() && target.exists()),
            "rule: a dry run deletes nothing"
        );
        assert!(
            report.receipt_path.is_some(),
            "rule: a dry run is logged too"
        );
    }

    /// A target dir under `base` whose marker names `checkout` (`None`: no marker). With
    /// `honest`, its name is the one the marker's path hashes to.
    fn marked_target(base: &Path, name_for: &Path, checkout: Option<&Path>) -> PathBuf {
        let dir = base.join(cargo::checkout_dir_name(name_for));
        fs::create_dir_all(dir.join("debug")).unwrap();
        fs::write(dir.join("debug/blob"), vec![0_u8; 500]).unwrap();
        if let Some(checkout) = checkout {
            let mut bytes = checkout.as_os_str().as_bytes().to_vec();
            bytes.push(b'\n');
            fs::write(dir.join(cargo::CHECKOUT_MARKER), bytes).unwrap();
        }
        dir
    }

    #[test]
    fn a_target_dir_goes_only_when_its_marked_checkout_is_gone() {
        let layout = layout("prune-orphans");
        let scratch = layout.root.parent().unwrap().to_path_buf();
        let alive = scratch.join("alive-checkout");
        fs::create_dir_all(&alive).unwrap();
        let alive = fs::canonicalize(&alive).unwrap();
        let gone = scratch.join("gone-checkout");
        let orphan = marked_target(&layout.base, &gone, Some(&gone));
        let living = marked_target(&layout.base, &alive, Some(&alive));
        let liar = marked_target(&layout.base, &scratch.join("other"), Some(&gone));
        let unmarked = marked_target(&layout.base, &scratch.join("unmarked"), None);
        let unrelated = layout.base.join("debug");
        fs::create_dir_all(&unrelated).unwrap();
        let report = run(&layout, false);
        assert!(report.ok, "report={report:#?}");
        let by_dir: BTreeMap<String, (Action, Reason)> = report
            .target_dirs
            .iter()
            .map(|d| (d.dir.clone(), (d.action, d.reason)))
            .collect();
        let key = |p: &Path| p.display().to_string();
        assert_eq!(
            by_dir[&key(&orphan)],
            (Action::Removed, Reason::CheckoutGone)
        );
        assert_eq!(
            by_dir[&key(&living)],
            (Action::Kept, Reason::CheckoutExists)
        );
        assert_eq!(by_dir[&key(&liar)], (Action::Kept, Reason::MarkerMismatch));
        assert_eq!(
            by_dir[&key(&unmarked)],
            (Action::Kept, Reason::Unattributed)
        );
        assert!(
            !by_dir.contains_key(&key(&unrelated)),
            "rule: only keyval-checkout-* dirs"
        );
        assert_eq!(
            (
                orphan.exists(),
                living.exists(),
                liar.exists(),
                unmarked.exists(),
                unrelated.exists()
            ),
            (false, true, true, true, true),
            "rule: only a target dir whose marked checkout is gone is removed"
        );
    }

    #[test]
    fn nothing_outside_the_retention_rule_is_touched() {
        let layout = layout("prune-scope");
        for n in 0..6 {
            accept_tree(&layout, n, Some(true), u64::from(n));
        }
        let accept_root = layout.root.join(accept::ACCEPT_ROOT);
        let candidate = layout.root.join(".local/candidate/src");
        fs::create_dir_all(&candidate).unwrap();
        fs::create_dir_all(accept_root.join("not-a-tree")).unwrap();
        fs::write(accept_root.join("notes.txt"), "keep\n").unwrap();
        let outside = layout.root.parent().unwrap().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("precious"), "keep\n").unwrap();
        std::os::unix::fs::symlink(&outside, accept_root.join(tree_id(99))).unwrap();
        let report = run(&layout, false);
        assert!(report.ok, "report={report:#?}");
        assert_eq!(
            report.accept_trees.len(),
            6,
            "rule: only real directories named by a tree ID count; report={report:#?}"
        );
        assert!(
            candidate.exists()
                && accept_root.join("not-a-tree").exists()
                && accept_root.join("notes.txt").exists()
                && outside.join("precious").exists()
                && accept_root.join(tree_id(99)).exists(),
            "rule: prune deletes generated accept trees and target dirs, nothing else"
        );
    }
}
