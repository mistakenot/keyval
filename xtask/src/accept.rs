//! `./dev accept <bead> --tree <id>`: run a bead's acceptance selectors on the named
//! committed tree and write a receipt naming that tree (PLAN.md §11.3 step 7, P11).
//!
//! 1. The tree must be the tree of a commit on `main` ("the current tree" is the tree of
//!    the commit that carried the work).
//! 2. The bead's selectors come from `registry/workgraph.toml` `accept` for a canonical
//!    work item (matched by recorded bead ID, else by its `plan:W<nn>` label), or from
//!    `accept:<CHECK-ID>` labels on a scoped subtask or discovered-work bead. Those are
//!    tracker metadata, so the canonical work graph gains no rows. An empty selection
//!    fails (P10).
//! 3. The tree is materialised into `.local/accept/<tree-id>/`, refreshed in place if a
//!    previous accept left it, under a per-directory lock and outside the commit lock. It
//!    is content-verified like the candidate.
//! 4. Each selector runs there, git-isolated (§11.10 step 4), building into the
//!    directory's own target dir (`cargo::checkout_target_dir`,
//!    br-gate-shared-target-dir-dtqu). A new accept tree is therefore a cold build.
//! 5. The tree is verified again, so tests cannot have edited it, and the receipt is
//!    written to `artifacts/receipts/`.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use crate::candidate;
use crate::cargo;
use crate::clock::Utc;
use crate::git::Git;
use crate::output::Report;
use crate::process;
use crate::registry;
use crate::tracker::Tracker;

/// Schema identifier of the accept receipt and `--json` report.
pub const SCHEMA: &str = "keyval.dev.accept/v1";

/// Accept trees live under here, one directory per tree ID.
pub const ACCEPT_ROOT: &str = ".local/accept";

/// Label prefix naming a check that accepts a non-canonical bead.
pub const LABEL_PREFIX: &str = "accept:";

const SELECTOR_LIMIT: Duration = Duration::from_mins(60);

/// Where the selectors came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectorSource {
    /// `registry/workgraph.toml` `accept` of the bead's canonical work item.
    Workgraph,
    /// `accept:<CHECK-ID>` labels on the bead (scoped subtask or discovered work).
    Labels,
}

/// One executed selector.
#[derive(Clone, Debug, Serialize)]
pub struct SelectorResult {
    /// The selector as registered, e.g. `./dev check DEV-COMMIT`.
    pub selector: String,
    /// Whether it passed.
    pub ok: bool,
    /// Per-check outcome lines, e.g. `DEV-COMMIT passed 14`.
    pub checks: Vec<String>,
    /// Why it failed, when it failed.
    pub detail: Option<String>,
}

/// The `./dev accept` report and receipt (`keyval.dev.accept/v1`).
#[derive(Debug, Serialize)]
pub struct AcceptReport {
    /// Schema identifier.
    pub schema: &'static str,
    /// True when every selector passed on an untouched tree.
    pub ok: bool,
    /// The bead accepted.
    pub bead: String,
    /// The tree judged.
    pub tree: String,
    /// The newest `main` commit with that tree.
    pub commit: Option<String>,
    /// The work item, for a canonical bead.
    pub item: Option<String>,
    /// Where the selectors came from.
    pub source: Option<SelectorSource>,
    /// Every selector and its result.
    pub selectors: Vec<SelectorResult>,
    /// The materialised tree.
    pub accept_dir: String,
    /// The target dir its builds used.
    pub cargo_target_dir: Option<String>,
    /// Why acceptance did not pass, if it did not.
    pub detail: Option<String>,
    /// Start time (UTC).
    pub started_at: String,
    /// End time (UTC).
    pub finished_at: String,
    /// Where this receipt was written.
    pub receipt_path: Option<String>,
}

/// Runs one selector (its words after `./dev`) in the accept directory.
pub trait SelectorRunner {
    /// Returns the selector's result.
    fn run(&self, accept_dir: &Path, target_dir: &Path, args: &[String]) -> SelectorResult;
}

/// The bead's selectors and where they came from, or why there are none.
pub fn selectors(
    root: &Path,
    tracker: &dyn Tracker,
    bead: &str,
) -> Result<(SelectorSource, Option<String>, Vec<String>), String> {
    let info = tracker.bead(bead)?;
    let items = registry::load_workgraph(root)?.unwrap_or_default();
    if let Some(item) = registry::item_for_bead(&items, bead, &info.labels) {
        return Ok((
            SelectorSource::Workgraph,
            Some(item.id.clone()),
            item.accept.clone(),
        ));
    }
    let labelled = info
        .labels
        .iter()
        .filter_map(|label| label.strip_prefix(LABEL_PREFIX))
        .map(|check| format!("./dev check {check}"))
        .collect();
    Ok((SelectorSource::Labels, None, labelled))
}

/// The words after `./dev` of a supported selector: `./dev check <ID>... [--case NAME]`.
pub fn parse_selector(selector: &str) -> Result<Vec<String>, String> {
    let words: Vec<String> = selector.split_whitespace().map(str::to_owned).collect();
    match words.as_slice() {
        [dev, check, rest @ ..] if dev == "./dev" && check == "check" && !rest.is_empty() => {
            Ok(words.into_iter().skip(1).collect())
        }
        _ => Err(format!(
            "unsupported selector {selector:?}: acceptance runs `./dev check <ID>... [--case NAME]`"
        )),
    }
}

/// The newest commit on `branch` whose tree is `tree`.
fn commit_with_tree(git: &Git, branch: &str, tree: &str) -> Result<Option<String>, String> {
    let log = git.run(&["log", "--format=%H %T", branch])?;
    Ok(log.lines().find_map(|line| {
        let (commit, commit_tree) = line.split_once(' ')?;
        (commit_tree == tree).then(|| commit.to_owned())
    }))
}

/// Runs acceptance for `bead` on `tree`, writing the receipt under `artifacts`.
pub fn run(
    git: &Git,
    tracker: &dyn Tracker,
    runner: &dyn SelectorRunner,
    bead: &str,
    tree: &str,
    artifacts: &Path,
) -> AcceptReport {
    let accept_dir = git.root().join(ACCEPT_ROOT).join(tree);
    let mut report = AcceptReport {
        schema: SCHEMA,
        ok: false,
        bead: bead.to_owned(),
        tree: tree.to_owned(),
        commit: None,
        item: None,
        source: None,
        selectors: Vec::new(),
        accept_dir: accept_dir.display().to_string(),
        cargo_target_dir: None,
        detail: None,
        started_at: Utc::now().rfc3339(),
        finished_at: String::new(),
        receipt_path: None,
    };
    if let Err(why) = accept(git, tracker, runner, &accept_dir, &mut report) {
        report.detail = Some(why);
    }
    report.ok = report.detail.is_none();
    report.finished_at = Utc::now().rfc3339();
    write_receipt(&mut report, artifacts);
    report
}

fn accept(
    git: &Git,
    tracker: &dyn Tracker,
    runner: &dyn SelectorRunner,
    accept_dir: &Path,
    report: &mut AcceptReport,
) -> Result<(), String> {
    let tree = report.tree.clone();
    let valid = matches!(tree.len(), 40 | 64) && tree.bytes().all(|b| b.is_ascii_hexdigit());
    if !valid {
        return Err(format!("--tree {tree:?} is not a git tree object ID"));
    }
    report.commit = commit_with_tree(git, crate::commit::BRANCH, &tree)?;
    if report.commit.is_none() {
        return Err(format!(
            "tree {tree} is not the tree of any commit on {}; acceptance judges committed \
             trees only (§11.3 step 7)",
            crate::commit::BRANCH
        ));
    }
    let (source, item, selectors) = selectors(git.root(), tracker, &report.bead)?;
    report.source = Some(source);
    report.item = item;
    if selectors.is_empty() {
        return Err(format!(
            "no acceptance selectors for {} ({source:?}); register them in \
             registry/workgraph.toml `accept` (canonical items) or as `accept:<CHECK-ID>` \
             labels (subtasks, discovered work). An empty selection fails (P10)",
            report.bead
        ));
    }
    let parsed: Vec<(String, Vec<String>)> = selectors
        .iter()
        .map(|s| parse_selector(s).map(|args| (s.clone(), args)))
        .collect::<Result<_, _>>()?;

    let _lock = lock(accept_dir)?;
    candidate::materialise(git, &tree, accept_dir)?;
    candidate::verify(git, &tree, accept_dir)?;
    let target = cargo::checkout_target_dir(&cargo::target_base(git.root()), accept_dir)?;
    report.cargo_target_dir = Some(target.display().to_string());
    for (selector, args) in parsed {
        let mut result = runner.run(accept_dir, &target, &args);
        result.selector = selector;
        report.selectors.push(result);
    }
    candidate::verify(git, &tree, accept_dir)
        .map_err(|why| format!("the selectors modified the accept tree: {why}"))?;
    let failed: Vec<&str> = report
        .selectors
        .iter()
        .filter(|r| !r.ok)
        .map(|r| r.selector.as_str())
        .collect();
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("selector(s) failed: {failed:?}"))
    }
}

/// Holds an accept directory's lock until dropped.
struct AcceptLock {
    _file: File,
}

fn lock(accept_dir: &Path) -> Result<AcceptLock, String> {
    let path = accept_dir.with_extension("lock");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("{}: {err}", parent.display()))?;
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    file.lock()
        .map_err(|err| format!("accept lock {}: {err}", path.display()))?;
    Ok(AcceptLock { _file: file })
}

fn write_receipt(report: &mut AcceptReport, artifacts: &Path) {
    let receipts = artifacts.join("receipts");
    let path = receipts.join(format!(
        "accept-{}-{}-{}-{}.json",
        report.bead,
        report.tree.get(..12).unwrap_or(&report.tree),
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
        report.detail = Some(format!("cannot write the accept receipt: {why}"));
    }
}

/// Runs selectors as `xtask --json <args>` in the accept directory, git-isolated and
/// building into the directory's own target dir.
pub struct LiveRunner;

impl SelectorRunner for LiveRunner {
    fn run(&self, accept_dir: &Path, target_dir: &Path, args: &[String]) -> SelectorResult {
        let mut result = SelectorResult {
            selector: String::new(),
            ok: false,
            checks: Vec::new(),
            detail: None,
        };
        let xtask_self = match std::env::current_exe() {
            Ok(path) => path,
            Err(err) => {
                result.detail = Some(err.to_string());
                return result;
            }
        };
        let mut cmd = Command::new(&xtask_self);
        cmd.arg("--json").args(args).current_dir(accept_dir);
        for (key, value) in isolated_env(accept_dir, target_dir) {
            cmd.env(key, value);
        }
        let captured = match process::run(&mut cmd, SELECTOR_LIMIT) {
            Ok(captured) => captured,
            Err(err) => {
                result.detail = Some(format!("cannot run the selector: {err}"));
                return result;
            }
        };
        match serde_json::from_str::<serde_json::Value>(&captured.stdout) {
            Ok(value) => {
                result.ok = value["ok"] == true;
                result.checks = value["checks"]
                    .as_array()
                    .map(|checks| {
                        checks
                            .iter()
                            .map(|c| format!("{} {} {}", c["id"], c["status"], c["passed"]))
                            .collect()
                    })
                    .unwrap_or_default();
                if !result.ok {
                    result.detail = value["error"]["message"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| Some(format!("check report: {value}")));
                }
            }
            Err(err) => {
                result.detail = Some(format!(
                    "selector output is not JSON ({err}); stderr: {}",
                    captured
                        .stderr
                        .lines()
                        .rev()
                        .take(5)
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
            }
        }
        result
    }
}

/// The environment of a run in an accept directory: git-isolated, rooted there, and
/// building into its own target dir under the shared base.
pub fn isolated_env(accept_dir: &Path, target_dir: &Path) -> Vec<(&'static str, PathBuf)> {
    let mut env: Vec<(&'static str, PathBuf)> = candidate::git_isolation(accept_dir)
        .into_iter()
        .map(|(key, value)| (key, PathBuf::from(value)))
        .collect();
    env.push(("KEYVAL_ROOT", accept_dir.to_path_buf()));
    env.push((cargo::TARGET_DIR_ENV, target_dir.to_path_buf()));
    if let Some(base) = target_dir.parent() {
        env.push((cargo::TARGET_BASE_ENV, base.to_path_buf()));
    }
    env
}

impl Report for AcceptReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!(
            "accept {} on tree {}: {}\n",
            self.bead,
            self.tree,
            if self.ok { "passed" } else { "FAILED" }
        );
        for selector in &self.selectors {
            let _ = writeln!(
                out,
                "  {} {} {}",
                if selector.ok { "passed" } else { "FAILED" },
                selector.selector,
                selector.checks.join(", ")
            );
            if let Some(detail) = &selector.detail {
                let _ = writeln!(out, "      {detail}");
            }
        }
        if let Some(detail) = &self.detail {
            let _ = writeln!(out, "  {detail}");
        }
        if let Some(path) = &self.receipt_path {
            let _ = write!(out, "  receipt: {path}");
        }
        out
    }
}

#[cfg(test)]
pub mod tests {
    // check: DEV-ACCEPT
    use super::*;
    use crate::git::fixture;
    use crate::test_support::scratch_dir;
    use crate::tracker::Bead;
    use std::cell::RefCell;

    /// A tracker over fixed beads; records closures and failures.
    pub struct FakeTracker {
        pub beads: RefCell<Vec<Bead>>,
        pub flushed: RefCell<u32>,
    }

    impl FakeTracker {
        pub fn new(beads: &[(&str, &str, &[&str])]) -> Self {
            Self {
                beads: RefCell::new(
                    beads
                        .iter()
                        .map(|(id, status, labels)| Bead {
                            id: (*id).to_owned(),
                            status: (*status).to_owned(),
                            labels: labels.iter().map(|l| (*l).to_owned()).collect(),
                        })
                        .collect(),
                ),
                flushed: RefCell::new(0),
            }
        }

        pub fn status(&self, id: &str) -> String {
            self.bead(id).map(|b| b.status).unwrap_or_default()
        }
    }

    impl Tracker for FakeTracker {
        fn bead(&self, id: &str) -> Result<Bead, String> {
            self.beads
                .borrow()
                .iter()
                .find(|b| b.id == id)
                .cloned()
                .ok_or_else(|| format!("no bead {id}"))
        }
        fn all_beads(&self) -> Result<Vec<Bead>, String> {
            Ok(self.beads.borrow().clone())
        }
        fn close(&self, id: &str, _reason: &str) -> Result<(), String> {
            let mut beads = self.beads.borrow_mut();
            let bead = beads.iter_mut().find(|b| b.id == id).ok_or("no bead")?;
            bead.status = "closed".to_owned();
            Ok(())
        }
        fn mark_failed(&self, id: &str, _comment: &str) -> Result<(), String> {
            let mut beads = self.beads.borrow_mut();
            let bead = beads.iter_mut().find(|b| b.id == id).ok_or("no bead")?;
            bead.status = "open".to_owned();
            bead.labels
                .extend(["failed".to_owned(), "needs-owner".to_owned()]);
            Ok(())
        }
        fn flush(&self) -> Result<(), String> {
            *self.flushed.borrow_mut() += 1;
            Ok(())
        }
    }

    /// Passes every selector except those containing `fail_on`; records what ran.
    struct FakeRunner {
        fail_on: Option<&'static str>,
        ran: RefCell<Vec<(PathBuf, PathBuf, Vec<String>)>>,
        tamper: bool,
    }

    impl FakeRunner {
        fn passing() -> Self {
            Self {
                fail_on: None,
                ran: RefCell::new(Vec::new()),
                tamper: false,
            }
        }
    }

    impl SelectorRunner for FakeRunner {
        fn run(&self, accept_dir: &Path, target_dir: &Path, args: &[String]) -> SelectorResult {
            self.ran.borrow_mut().push((
                accept_dir.to_path_buf(),
                target_dir.to_path_buf(),
                args.to_vec(),
            ));
            if self.tamper {
                fs::write(accept_dir.join("a.rs"), "tampered\n").unwrap();
            }
            let ok = self.fail_on.is_none_or(|f| !args.join(" ").contains(f));
            SelectorResult {
                selector: String::new(),
                ok,
                checks: vec![format!(
                    "{} {}",
                    args.join(" "),
                    if ok { "passed" } else { "failed" }
                )],
                detail: (!ok).then(|| "fake failure".to_owned()),
            }
        }
    }

    const WORKGRAPH: &str = "[[item]]\nid = \"W07\"\nreqs = [\"P11\"]\naccept = [\"./dev check DEV-COMMIT\", \"./dev check DEV-GATE --case isolation\"]\nbead = \"\"\n";

    /// A repo whose committed tree carries the work graph; returns (git, tree).
    pub fn repo(name: &str) -> (Git, String) {
        let git = fixture::repo(
            name,
            &[("a.rs", "a\n"), ("registry/workgraph.toml", WORKGRAPH)],
        );
        let committed = git.rev_parse("HEAD^{tree}").unwrap();
        (git, committed)
    }

    fn accept_in(
        git: &Git,
        tracker: &FakeTracker,
        runner: &FakeRunner,
        bead: &str,
        tree: &str,
    ) -> AcceptReport {
        run(
            git,
            tracker,
            runner,
            bead,
            tree,
            &scratch_dir("accept-artifacts"),
        )
    }

    #[test]
    fn a_canonical_bead_runs_its_workgraph_selectors_in_its_own_tree_dir() {
        let (git, tree) = repo("accept-canonical");
        let tracker = FakeTracker::new(&[("br-w07", "in_progress", &["m0", "plan:W07"])]);
        let runner = FakeRunner::passing();
        let report = accept_in(&git, &tracker, &runner, "br-w07", &tree);
        assert!(report.ok, "report={report:#?}");
        assert_eq!(report.source, Some(SelectorSource::Workgraph));
        assert_eq!(report.item.as_deref(), Some("W07"));
        assert_eq!(report.commit, Some(git.rev_parse("HEAD").unwrap()));
        let ran = runner.ran.borrow();
        let accept_dir = git.root().join(ACCEPT_ROOT).join(&tree);
        assert_eq!(ran.len(), 2);
        assert_eq!(
            ran[0].0, accept_dir,
            "rule: acceptance runs in .local/accept/<tree-id>/"
        );
        assert_eq!(ran[1].2, ["check", "DEV-GATE", "--case", "isolation"]);
        assert_eq!(
            ran[0].1,
            cargo::checkout_target_dir(&cargo::target_base(git.root()), &accept_dir).unwrap(),
            "rule: the accept tree builds into its own target dir (br-gate-shared-target-dir-dtqu)"
        );
        assert_eq!(candidate::verify(&git, &tree, &accept_dir), Ok(2));
        let receipt: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(report.receipt_path.as_ref().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            (&receipt["schema"], &receipt["tree"], &receipt["ok"]),
            (
                &serde_json::json!(SCHEMA),
                &serde_json::json!(tree),
                &serde_json::json!(true)
            ),
            "rule: the receipt names the judged tree; receipt={receipt:#}"
        );
    }

    #[test]
    fn a_subtask_is_accepted_through_its_accept_labels_without_workgraph_rows() {
        let (git, tree) = repo("accept-subtask");
        let tracker = FakeTracker::new(&[(
            "br-w07-3",
            "in_progress",
            &["m0", "sub:W07", "accept:DEV-ACCEPT", "accept:DEV-COMMIT"],
        )]);
        let runner = FakeRunner::passing();
        let report = accept_in(&git, &tracker, &runner, "br-w07-3", &tree);
        assert!(report.ok, "report={report:#?}");
        assert_eq!(report.source, Some(SelectorSource::Labels));
        let selectors: Vec<&str> = report
            .selectors
            .iter()
            .map(|s| s.selector.as_str())
            .collect();
        assert_eq!(
            selectors,
            ["./dev check DEV-ACCEPT", "./dev check DEV-COMMIT"]
        );
    }

    #[test]
    fn acceptance_fails_without_selectors_on_uncommitted_trees_and_on_failures() {
        let (git, tree) = repo("accept-refusals");
        let tracker = FakeTracker::new(&[
            ("br-none", "in_progress", &["sub:W07"]),
            ("br-w07", "in_progress", &["plan:W07"]),
        ]);
        let none = accept_in(&git, &tracker, &FakeRunner::passing(), "br-none", &tree);
        assert!(
            !none.ok && none.detail.as_deref().is_some_and(|d| d.contains("P10")),
            "rule: no selectors is a failure, never a pass; report={none:#?}"
        );
        fixture::write(git.root(), "a.rs", "uncommitted\n");
        git.stage(&["a.rs".to_owned()]).unwrap();
        let staged_tree = git.write_tree().unwrap();
        let uncommitted = accept_in(
            &git,
            &tracker,
            &FakeRunner::passing(),
            "br-w07",
            &staged_tree,
        );
        assert!(
            !uncommitted.ok
                && uncommitted
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("not the tree of any commit")),
            "rule: only committed trees are accepted; report={uncommitted:#?}"
        );
        let failing = FakeRunner {
            fail_on: Some("DEV-GATE"),
            ran: RefCell::new(Vec::new()),
            tamper: false,
        };
        let failed = accept_in(&git, &tracker, &failing, "br-w07", &tree);
        assert!(!failed.ok, "report={failed:#?}");
        assert!(
            failed.receipt_path.is_some(),
            "rule: a failing run still leaves its receipt"
        );
    }

    #[test]
    fn a_selector_that_edits_the_accept_tree_fails_acceptance() {
        let (git, tree) = repo("accept-tamper");
        let tracker = FakeTracker::new(&[("br-w07", "in_progress", &["plan:W07"])]);
        let runner = FakeRunner {
            fail_on: None,
            ran: RefCell::new(Vec::new()),
            tamper: true,
        };
        let report = accept_in(&git, &tracker, &runner, "br-w07", &tree);
        assert!(
            !report.ok
                && report
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("modified the accept tree")),
            "report={report:#?}"
        );
    }

    #[test]
    fn selectors_must_be_dev_check_command_lines() {
        assert_eq!(
            parse_selector("./dev check K-5 --case committed-prefix"),
            Ok(vec![
                "check".to_owned(),
                "K-5".to_owned(),
                "--case".to_owned(),
                "committed-prefix".to_owned()
            ])
        );
        for bad in ["./dev check", "cargo test", "./dev gate fast", "rm -rf /"] {
            assert!(parse_selector(bad).is_err(), "selector={bad:?}");
        }
    }

    #[test]
    fn the_accept_environment_is_git_isolated_and_uses_the_trees_target_dir() {
        let env = isolated_env(
            Path::new("/w/.local/accept/abc"),
            Path::new("/t/keyval-checkout-1"),
        );
        let get = |key: &str| env.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone());
        assert_eq!(
            get("GIT_DIR"),
            Some(PathBuf::from(candidate::ISOLATED_GIT_DIR))
        );
        assert_eq!(
            get("GIT_CEILING_DIRECTORIES"),
            Some(PathBuf::from("/w/.local/accept/abc:/w/.local/accept"))
        );
        assert_eq!(
            get("KEYVAL_ROOT"),
            Some(PathBuf::from("/w/.local/accept/abc"))
        );
        assert_eq!(
            get(cargo::TARGET_DIR_ENV),
            Some(PathBuf::from("/t/keyval-checkout-1"))
        );
        assert_eq!(get(cargo::TARGET_BASE_ENV), Some(PathBuf::from("/t")));
    }
}
