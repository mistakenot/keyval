//! `./dev commit --close <bead>` and `./dev commit --close <bead> --failed --evidence
//! <path>`: the bead-closure lifecycle (PLAN.md §11.3 step 7, §11.4, §10.13).
//!
//! **Close.** Validate the bead's acceptance receipt: the newest passing
//! `keyval.dev.accept/v1` receipt for the bead must name the tree of the newest `main`
//! commit carrying the bead's work (trailer `Bead: <id>`). Then `br close`,
//! `br sync --flush-only`, and recomputation of requirement activation
//! ([`crate::registry`]). `.beads/issues.jsonl`, plus `registry/requirements.toml` if a
//! requirement activated, are committed through the wrapper as one closure commit
//! (trailer `Closes: <id>`). Re-running it on an already-closed bead finishes an
//! interrupted closure.
//!
//! **Failed** (a failed spike, §11.4, D-70). Record the failure-evidence report, label
//! the bead `failed` + `needs-owner`, and leave it **open**, so its dependants stay
//! blocked. Then commit `.beads/` plus the evidence file through the wrapper (trailer
//! `Failed: <id>`).
//!
//! Beads closed before W07.3 landed carry the coordinator's bootstrap hand receipts in
//! their comments (§11.2 step 2). That exception stays explicit: from now on, `--close`
//! requires an accept receipt.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::accept;
use crate::commit::{self, CommitKind, CommitReport, Ports};
use crate::git::Git;
use crate::output::Report;
use crate::registry;
use crate::tracker::Tracker;

/// Schema identifier of the `--json` report.
pub const SCHEMA: &str = "keyval.dev.close/v1";

/// The tracker export the closure commit carries.
pub const BEADS_EXPORT: &str = ".beads/issues.jsonl";

/// What the agent asked for.
#[derive(Clone, Debug)]
pub struct CloseRequest {
    /// The bead to close (or record as failed).
    pub bead: String,
    /// The Agent Mail identity whose reservations cover `.beads/` (and the registry or
    /// evidence file).
    pub agent: String,
    /// The closure commit's message.
    pub message: String,
    /// For `--failed`: the failure-evidence report, relative to the root.
    pub failed_evidence: Option<String>,
}

/// The `--close` / `--failed` report (`keyval.dev.close/v1`).
#[derive(Debug, Serialize)]
pub struct CloseReport {
    /// Schema identifier.
    pub schema: &'static str,
    /// True when the closure (or failure record) was committed.
    pub ok: bool,
    /// `close` or `failed`.
    pub mode: &'static str,
    /// The bead.
    pub bead: String,
    /// The accept receipt that justified the closure.
    pub accept_receipt: Option<String>,
    /// The commit that carried the work.
    pub work_commit: Option<String>,
    /// That commit's tree, which the receipt must name.
    pub work_tree: Option<String>,
    /// Requirements this closure activated.
    pub activated: Vec<String>,
    /// The wrapped commit's report.
    pub commit: Option<CommitReport>,
    /// Why it stopped, if it did.
    pub detail: Option<String>,
}

/// Runs `--close` or `--failed` for `request`.
pub fn run(
    git: &Git,
    request: &CloseRequest,
    tracker: &dyn Tracker,
    ports: &dyn Ports,
    artifacts: &Path,
) -> CloseReport {
    let mut report = CloseReport {
        schema: SCHEMA,
        ok: false,
        mode: if request.failed_evidence.is_some() {
            "failed"
        } else {
            "close"
        },
        bead: request.bead.clone(),
        accept_receipt: None,
        work_commit: None,
        work_tree: None,
        activated: Vec::new(),
        commit: None,
        detail: None,
    };
    let prepared = request.failed_evidence.as_deref().map_or_else(
        || close(git, request, tracker, artifacts, &mut report),
        |evidence| record_failure(git, request, tracker, evidence),
    );
    let (kind, paths) = match prepared {
        Ok(prepared) => prepared,
        Err(why) => {
            report.detail = Some(why);
            return report;
        }
    };
    let wrapped = commit::Request {
        kind,
        bead: request.bead.clone(),
        agent: request.agent.clone(),
        message: request.message.clone(),
        paths,
    };
    let committed = commit::run(git, &wrapped, ports, artifacts);
    report.ok = committed.ok;
    if !committed.ok {
        report.detail = Some(format!(
            "the {} commit was not made: {}. The tracker change stands; re-run the same \
             command to finish it",
            report.mode,
            committed
                .detail
                .as_deref()
                .unwrap_or("see the commit report")
        ));
    }
    report.commit = Some(committed);
    report
}

fn close(
    git: &Git,
    request: &CloseRequest,
    tracker: &dyn Tracker,
    artifacts: &Path,
    report: &mut CloseReport,
) -> Result<(CommitKind, Vec<String>), String> {
    let bead = tracker.bead(&request.bead)?;
    let receipt = latest_passing_receipt(artifacts, &request.bead)?.ok_or_else(|| {
        format!(
            "no passing accept receipt for {}; run ./dev accept {} --tree <tree of the work \
             commit> first (rule 16)",
            request.bead, request.bead
        )
    })?;
    report.accept_receipt = Some(receipt.path.display().to_string());
    let (work_commit, work_tree) = work_commit(git, &request.bead)?.ok_or_else(|| {
        format!(
            "no commit on {} carries `Bead: {}`; the work must be committed (through ./dev \
             commit) before the bead can close",
            commit::BRANCH,
            request.bead
        )
    })?;
    report.work_commit = Some(work_commit.clone());
    report.work_tree = Some(work_tree.clone());
    if receipt.tree != work_tree {
        return Err(format!(
            "wrong tree: the newest passing accept receipt ({}) names tree {}, but the work \
             commit {work_commit} has tree {work_tree}; accept that tree (P11)",
            receipt.path.display(),
            receipt.tree
        ));
    }
    if bead.status != "closed" {
        tracker.close(
            &request.bead,
            &format!(
                "Accepted on tree {work_tree} (work commit {work_commit}); receipt {}",
                receipt.path.display()
            ),
        )?;
    }
    tracker.flush()?;
    let mut paths = vec![BEADS_EXPORT.to_owned()];
    report.activated = activate(git.root(), tracker, &request.bead, &bead.labels)?;
    if !report.activated.is_empty() {
        paths.push(registry::REQUIREMENTS.to_owned());
    }
    Ok((CommitKind::Closure, paths))
}

fn record_failure(
    git: &Git,
    request: &CloseRequest,
    tracker: &dyn Tracker,
    evidence: &str,
) -> Result<(CommitKind, Vec<String>), String> {
    let bead = tracker.bead(&request.bead)?;
    if bead.status == "closed" {
        return Err(format!(
            "{} is closed; a failure is recorded on an open bead, which stays open (§11.4)",
            request.bead
        ));
    }
    if !git.root().join(evidence).is_file() {
        return Err(format!(
            "--evidence {evidence:?} is not a file in the workspace; write the failure-evidence \
             report first"
        ));
    }
    tracker.mark_failed(
        &request.bead,
        &format!(
            "FAILED (./dev commit --close --failed): evidence in {evidence}. The bead stays open, \
             labelled failed + needs-owner, so its dependants stay blocked until the owner's \
             recorded plan revision (§11.4, D-70)."
        ),
    )?;
    tracker.flush()?;
    Ok((
        CommitKind::Failure,
        vec![BEADS_EXPORT.to_owned(), evidence.to_owned()],
    ))
}

/// Writes requirement activation for closing `bead` and returns the activated IDs.
fn activate(
    root: &Path,
    tracker: &dyn Tracker,
    bead: &str,
    labels: &[String],
) -> Result<Vec<String>, String> {
    let (Some(items), Some(text)) = (
        registry::load_workgraph(root)?,
        registry::load_requirements_text(root)?,
    ) else {
        return Ok(Vec::new());
    };
    let Some(item) = registry::item_for_bead(&items, bead, labels) else {
        // A scoped subtask or discovered-work bead never activates anything.
        return Ok(Vec::new());
    };
    let requirements = registry::parse_requirements(&text)?;
    let beads = tracker.all_beads()?;
    let is_closed = |item_id: &str| {
        let Some(listed) = items.iter().find(|i| i.id == item_id) else {
            return false;
        };
        let plan_label = format!("plan:{item_id}");
        beads.iter().any(|b| {
            (b.id == listed.bead || b.labels.contains(&plan_label)) && b.status == "closed"
        })
    };
    let activated = registry::newly_active(&items, &requirements, &item.id, &is_closed);
    if !activated.is_empty() {
        let updated = registry::activate(&text, &activated)?;
        let path = root.join(registry::REQUIREMENTS);
        fs::write(&path, updated).map_err(|err| format!("{}: {err}", path.display()))?;
    }
    Ok(activated)
}

/// An accept receipt on disk.
struct AcceptReceipt {
    path: PathBuf,
    tree: String,
}

/// The newest passing accept receipt for `bead` under `artifacts/receipts/`.
fn latest_passing_receipt(artifacts: &Path, bead: &str) -> Result<Option<AcceptReceipt>, String> {
    let dir = artifacts.join("receipts");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(None);
    };
    let prefix = format!("accept-{bead}-");
    let mut best: Option<(String, AcceptReceipt)> = None;
    for entry in entries {
        let path = entry
            .map_err(|err| format!("{}: {err}", dir.display()))?
            .path();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let json = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("json"));
        if !name.starts_with(&prefix) || !json {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let passing =
            value["schema"] == accept::SCHEMA && value["ok"] == true && value["bead"] == bead;
        let (Some(tree), Some(finished)) = (value["tree"].as_str(), value["finished_at"].as_str())
        else {
            continue;
        };
        if passing && best.as_ref().is_none_or(|(at, _)| finished > at.as_str()) {
            best = Some((
                finished.to_owned(),
                AcceptReceipt {
                    path,
                    tree: tree.to_owned(),
                },
            ));
        }
    }
    Ok(best.map(|(_, receipt)| receipt))
}

/// The newest commit on `main` whose message carries `Bead: <bead>`, with its tree.
fn work_commit(git: &Git, bead: &str) -> Result<Option<(String, String)>, String> {
    let log = git.run(&["log", "--format=%H %T%n%B%x1e", commit::BRANCH])?;
    let trailer = format!("{}: {bead}", CommitKind::Work.trailer());
    Ok(log.split('\u{1e}').find_map(|record| {
        let mut lines = record.trim_start_matches('\n').lines();
        let (commit, tree) = lines.next()?.split_once(' ')?;
        lines
            .any(|line| line.trim() == trailer)
            .then(|| (commit.to_owned(), tree.to_owned()))
    }))
}

impl Report for CloseReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!(
            "{} {}: {}\n",
            self.mode,
            self.bead,
            if self.ok {
                "committed"
            } else {
                "NOT committed"
            }
        );
        if let (Some(commit), Some(tree)) = (&self.work_commit, &self.work_tree) {
            let _ = writeln!(out, "  work commit {commit} (tree {tree})");
        }
        if let Some(receipt) = &self.accept_receipt {
            let _ = writeln!(out, "  accept receipt: {receipt}");
        }
        if !self.activated.is_empty() {
            let _ = writeln!(out, "  activated: {}", self.activated.join(", "));
        }
        if let Some(commit) = &self.commit {
            let _ = writeln!(out, "  {}", commit.render_human().replace('\n', "\n  "));
        }
        if let Some(detail) = &self.detail {
            let _ = write!(out, "  {detail}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    // check: DEV-CLOSE
    use super::*;
    use crate::accept::tests::FakeTracker;
    use crate::accept::{SelectorResult, SelectorRunner};
    use crate::commit::{GateRun, Reservation};
    use crate::git::fixture;
    use crate::test_support::scratch_dir;

    /// Commit ports whose bead status mirrors the fake tracker; the gate always passes.
    struct FakePorts<'a> {
        tracker: &'a FakeTracker,
    }

    impl commit::Ports for FakePorts<'_> {
        fn bead_status(&self, bead: &str) -> Result<String, String> {
            Ok(self.tracker.status(bead))
        }
        fn reservations(&self, _agent: &str) -> Result<Vec<Reservation>, String> {
            Ok(vec![Reservation {
                pattern: "**".to_owned(),
                remaining_seconds: 600,
            }])
        }
        fn gate(&self, _dir: &Path, tree: &str, _changed: &[String]) -> Result<GateRun, String> {
            Ok(GateRun {
                passed: true,
                receipt: Some(format!("gate-{tree}")),
                summary: "ok".to_owned(),
            })
        }
        fn push(&self, _git: &Git) -> Vec<String> {
            Vec::new()
        }
    }

    struct PassAll;

    impl SelectorRunner for PassAll {
        fn run(&self, _dir: &Path, _target: &Path, args: &[String]) -> SelectorResult {
            SelectorResult {
                selector: String::new(),
                ok: true,
                checks: vec![args.join(" ")],
                detail: None,
            }
        }
    }

    const WORKGRAPH: &str = "\
[[item]]
id = \"W01\"
reqs = [\"P7\", \"O-8\"]
accept = [\"./dev check XTASK\"]
bead = \"\"

[[item]]
id = \"W49\"
reqs = [\"O-8\"]
accept = [\"./dev check STATUS\"]
bead = \"\"
";

    const REQUIREMENTS: &str = "\
[[requirement]]
id = \"O-8\"
level = \"MUST\"
status = \"planned\"
";

    /// A repository with a registry, a tracker export file and two work commits.
    struct World {
        git: Git,
        tracker: FakeTracker,
        artifacts: PathBuf,
    }

    fn world(name: &str) -> World {
        let git = fixture::repo(
            name,
            &[
                ("registry/workgraph.toml", WORKGRAPH),
                ("registry/requirements.toml", REQUIREMENTS),
                (BEADS_EXPORT, "{}\n"),
            ],
        );
        let tracker = FakeTracker::new(&[
            ("br-w01", "in_progress", &["m0", "plan:W01"]),
            (
                "br-w01-3",
                "in_progress",
                &["m0", "sub:W01", "accept:LINT-PROBES"],
            ),
            ("br-w49", "in_progress", &["m4", "plan:W49"]),
        ]);
        World {
            git,
            tracker,
            artifacts: scratch_dir(&format!("{name}-artifacts")),
        }
    }

    impl World {
        /// Commits a change carrying `Bead: <bead>` and returns its tree.
        fn work(&self, bead: &str, file: &str) -> String {
            fixture::write(self.git.root(), file, bead);
            self.git.stage(&[file.to_owned()]).unwrap();
            let message = format!("work\n\nBead: {bead}\n");
            self.git
                .run(&["commit", "--quiet", "-m", &message])
                .unwrap();
            self.git.rev_parse("HEAD^{tree}").unwrap()
        }

        fn accept(&self, bead: &str, tree: &str) {
            let report = accept::run(
                &self.git,
                &self.tracker,
                &PassAll,
                bead,
                tree,
                &self.artifacts,
            );
            assert!(report.ok, "accept: {report:#?}");
        }

        /// The tracker export changes when the fake tracker flushes, as br's would.
        fn close(&self, bead: &str) -> CloseReport {
            fixture::write(self.git.root(), BEADS_EXPORT, &format!("{bead} closed\n"));
            run(
                &self.git,
                &request(bead, None),
                &self.tracker,
                &FakePorts {
                    tracker: &self.tracker,
                },
                &self.artifacts,
            )
        }

        fn requirement_status(&self) -> String {
            let text = fs::read_to_string(self.git.root().join(registry::REQUIREMENTS)).unwrap();
            registry::parse_requirements(&text).unwrap()[0]
                .status
                .clone()
        }
    }

    fn request(bead: &str, failed_evidence: Option<&str>) -> CloseRequest {
        CloseRequest {
            bead: bead.to_owned(),
            agent: "TestAgent".to_owned(),
            message: format!("close {bead}"),
            failed_evidence: failed_evidence.map(str::to_owned),
        }
    }

    #[test]
    fn a_receipt_backed_closure_closes_flushes_and_commits_the_tracker_export() {
        let w = world("close-happy");
        let tree = w.work("br-w01-3", "probe.rs");
        w.accept("br-w01-3", &tree);
        let report = w.close("br-w01-3");
        assert!(report.ok, "report={report:#?}");
        assert_eq!(w.tracker.status("br-w01-3"), "closed");
        assert_eq!(
            *w.tracker.flushed.borrow(),
            1,
            "rule: br sync --flush-only runs"
        );
        assert_eq!(report.work_tree.as_deref(), Some(tree.as_str()));
        let message = w.git.run(&["log", "-1", "--format=%B"]).unwrap();
        assert!(message.contains("Closes: br-w01-3"), "message={message}");
        let committed = w
            .git
            .run(&["show", "--name-only", "--format=", "HEAD"])
            .unwrap();
        assert_eq!(
            committed.trim(),
            BEADS_EXPORT,
            "rule: the closure commit carries .beads/"
        );
    }

    #[test]
    fn a_missing_receipt_refuses_and_leaves_the_bead_open() {
        let w = world("close-missing");
        w.work("br-w01-3", "probe.rs");
        let report = w.close("br-w01-3");
        assert!(
            !report.ok
                && report
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("no passing accept receipt")),
            "report={report:#?}"
        );
        assert_eq!(
            w.tracker.status("br-w01-3"),
            "in_progress",
            "rule: nothing closes without a receipt"
        );
    }

    #[test]
    fn a_receipt_for_an_older_tree_is_the_wrong_tree() {
        let w = world("close-wrong-tree");
        let old = w.work("br-w01-3", "probe.rs");
        w.accept("br-w01-3", &old);
        w.work("br-w01-3", "probe2.rs");
        let report = w.close("br-w01-3");
        assert!(
            !report.ok
                && report
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("wrong tree")),
            "rule: the receipt must name the tree of the commit that carried the work; \
             report={report:#?}"
        );
        assert_eq!(w.tracker.status("br-w01-3"), "in_progress");
    }

    #[test]
    fn a_child_closes_before_its_parent_and_only_the_last_canonical_parent_activates() {
        let w = world("close-activation");
        // The subtask closes first: nothing activates.
        let sub = w.work("br-w01-3", "probe.rs");
        w.accept("br-w01-3", &sub);
        let child = w.close("br-w01-3");
        assert!(child.ok && child.activated.is_empty(), "child={child:#?}");
        assert_eq!(w.requirement_status(), "planned");
        // W01 closes while W49, which also lists O-8, is open: still planned.
        let w01 = w.work("br-w01", "w01.rs");
        w.accept("br-w01", &w01);
        let first = w.close("br-w01");
        assert!(first.ok && first.activated.is_empty(), "first={first:#?}");
        assert_eq!(w.requirement_status(), "planned");
        // The last listing parent closes: O-8 activates, in the same closure commit.
        let w49 = w.work("br-w49", "w49.rs");
        w.accept("br-w49", &w49);
        let last = w.close("br-w49");
        assert_eq!(last.activated, ["O-8"], "last={last:#?}");
        assert_eq!(w.requirement_status(), "active");
        let committed = w
            .git
            .run(&["show", "--name-only", "--format=", "HEAD"])
            .unwrap();
        assert!(
            committed.contains(registry::REQUIREMENTS) && committed.contains(BEADS_EXPORT),
            "rule: the activation commits with .beads/ in one closure commit; files={committed}"
        );
    }

    #[test]
    fn a_failed_spike_stays_open_with_failed_and_needs_owner() {
        let w = world("close-failed");
        fixture::write(
            w.git.root(),
            "reports/w01-failure.md",
            "# W01 spike failed\n",
        );
        fixture::write(w.git.root(), BEADS_EXPORT, "br-w01 failed\n");
        let report = run(
            &w.git,
            &request("br-w01", Some("reports/w01-failure.md")),
            &w.tracker,
            &FakePorts {
                tracker: &w.tracker,
            },
            &w.artifacts,
        );
        assert!(report.ok, "report={report:#?}");
        let bead = w.tracker.bead("br-w01").unwrap();
        assert_eq!(
            bead.status, "open",
            "rule: a failed spike is never closed (§11.4, D-70)"
        );
        assert!(
            bead.labels.contains(&"failed".to_owned())
                && bead.labels.contains(&"needs-owner".to_owned())
        );
        let committed = w
            .git
            .run(&["show", "--name-only", "--format=%B", "HEAD"])
            .unwrap();
        assert!(
            committed.contains("Failed: br-w01") && committed.contains("reports/w01-failure.md"),
            "rule: the evidence report is committed with .beads/; commit={committed}"
        );
    }

    #[test]
    fn failure_needs_evidence_and_an_open_bead() {
        let w = world("close-failed-refusals");
        let no_file = run(
            &w.git,
            &request("br-w01", Some("reports/missing.md")),
            &w.tracker,
            &FakePorts {
                tracker: &w.tracker,
            },
            &w.artifacts,
        );
        assert!(
            !no_file.ok
                && no_file
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("not a file"))
        );
        w.tracker.close("br-w49", "done").unwrap();
        fixture::write(w.git.root(), "reports/x.md", "x\n");
        let closed = run(
            &w.git,
            &request("br-w49", Some("reports/x.md")),
            &w.tracker,
            &FakePorts {
                tracker: &w.tracker,
            },
            &w.artifacts,
        );
        assert!(
            !closed.ok
                && closed
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains("is closed"))
        );
    }
}
