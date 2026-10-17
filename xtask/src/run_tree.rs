//! The per-run tree of the T1, T2 and T3 gates (PLAN.md §10.10 "Tree isolation"; W46.1).
//!
//! T0 judges the commit wrapper's candidate. Every other tier materialises its own plain
//! directory, `.local/gates/<run-id>/tree`, from the committed tree it judges, verifies it
//! byte for byte against that tree (the same path set, blob IDs and modes as
//! [`candidate::verify`]), and runs the tree's own `./dev` there under git isolation, so
//! the judge is built from the tree under test and nothing it runs can see the shared
//! checkout. The directory is used once: a second run gets a new run ID, never a refreshed
//! directory, and it is verified again after the run so an edit during the run fails the
//! tier. Only `./dev prune` deletes it (D-74, Q9).

use std::path::{Path, PathBuf};

use crate::candidate;
use crate::gate::{self, GateTier};
use crate::git::Git;

/// Where run trees live, relative to the checkout root.
pub const DIR: &str = ".local/gates";

/// A materialised, verified run tree.
#[derive(Debug)]
pub struct RunTree {
    /// `<root>/.local/gates/<run-id>/tree`.
    pub dir: PathBuf,
    /// The git tree it holds.
    pub tree: String,
}

/// The run ID of a tier run: unique per process, so no two runs share a directory.
#[must_use]
pub fn run_id(tier: GateTier, tree: &str, stamp: &str, pid: u32) -> String {
    format!(
        "{}-{stamp}-{}-{pid}",
        gate::tier_name(tier),
        tree.get(..12).unwrap_or(tree)
    )
}

fn valid_tree(tree: &str) -> bool {
    matches!(tree.len(), 40 | 64) && tree.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Materialises `tree` into a fresh run directory below `base` (`<root>/.local/gates`) and
/// verifies it.
///
/// # Errors
///
/// A malformed tree ID, a run directory that already exists (a run tree is never
/// reused), or a materialisation or verification failure.
pub fn prepare(git: &Git, base: &Path, run_id: &str, tree: &str) -> Result<RunTree, String> {
    if !valid_tree(tree) {
        return Err(format!("{tree:?} is not a git tree object ID"));
    }
    if run_id.is_empty() || run_id.contains('/') || run_id.contains("..") {
        return Err(format!("{run_id:?} is not a usable run ID"));
    }
    let run_dir = base.join(run_id);
    if run_dir.exists() {
        return Err(format!(
            "{} already exists: a run tree is used once (§10.10); pick a new run ID",
            run_dir.display()
        ));
    }
    let dir = run_dir.join("tree");
    candidate::materialise(git, tree, &dir)?;
    candidate::verify(git, tree, &dir)?;
    Ok(RunTree {
        dir,
        tree: tree.to_owned(),
    })
}

/// Checks that the run left the tree exactly as materialised.
///
/// # Errors
///
/// A path added, removed, changed in content, or changed in mode.
pub fn verify(git: &Git, run: &RunTree) -> Result<usize, String> {
    candidate::verify(git, &run.tree, &run.dir)
}

/// Whether `root` is a run tree: `<checkout>/.local/gates/<run-id>/tree`. T1 to T3 refuse to
/// judge a tree anywhere else, so a caller cannot opt out of the isolation (§10.10).
#[must_use]
pub fn is_run_tree(root: &Path) -> bool {
    root.file_name().is_some_and(|name| name == "tree")
        && root
            .parent()
            .and_then(Path::parent)
            .is_some_and(|base| base.ends_with(DIR))
}

/// Copies every file below `from` that `to` lacks into `to`, keeping relative paths, and
/// returns how many it copied. Nothing in `to` is overwritten or removed.
///
/// # Errors
///
/// A directory that cannot be read or created, or a file that cannot be copied.
pub fn copy_missing(from: &Path, to: &Path) -> Result<usize, String> {
    let io = |what: &str, path: &Path, err: std::io::Error| {
        format!("cannot {what} {}: {err}", path.display())
    };
    if !from.is_dir() {
        return Ok(0);
    }
    std::fs::create_dir_all(to).map_err(|err| io("create", to, err))?;
    let mut copied = 0;
    for entry in std::fs::read_dir(from).map_err(|err| io("read", from, err))? {
        let entry = entry.map_err(|err| io("read", from, err))?;
        let (source, target) = (entry.path(), to.join(entry.file_name()));
        let kind = entry
            .file_type()
            .map_err(|err| io("inspect", &source, err))?;
        if kind.is_dir() {
            copied += copy_missing(&source, &target)?;
        } else if kind.is_file() && !target.exists() {
            std::fs::copy(&source, &target).map_err(|err| io("copy", &source, err))?;
            copied += 1;
        }
    }
    Ok(copied)
}

/// The arguments of the tree's own `./dev` for `tier`: judged on the asserted tree, with
/// the parent's registry from `parent_checks`, receipts under `artifacts`. `--changed` is
/// omitted on purpose: T1 and above test the whole workspace.
#[must_use]
pub fn inner_args(
    tier: GateTier,
    run: &RunTree,
    parent_checks: &Path,
    artifacts: &Path,
    allow_demotions: &[String],
    json: bool,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if json {
        args.push("--json".to_owned());
    }
    args.extend(["gate".to_owned(), gate::tier_name(tier).to_owned()]);
    args.extend(["--tree".to_owned(), run.tree.clone()]);
    args.extend([
        "--parent-checks".to_owned(),
        parent_checks.display().to_string(),
    ]);
    args.extend(["--artifacts".to_owned(), artifacts.display().to_string()]);
    for allowance in allow_demotions {
        args.extend(["--allow-demotion".to_owned(), allowance.clone()]);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::fixture;
    use crate::test_support::scratch_dir;
    use std::fs;

    fn tree_of(name: &str) -> (Git, String) {
        let git = fixture::repo(name, &[("a.txt", "alpha\n"), ("dir/b.txt", "beta\n")]);
        let tree = git.write_tree().unwrap();
        (git, tree)
    }

    // check: RUN-TREE
    #[test]
    fn a_run_tree_holds_exactly_the_tree_and_no_git_directory() {
        let (git, tree) = tree_of("runtree-exact");
        let base = scratch_dir("runtree-exact-base");
        let run = prepare(&git, &base, "t1-run", &tree).unwrap();
        assert_eq!(run.dir, base.join("t1-run").join("tree"));
        assert_eq!(
            verify(&git, &run),
            Ok(2),
            "rule: the run tree equals the tree"
        );
        assert!(
            !run.dir.join(".git").exists(),
            "rule: a run tree is git-isolated (§10.10); dir={:?}",
            run.dir
        );
    }

    #[test]
    fn a_run_directory_is_used_once() {
        let (git, tree) = tree_of("runtree-once");
        let base = scratch_dir("runtree-once-base");
        prepare(&git, &base, "t1-run", &tree).unwrap();
        let again = prepare(&git, &base, "t1-run", &tree).unwrap_err();
        assert!(
            again.contains("already exists"),
            "rule: a run tree is never refreshed in place; got {again:?}"
        );
    }

    #[test]
    fn a_modified_or_stray_file_fails_the_post_run_verification() {
        let (git, tree) = tree_of("runtree-edit");
        let base = scratch_dir("runtree-edit-base");
        let run = prepare(&git, &base, "t2-run", &tree).unwrap();
        fs::write(run.dir.join("a.txt"), "changed\n").unwrap();
        assert!(
            verify(&git, &run).is_err(),
            "rule: an edited file is caught"
        );
        fs::write(run.dir.join("a.txt"), "alpha\n").unwrap();
        assert_eq!(verify(&git, &run), Ok(2));
        fs::write(run.dir.join("stray.txt"), "x\n").unwrap();
        assert!(verify(&git, &run).is_err(), "rule: a stray file is caught");
    }

    #[test]
    fn malformed_tree_and_run_ids_are_refused_before_anything_is_written() {
        let (git, tree) = tree_of("runtree-ids");
        let base = scratch_dir("runtree-ids-base");
        for bad_tree in ["", "HEAD", "xyz", &tree[..39]] {
            assert!(
                prepare(&git, &base, "ok", bad_tree).is_err(),
                "rule: tree {bad_tree:?} is not an object ID"
            );
        }
        for bad_run in ["", "a/b", "..", "x..y"] {
            assert!(
                prepare(&git, &base, bad_run, &tree).is_err(),
                "rule: run ID {bad_run:?} is unusable"
            );
        }
        assert!(!base.join("ok").exists(), "rule: nothing was materialised");
    }

    #[test]
    fn the_inner_command_judges_the_asserted_tree_without_a_changed_list() {
        let run = RunTree {
            dir: PathBuf::from("/x/tree"),
            tree: "a".repeat(40),
        };
        let args = inner_args(
            GateTier::Integration,
            &run,
            Path::new("/x/parent.toml"),
            Path::new("/x/art"),
            &["K=br".to_owned()],
            true,
        );
        assert_eq!(
            args,
            [
                "--json",
                "gate",
                gate::tier_name(GateTier::Integration),
                "--tree",
                &"a".repeat(40),
                "--parent-checks",
                "/x/parent.toml",
                "--artifacts",
                "/x/art",
                "--allow-demotion",
                "K=br"
            ],
            "rule: T1+ test the whole workspace, so no --changed"
        );
    }

    #[test]
    fn only_a_gates_run_directory_is_a_run_tree() {
        assert!(is_run_tree(Path::new("/c/.local/gates/t1-x/tree")));
        for other in [
            "/c",
            "/c/.local/gates/t1-x",
            "/c/.local/gates/tree",
            "/c/gates/t1-x/tree",
        ] {
            assert!(
                !is_run_tree(Path::new(other)),
                "rule: {other} is not <checkout>/.local/gates/<run>/tree"
            );
        }
    }

    #[test]
    fn copy_missing_adds_files_and_never_overwrites() {
        let base = scratch_dir("runtree-copy");
        let (from, to) = (base.join("from"), base.join("to"));
        fs::create_dir_all(from.join("receipts")).unwrap();
        fs::create_dir_all(to.join("receipts")).unwrap();
        fs::write(from.join("receipts/new.json"), "new").unwrap();
        fs::write(from.join("receipts/old.json"), "from-run").unwrap();
        fs::write(to.join("receipts/old.json"), "shared").unwrap();
        assert_eq!(copy_missing(&from, &to), Ok(1));
        assert_eq!(
            fs::read_to_string(to.join("receipts/new.json")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(to.join("receipts/old.json")).unwrap(),
            "shared",
            "rule: an existing receipt is never overwritten"
        );
        assert_eq!(copy_missing(&base.join("absent"), &to), Ok(0));
    }

    #[test]
    fn run_ids_are_unique_per_process_and_name_the_tier_and_tree() {
        let a = run_id(
            GateTier::Integration,
            &"c".repeat(40),
            "20261017T000000Z",
            7,
        );
        let b = run_id(
            GateTier::Integration,
            &"c".repeat(40),
            "20261017T000000Z",
            8,
        );
        assert_ne!(a, b, "rule: two processes never share a run directory");
        assert!(a.contains(&"c".repeat(12)), "run id {a:?} names the tree");
    }
}
