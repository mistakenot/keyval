//! `./dev commit`: the exact-candidate commit wrapper (PLAN.md §11.10, P11).
//!
//! It serialises the commit boundary, not the coding:
//!
//! 1. Take an exclusive lock (`.local/commit.lock`). Check that HEAD is on `main`, that
//!    no git operation is in progress, that the bead is in progress, and that every path
//!    lies inside the agent's live Agent Mail reservation. Refuse if anything foreign is
//!    already staged.
//! 2. Stage only the agent's named paths, and record the parent commit and the candidate
//!    tree ID.
//! 3. Refresh the fixed `.local/candidate/` directory in place to hold exactly that tree,
//!    and verify its manifest ([`crate::candidate`]).
//! 4. Run the T0 gate there, git-isolated, in candidate mode (`--tree`, `--changed`).
//! 5. Re-check the parent, the index tree and the candidate. If anything changed, run
//!    again on the new candidate (up to [`MAX_ATTEMPTS`]); never commit a different tree
//!    because a gate already passed.
//! 6. Commit exactly the tested tree with `git commit-tree`, and move `main` with a
//!    compare-and-swap on the recorded parent. Push `main` and the `master` mirror
//!    (§17 step 2). Write a commit receipt and release the lock.
//!
//! A failed gate unstages the agent's paths and leaves every working file alone, the
//! agent's and everybody else's. Nothing here creates a branch or a worktree.

use std::fmt::Write as _;
use std::fs::{self, File, TryLockError};
use std::io::{self, Write as _};
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

/// Schema identifier of the `--json` report and of the commit receipt file.
pub const SCHEMA: &str = "keyval.dev.commit/v1";

/// Gate runs per commit before giving up on a moving checkout.
pub const MAX_ATTEMPTS: u32 = 3;

/// The only branch commits land on (D-16).
pub const BRANCH: &str = "main";

/// The fixed candidate directory, relative to the root (§11.10 step 3).
pub const CANDIDATE_DIR: &str = ".local/candidate";

/// The commit lock file, relative to the root.
pub const LOCK_FILE: &str = ".local/commit.lock";

/// An Agent Mail file reservation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reservation {
    /// Glob pattern, e.g. `xtask/**`.
    pub pattern: String,
    /// Seconds until it expires.
    pub remaining_seconds: u64,
}

/// The outcome of one gate run on the candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateRun {
    /// Whether the gate passed.
    pub passed: bool,
    /// Path of the gate receipt, if one was written.
    pub receipt: Option<String>,
    /// One line on the result.
    pub summary: String,
}

/// The outside world the wrapper depends on; tests substitute fakes.
pub trait Ports {
    /// The bead's tracker status, e.g. `in_progress`.
    fn bead_status(&self, bead: &str) -> Result<String, String>;
    /// The agent's live reservations.
    fn reservations(&self, agent: &str) -> Result<Vec<Reservation>, String>;
    /// Runs the T0 gate on the candidate directory holding `tree`.
    fn gate(&self, candidate: &Path, tree: &str, changed: &[String]) -> Result<GateRun, String>;
    /// Pushes after a commit; one line per refspec.
    fn push(&self, git: &Git) -> Vec<String>;
}

/// What the agent asked to commit.
#[derive(Clone, Debug)]
pub struct Request {
    /// What sort of commit this is.
    pub kind: CommitKind,
    /// The bead the commit belongs to.
    pub bead: String,
    /// The Agent Mail identity whose reservations cover the paths.
    pub agent: String,
    /// The commit message.
    pub message: String,
    /// Paths (files or directories) to commit, relative to the root.
    pub paths: Vec<String>,
}

/// What a commit carries, which decides the bead status it requires and its trailer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitKind {
    /// The bead's work. The bead must be `in_progress`; trailer `Bead: <id>`, which is
    /// how closure finds the commit that carried the work (§11.3 step 7).
    Work,
    /// A closure (`./dev commit --close`): `.beads/` plus registry flips. The bead must
    /// be the just-closed one; trailer `Closes: <id>`.
    Closure,
    /// A failure record (`--close --failed`): `.beads/` plus the evidence report. The bead
    /// must still be open; trailer `Failed: <id>`.
    Failure,
}

impl CommitKind {
    /// The bead statuses this kind of commit accepts.
    const fn statuses(self) -> &'static [&'static str] {
        match self {
            Self::Work => &["in_progress"],
            Self::Closure => &["closed"],
            Self::Failure => &["open", "in_progress"],
        }
    }

    /// The trailer key naming the bead.
    pub const fn trailer(self) -> &'static str {
        match self {
            Self::Work => "Bead",
            Self::Closure => "Closes",
            Self::Failure => "Failed",
        }
    }
}

/// How a commit attempt ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The tested tree was committed.
    Committed,
    /// A precondition failed; nothing was committed.
    Refused,
    /// The gate failed; the paths were unstaged and nothing was committed.
    GateFailed,
}

/// The `./dev commit` report, also written as the commit receipt.
#[derive(Debug, Serialize)]
pub struct CommitReport {
    /// Schema identifier.
    pub schema: &'static str,
    /// True only when committed.
    pub ok: bool,
    /// How it ended.
    pub outcome: Outcome,
    /// The bead.
    pub bead: String,
    /// The agent.
    pub agent: String,
    /// The requested paths.
    pub paths: Vec<String>,
    /// The staged changes the commit carries.
    pub changed: Vec<String>,
    /// The parent commit.
    pub parent: Option<String>,
    /// The candidate tree that was gated.
    pub tree: Option<String>,
    /// The new commit.
    pub commit: Option<String>,
    /// Gate runs made (more than one when the checkout moved).
    pub attempts: u32,
    /// The gate receipt of the run that decided the outcome.
    pub gate_receipt: Option<String>,
    /// One line per push refspec.
    pub push: Vec<String>,
    /// Path of this report's receipt copy.
    pub receipt_path: Option<String>,
    /// Why it was refused or failed.
    pub detail: Option<String>,
    /// Time of the decision (UTC).
    pub at: String,
}

impl CommitReport {
    fn new(request: &Request) -> Self {
        Self {
            schema: SCHEMA,
            ok: false,
            outcome: Outcome::Refused,
            bead: request.bead.clone(),
            agent: request.agent.clone(),
            paths: request.paths.clone(),
            changed: Vec::new(),
            parent: None,
            tree: None,
            commit: None,
            attempts: 0,
            gate_receipt: None,
            push: Vec::new(),
            receipt_path: None,
            detail: None,
            at: Utc::now().rfc3339(),
        }
    }

    fn refuse(mut self, why: impl Into<String>) -> Self {
        self.outcome = Outcome::Refused;
        self.detail = Some(why.into());
        self
    }
}

/// Runs the commit protocol for `request` in the repository `git`, writing the receipt
/// under `artifacts/receipts/`.
pub fn run(git: &Git, request: &Request, ports: &dyn Ports, artifacts: &Path) -> CommitReport {
    let mut report = commit(git, request, ports);
    let receipts = artifacts.join("receipts");
    let name = format!(
        "commit-{}-{}-{}.json",
        Utc::now().compact(),
        report
            .tree
            .as_deref()
            .and_then(|t| t.get(..12))
            .unwrap_or("none"),
        std::process::id()
    );
    let path = receipts.join(name);
    report.receipt_path = Some(path.display().to_string());
    let written = fs::create_dir_all(&receipts)
        .map_err(|err| err.to_string())
        .and_then(|()| {
            serde_json::to_string_pretty(&report)
                .map_err(|err| err.to_string())
                .and_then(|json| fs::write(&path, json + "\n").map_err(|err| err.to_string()))
        });
    if let Err(why) = written {
        report.receipt_path = None;
        let note = format!("cannot write the commit receipt: {why}");
        report.detail = Some(
            report
                .detail
                .map_or_else(|| note.clone(), |d| format!("{d}; {note}")),
        );
    }
    report
}

fn commit(git: &Git, request: &Request, ports: &dyn Ports) -> CommitReport {
    let report = CommitReport::new(request);
    let _lock = match acquire_lock(&git.root().join(LOCK_FILE)) {
        Ok(lock) => lock,
        Err(why) => return report.refuse(why),
    };
    let paths = match normalise_paths(&request.paths) {
        Ok(paths) => paths,
        Err(why) => return report.refuse(why),
    };
    if let Err(why) = preconditions(git, request, ports) {
        return report.refuse(why);
    }
    let reservations = match ports.reservations(&request.agent) {
        Ok(found) => found,
        Err(why) => return report.refuse(format!("cannot read Agent Mail reservations: {why}")),
    };
    match foreign_staged(git, &paths) {
        Ok(foreign) if foreign.is_empty() => {}
        Ok(foreign) => {
            return report.refuse(format!(
                "paths outside this commit are already staged: {foreign:?}; the wrapper \
                 never commits another agent's staging (§11.10 step 1)"
            ));
        }
        Err(why) => return report.refuse(why),
    }
    if let Err(why) = git.stage(&paths) {
        return report.refuse(format!("cannot stage {paths:?}: {why}"));
    }
    let outcome = gated_commit(git, request, ports, &paths, &reservations, report);
    if outcome.outcome != Outcome::Committed {
        // Give the index back. If it still holds exactly our last snapshot, nobody else
        // staged, so reset it to HEAD: that also drops the stale view a re-run left when
        // HEAD moved. Otherwise unstage only our paths and leave theirs alone. Working
        // files are never touched.
        let ours = outcome
            .tree
            .as_ref()
            .is_some_and(|tree| git.write_tree().ok().as_ref() == Some(tree));
        let _ = if ours {
            git.run(&["read-tree", "HEAD"]).map(drop)
        } else {
            git.unstage(&paths)
        };
    }
    outcome
}

fn preconditions(git: &Git, request: &Request, ports: &dyn Ports) -> Result<(), String> {
    match git.branch() {
        Some(branch) if branch == BRANCH => {}
        other => return Err(format!("HEAD must be on {BRANCH}, found {other:?}")),
    }
    if let Some(what) = git.operation_in_progress()? {
        return Err(format!("{what}; finish it first"));
    }
    let status = ports.bead_status(&request.bead)?;
    let allowed = request.kind.statuses();
    if !allowed.contains(&status.as_str()) {
        let hint = match request.kind {
            CommitKind::Work => "claim it (in_progress) before committing its work",
            CommitKind::Closure => "a closure commit carries a bead that was just closed",
            CommitKind::Failure => "a failure record leaves the bead open",
        };
        return Err(format!(
            "bead {} is {status}; {hint} ({:?} commits need {allowed:?})",
            request.bead, request.kind
        ));
    }
    Ok(())
}

fn gated_commit(
    git: &Git,
    request: &Request,
    ports: &dyn Ports,
    paths: &[String],
    reservations: &[Reservation],
    mut report: CommitReport,
) -> CommitReport {
    let candidate_dir = git.root().join(CANDIDATE_DIR);
    let mut previous: Option<String> = None;
    while report.attempts < MAX_ATTEMPTS {
        report.attempts += 1;
        if let Some(tree) = &previous
            && let Err(why) = rebase_index(git, tree, paths)
        {
            return report.refuse(why);
        }
        let snapshot = match snapshot(git, paths, reservations) {
            Ok(snapshot) => snapshot,
            Err(why) => return report.refuse(why),
        };
        report.changed.clone_from(&snapshot.changed);
        report.parent = Some(snapshot.parent.clone());
        report.tree = Some(snapshot.tree.clone());
        previous = Some(snapshot.tree.clone());
        let prepared = candidate::materialise(git, &snapshot.tree, &candidate_dir)
            .and_then(|_| candidate::verify(git, &snapshot.tree, &candidate_dir));
        if let Err(why) = prepared {
            return report.refuse(format!("candidate preparation failed: {why}"));
        }
        let gate = match ports.gate(&candidate_dir, &snapshot.tree, &snapshot.changed) {
            Ok(gate) => gate,
            Err(why) => return report.refuse(format!("the gate could not run: {why}")),
        };
        report.gate_receipt.clone_from(&gate.receipt);
        if let Err(why) = candidate::verify(git, &snapshot.tree, &candidate_dir) {
            return report.refuse(format!(
                "the gate run modified the candidate; tests must not write into the source \
                 tree: {why}"
            ));
        }
        if !gate.passed {
            report.outcome = Outcome::GateFailed;
            report.detail = Some(gate.summary);
            return report;
        }
        // Step 5: commit only if nothing moved while the gate ran.
        match snapshot_unchanged(git, &snapshot) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(why) => return report.refuse(why),
        }
        let message = format!(
            "{}\n\n{}: {}\nAgent: {}\nTree: {}\nGate-Receipt: {}\n",
            request.message.trim_end(),
            request.kind.trailer(),
            request.bead,
            request.agent,
            snapshot.tree,
            gate.receipt.as_deref().unwrap_or("none")
        );
        let commit = match git.commit_tree(&snapshot.tree, &snapshot.parent, &message) {
            Ok(commit) => commit,
            Err(why) => return report.refuse(why),
        };
        if git
            .advance_branch(BRANCH, &commit, &snapshot.parent)
            .is_err()
        {
            // HEAD moved between the re-check and the update: run again.
            continue;
        }
        report.commit = Some(commit);
        report.outcome = Outcome::Committed;
        report.ok = true;
        report.detail = None;
        report.push = ports.push(git);
        return report;
    }
    report.refuse(format!(
        "the checkout kept moving during the gate ({MAX_ATTEMPTS} attempts); nothing committed"
    ))
}

/// Prepares a re-run after the checkout moved during the gate (§11.10 step 5).
///
/// If the index still holds exactly the tree we staged (`staged`), only HEAD moved: the
/// index is reset to the new HEAD (index only, working files untouched) and our paths
/// are staged again, so the new candidate is the new HEAD plus our changes and never
/// reverts the concurrent commit. If anything else changed the index, someone staged
/// during the gate: refuse, and leave their staging alone.
fn rebase_index(git: &Git, staged: &str, paths: &[String]) -> Result<(), String> {
    if git.write_tree()? != staged {
        let touched = git.run(&["diff-index", "--cached", "--name-only", staged])?;
        let touched: Vec<&str> = touched.lines().collect();
        return Err(format!(
            "the index changed while the gate ran ({touched:?}); the wrapper never commits or \
             discards another agent's staging (§11.10 step 1)"
        ));
    }
    git.run(&["read-tree", "HEAD"])?;
    git.stage(paths)
}

/// The parent, the staged tree and the changes it carries, at one moment.
struct Snapshot {
    parent: String,
    tree: String,
    changed: Vec<String>,
}

fn snapshot(git: &Git, paths: &[String], reservations: &[Reservation]) -> Result<Snapshot, String> {
    let changed = git.staged_paths()?;
    if changed.is_empty() {
        return Err(format!("nothing to commit: {paths:?} have no changes"));
    }
    let foreign: Vec<&String> = changed.iter().filter(|c| !covered(c, paths)).collect();
    if !foreign.is_empty() {
        return Err(format!("unexpected staged paths: {foreign:?}"));
    }
    let unreserved: Vec<&String> = changed
        .iter()
        .filter(|c| !reserved(c, reservations))
        .collect();
    if !unreserved.is_empty() {
        return Err(format!(
            "not inside a live Agent Mail reservation of this agent: {unreserved:?}; \
             reserve them first (§11.5 rule 8)"
        ));
    }
    Ok(Snapshot {
        parent: git.rev_parse("HEAD")?,
        tree: git.write_tree()?,
        changed,
    })
}

fn snapshot_unchanged(git: &Git, snapshot: &Snapshot) -> Result<bool, String> {
    Ok(git.branch().as_deref() == Some(BRANCH)
        && git.operation_in_progress()?.is_none()
        && git.rev_parse("HEAD")? == snapshot.parent
        && git.write_tree()? == snapshot.tree)
}

fn foreign_staged(git: &Git, paths: &[String]) -> Result<Vec<String>, String> {
    Ok(git
        .staged_paths()?
        .into_iter()
        .filter(|staged| !covered(staged, paths))
        .collect())
}

/// Whether `file` is one of `paths` or lies under one of them.
fn covered(file: &str, paths: &[String]) -> bool {
    paths.iter().any(|path| {
        file == path
            || file
                .strip_prefix(path.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn reserved(file: &str, reservations: &[Reservation]) -> bool {
    reservations
        .iter()
        .any(|r| r.remaining_seconds > 0 && (glob_match(&r.pattern, file) || r.pattern == file))
}

/// Shell-style glob as Agent Mail uses it (Python `fnmatchcase`): `*` matches any run
/// of characters including `/`, `?` matches one character.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some(&c) if c == '?' || Some(&c) == text.get(t) => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((star_p, star_t)) => {
                    p = star_p + 1;
                    t = star_t + 1;
                    star = Some((star_p, star_t + 1));
                }
                None => return false,
            },
        }
    }
    pattern.iter().skip(p).all(|&c| c == '*')
}

fn normalise_paths(paths: &[String]) -> Result<Vec<String>, String> {
    if paths.is_empty() {
        return Err("name the paths to commit".to_owned());
    }
    paths
        .iter()
        .map(|path| {
            let trimmed = path.trim_start_matches("./").trim_end_matches('/');
            let bad = trimmed.is_empty()
                || Path::new(trimmed).is_absolute()
                || trimmed.split('/').any(|part| part == ".." || part == ".")
                || trimmed.starts_with(".git/")
                || trimmed == ".git";
            if bad {
                Err(format!("{path:?} is not a repository-relative path"))
            } else {
                Ok(trimmed.to_owned())
            }
        })
        .collect()
}

/// Holds the commit lock until dropped.
struct CommitLock {
    _file: File,
}

fn acquire_lock(path: &Path) -> Result<CommitLock, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("{}: {err}", parent.display()))?;
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(CommitLock { _file: file }),
        Err(TryLockError::WouldBlock) => {
            let _ = writeln!(
                io::stderr().lock(),
                "waiting for the commit lock ({}): another commit is in progress",
                path.display()
            );
            file.lock()
                .map(|()| CommitLock { _file: file })
                .map_err(|err| format!("commit lock: {err}"))
        }
        Err(TryLockError::Error(err)) => Err(format!("commit lock: {err}")),
    }
}

/// The real ports: `br`, `am`, the gate in a git-isolated subprocess, and `git push`.
pub struct LivePorts {
    /// The shared checkout's root.
    pub root: PathBuf,
    /// Where gate receipts go.
    pub artifacts: PathBuf,
}

const TOOL_LIMIT: Duration = Duration::from_mins(2);
const GATE_LIMIT: Duration = Duration::from_mins(60);

impl Ports for LivePorts {
    fn bead_status(&self, bead: &str) -> Result<String, String> {
        let mut cmd = Command::new("br");
        cmd.args(["show", bead, "--json"]).current_dir(&self.root);
        let captured = process::run(&mut cmd, TOOL_LIMIT).map_err(|err| format!("br: {err}"))?;
        let value: serde_json::Value = serde_json::from_str(&captured.stdout)
            .map_err(|err| format!("br show {bead} --json: {err}"))?;
        value[0]["status"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("no bead {bead}"))
    }

    fn reservations(&self, agent: &str) -> Result<Vec<Reservation>, String> {
        let project = self.root.display().to_string();
        let mut cmd = Command::new("am");
        cmd.args([
            "reservations",
            "--project",
            &project,
            "--agent",
            agent,
            "--json",
        ])
        .current_dir(&self.root);
        let captured = process::run(&mut cmd, TOOL_LIMIT).map_err(|err| format!("am: {err}"))?;
        let value: serde_json::Value = serde_json::from_str(&captured.stdout)
            .map_err(|err| format!("am reservations --json: {err}"))?;
        let mine = value["my_reservations"]
            .as_array()
            .ok_or("am reservations --json: no my_reservations")?;
        Ok(mine
            .iter()
            .filter(|r| r["agent"] == agent)
            .filter_map(|r| {
                Some(Reservation {
                    pattern: r["path"].as_str()?.to_owned(),
                    remaining_seconds: r["remaining_seconds"].as_u64()?,
                })
            })
            .collect())
    }

    fn gate(&self, candidate: &Path, tree: &str, changed: &[String]) -> Result<GateRun, String> {
        let xtask_self = std::env::current_exe().map_err(|err| err.to_string())?;
        let mut cmd = gate_command(
            &xtask_self,
            &self.root,
            &self.artifacts,
            candidate,
            tree,
            changed,
        )?;
        let captured = process::run(&mut cmd, GATE_LIMIT).map_err(|err| format!("gate: {err}"))?;
        // The gate's step records arrive on stderr; pass them through as progress.
        let _ = io::stderr().lock().write_all(captured.stderr.as_bytes());
        let value: serde_json::Value = serde_json::from_str(&captured.stdout).map_err(|err| {
            format!(
                "gate output is not JSON ({err}): {}",
                captured.stdout.trim()
            )
        })?;
        Ok(GateRun {
            passed: value["ok"] == true,
            receipt: value["receipt_path"].as_str().map(str::to_owned),
            summary: value["error"]["message"].as_str().map_or_else(
                || format!("gate result: {}", value["receipt"]["result"]),
                str::to_owned,
            ),
        })
    }

    fn push(&self, git: &Git) -> Vec<String> {
        if git.run(&["remote", "get-url", "origin"]).is_err() {
            return vec!["no remote `origin`; not pushed".to_owned()];
        }
        ["main", "main:master"]
            .iter()
            .map(
                |refspec| match git.run(&["push", "--quiet", "origin", refspec]) {
                    Ok(_) => format!("pushed origin {refspec}"),
                    Err(why) => format!("push origin {refspec} FAILED: {why}"),
                },
            )
            .collect()
    }
}

/// The `xtask gate fast` command that judges `candidate` in candidate mode, git-isolated
/// (§11.10 step 4) and building into the candidate's own target directory.
fn gate_command(
    xtask: &Path,
    root: &Path,
    artifacts: &Path,
    candidate: &Path,
    tree: &str,
    changed: &[String],
) -> Result<Command, String> {
    let mut cmd = Command::new(xtask);
    cmd.args(["--json", "gate", "fast", "--tree", tree, "--artifacts"])
        .arg(artifacts);
    for path in changed {
        cmd.args(["--changed", path]);
    }
    cmd.current_dir(candidate).env("KEYVAL_ROOT", candidate);
    // Git isolation (§11.10 step 4): nothing in the candidate can see the checkout.
    for (key, value) in candidate::git_isolation(candidate) {
        cmd.env(key, value);
    }
    // The candidate builds into its own target dir, never the checkout's or a shared one,
    // so the gate's tests are the candidate's own build (br-gate-shared-target-dir-dtqu).
    // Under the shared base, it also stays out of the candidate, whose manifest must stay
    // exact.
    let base = cargo::target_base(root);
    cmd.env(cargo::TARGET_BASE_ENV, &base).env(
        cargo::TARGET_DIR_ENV,
        cargo::checkout_target_dir(&base, candidate)?,
    );
    Ok(cmd)
}

impl Report for CommitReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        let mut out = match self.outcome {
            Outcome::Committed => format!(
                "committed {} (tree {}) for {} after {} gate run(s)\n",
                self.commit.as_deref().unwrap_or("?"),
                self.tree.as_deref().unwrap_or("?"),
                self.bead,
                self.attempts
            ),
            Outcome::Refused => format!(
                "commit REFUSED for {}: {}\n",
                self.bead,
                self.detail.as_deref().unwrap_or("")
            ),
            Outcome::GateFailed => format!(
                "commit NOT made for {}: the gate failed ({}); your paths were unstaged and \
                 your files are untouched\n",
                self.bead,
                self.detail.as_deref().unwrap_or("")
            ),
        };
        for line in &self.push {
            let _ = writeln!(out, "  {line}");
        }
        if let Some(gate) = &self.gate_receipt {
            let _ = writeln!(out, "  gate receipt: {gate}");
        }
        if let Some(receipt) = &self.receipt_path {
            let _ = write!(out, "  commit receipt: {receipt}");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    // check: DEV-COMMIT
    use super::*;
    use crate::git::fixture;
    use crate::test_support::scratch_dir;
    use std::cell::RefCell;

    /// Fake ports: an in-progress bead, reservations over everything, and a gate that
    /// runs `during_gate` (to simulate concurrent activity) and then returns `passed`.
    struct Fake<'a> {
        status: &'a str,
        reservations: Vec<Reservation>,
        passed: bool,
        during_gate: RefCell<Box<dyn FnMut() + 'a>>,
        gate_trees: RefCell<Vec<String>>,
    }

    impl Fake<'_> {
        fn new() -> Self {
            Self {
                status: "in_progress",
                reservations: vec![Reservation {
                    pattern: "**".to_owned(),
                    remaining_seconds: 600,
                }],
                passed: true,
                during_gate: RefCell::new(Box::new(|| {})),
                gate_trees: RefCell::new(Vec::new()),
            }
        }
    }

    impl Ports for Fake<'_> {
        fn bead_status(&self, _bead: &str) -> Result<String, String> {
            Ok(self.status.to_owned())
        }
        fn reservations(&self, _agent: &str) -> Result<Vec<Reservation>, String> {
            Ok(self.reservations.clone())
        }
        fn gate(&self, _dir: &Path, tree: &str, _changed: &[String]) -> Result<GateRun, String> {
            self.gate_trees.borrow_mut().push(tree.to_owned());
            (self.during_gate.borrow_mut())();
            Ok(GateRun {
                passed: self.passed,
                receipt: Some(format!("receipt-for-{tree}")),
                summary: if self.passed { "ok" } else { "fmt failed" }.to_owned(),
            })
        }
        fn push(&self, _git: &Git) -> Vec<String> {
            vec!["push skipped (test)".to_owned()]
        }
    }

    fn request(paths: &[&str]) -> Request {
        Request {
            kind: CommitKind::Work,
            bead: "br-test".to_owned(),
            agent: "TestAgent".to_owned(),
            message: "test commit".to_owned(),
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    fn repo(name: &str) -> Git {
        fixture::repo(
            name,
            &[("a.rs", "a\n"), ("b.rs", "b\n"), ("dir/c.rs", "c\n")],
        )
    }

    fn commit_in(git: &Git, request: &Request, ports: &dyn Ports) -> CommitReport {
        run(git, request, ports, &scratch_dir("commit-artifacts"))
    }

    #[test]
    fn the_candidate_gate_builds_into_the_candidates_own_target_dir() {
        let root = scratch_dir("commit-gate-cmd");
        let candidate = root.join(CANDIDATE_DIR);
        fs::create_dir_all(&candidate).unwrap();
        let cmd = gate_command(
            Path::new("/bin/xtask"),
            &root,
            &root.join("artifacts"),
            &candidate,
            "0123",
            &["a.rs".to_owned()],
        )
        .unwrap();
        let env = |key: &str| {
            cmd.get_envs()
                .find(|(name, _)| *name == key)
                .and_then(|(_, value)| value)
                .map(PathBuf::from)
        };
        let base = cargo::target_base(&root);
        assert_eq!(
            env(cargo::TARGET_DIR_ENV),
            Some(cargo::checkout_target_dir(&base, &candidate).unwrap()),
            "rule: the candidate's gate never shares the checkout's or a shared target \
             dir (br-gate-shared-target-dir-dtqu); cmd={cmd:?}"
        );
        assert_ne!(
            env(cargo::TARGET_DIR_ENV),
            Some(cargo::checkout_target_dir(&base, &root).unwrap()),
            "cmd={cmd:?}"
        );
        assert_eq!(env(cargo::TARGET_BASE_ENV), Some(base), "cmd={cmd:?}");
        assert_eq!(env("KEYVAL_ROOT").as_deref(), Some(candidate.as_path()));
        for (key, value) in candidate::git_isolation(&candidate) {
            assert_eq!(
                env(key),
                Some(PathBuf::from(value)),
                "rule: the candidate gate runs git-isolated (§11.10 step 4); cmd={cmd:?}"
            );
        }
    }

    #[test]
    fn commits_exactly_the_gated_tree_with_only_the_named_paths() {
        let git = repo("commit-happy");
        let parent = git.rev_parse("HEAD").unwrap();
        fixture::write(git.root(), "a.rs", "a2\n");
        fixture::write(git.root(), "b.rs", "someone else's edit\n");
        let fake = Fake::new();
        let report = commit_in(&git, &request(&["a.rs"]), &fake);
        assert_eq!(report.outcome, Outcome::Committed, "report={report:#?}");
        let head_tree = git.rev_parse("HEAD^{tree}").unwrap();
        assert_eq!(report.tree.as_deref(), Some(head_tree.as_str()));
        assert_eq!(
            fake.gate_trees.borrow().as_slice(),
            std::slice::from_ref(&head_tree)
        );
        assert_eq!(git.rev_parse("HEAD~1").unwrap(), parent);
        assert_eq!(report.changed, ["a.rs"]);
        assert_eq!(
            git.run(&["show", "HEAD:b.rs"]).unwrap(),
            "b\n",
            "rule: another agent's unstaged edit is never committed"
        );
        assert_eq!(
            fs::read_to_string(git.root().join("b.rs")).unwrap(),
            "someone else's edit\n"
        );
        let message = git.run(&["log", "-1", "--format=%B"]).unwrap();
        assert!(
            message.contains("Bead: br-test") && message.contains(&format!("Tree: {head_tree}")),
            "message={message}"
        );
        let receipt: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(report.receipt_path.as_ref().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["schema"], SCHEMA);
        assert_eq!(receipt["commit"], git.rev_parse("HEAD").unwrap());
        assert_eq!(receipt["tree"], head_tree);
    }

    #[test]
    fn the_candidate_holds_exactly_the_committed_tree() {
        let git = repo("commit-candidate");
        fixture::write(git.root(), "dir/c.rs", "c2\n");
        let report = commit_in(&git, &request(&["dir"]), &Fake::new());
        assert_eq!(report.outcome, Outcome::Committed, "report={report:#?}");
        let tree = report.tree.unwrap();
        assert_eq!(
            candidate::verify(&git, &tree, &git.root().join(CANDIDATE_DIR)),
            Ok(3)
        );
    }

    /// Puts a repository or the fake ports into a refusing state.
    type Setup = Box<dyn Fn(&Git, &mut Fake<'_>)>;

    #[test]
    fn preconditions_refuse_without_touching_the_index() {
        let cases: [(&str, Setup, &str); 5] = [
            ("bead", Box::new(|_, f| f.status = "open"), "claim it"),
            (
                "reservation",
                Box::new(|_, f| {
                    f.reservations = vec![Reservation {
                        pattern: "dir/**".to_owned(),
                        remaining_seconds: 600,
                    }];
                }),
                "not inside a live Agent Mail reservation",
            ),
            (
                "expired",
                Box::new(|_, f| {
                    f.reservations = vec![Reservation {
                        pattern: "**".to_owned(),
                        remaining_seconds: 0,
                    }];
                }),
                "not inside a live Agent Mail reservation",
            ),
            (
                "branch",
                Box::new(|g, _| {
                    g.run(&["switch", "--quiet", "--detach"]).unwrap();
                }),
                "HEAD must be on main",
            ),
            (
                "foreign",
                Box::new(|g, _| {
                    fixture::write(g.root(), "b.rs", "b2\n");
                    g.stage(&["b.rs".to_owned()]).unwrap();
                }),
                "already staged",
            ),
        ];
        for (name, setup, expected) in cases {
            let git = repo(&format!("commit-refuse-{name}"));
            let head = git.rev_parse("HEAD").unwrap();
            fixture::write(git.root(), "a.rs", "a2\n");
            let mut fake = Fake::new();
            setup(&git, &mut fake);
            let staged_before = git.staged_paths().unwrap();
            let report = commit_in(&git, &request(&["a.rs"]), &fake);
            assert_eq!(
                report.outcome,
                Outcome::Refused,
                "case={name} report={report:#?}"
            );
            assert!(
                report
                    .detail
                    .as_deref()
                    .is_some_and(|d| d.contains(expected)),
                "case={name} expected {expected:?}; report={report:#?}"
            );
            assert_eq!(
                git.rev_parse("HEAD").unwrap(),
                head,
                "case={name}: no commit"
            );
            assert_eq!(
                git.staged_paths().unwrap(),
                staged_before,
                "case={name}: rule: a refusal leaves the index as it was"
            );
            assert_eq!(fs::read_to_string(git.root().join("a.rs")).unwrap(), "a2\n");
        }
    }

    #[test]
    fn a_failed_gate_commits_nothing_and_unstages_only_our_paths() {
        let git = repo("commit-gate-fail");
        let head = git.rev_parse("HEAD").unwrap();
        fixture::write(git.root(), "a.rs", "a2\n");
        let mut fake = Fake::new();
        fake.passed = false;
        let report = commit_in(&git, &request(&["a.rs"]), &fake);
        assert_eq!(report.outcome, Outcome::GateFailed, "report={report:#?}");
        assert_eq!(git.rev_parse("HEAD").unwrap(), head);
        assert_eq!(git.staged_paths().unwrap(), Vec::<String>::new());
        assert_eq!(
            fs::read_to_string(git.root().join("a.rs")).unwrap(),
            "a2\n",
            "rule: a failed gate preserves the agent's work (§11.10)"
        );
        assert!(
            report.gate_receipt.is_some(),
            "rule: the failing evidence is kept"
        );
    }

    #[test]
    fn invalid_paths_and_empty_changes_are_refused() {
        let git = repo("commit-paths");
        for (paths, expected) in [
            (vec!["../escape.rs"], "not a repository-relative path"),
            (vec!["/etc/passwd"], "not a repository-relative path"),
            (vec![".git/config"], "not a repository-relative path"),
            (vec!["a.rs"], "nothing to commit"),
        ] {
            let report = commit_in(&git, &request(&paths), &Fake::new());
            assert!(
                report.outcome == Outcome::Refused
                    && report
                        .detail
                        .as_deref()
                        .is_some_and(|d| d.contains(expected)),
                "paths={paths:?} expected {expected:?}; report={report:#?}"
            );
        }
    }

    #[test]
    fn glob_matching_follows_fnmatch() {
        let cases = [
            ("xtask/**", "xtask/src/main.rs", true),
            ("xtask/**", "xtaskx/a", false),
            ("Cargo.toml", "Cargo.toml", true),
            ("crates/*/src/*.rs", "crates/types/src/lib.rs", true),
            ("*.md", "docs/a.md", true),
            ("a?c", "abc", true),
            ("a?c", "ac", false),
            ("", "a", false),
        ];
        for (pattern, text, expected) in cases {
            assert_eq!(
                glob_match(pattern, text),
                expected,
                "pattern={pattern} text={text}"
            );
        }
    }

    // --- Adversarial fixtures (W07.4, §11.10): the checkout moves while the gate runs. ---

    /// Makes a commit on `main` the way another agent's wrapper would: through plumbing
    /// and a private index, leaving the shared index untouched. Returns the new commit.
    fn concurrent_commit(git: &Git, path: &str, content: &str) -> String {
        let index = scratch_dir("concurrent-index").join("index");
        let plumbing = |args: &[&str], stdin: Option<&str>| -> String {
            let mut cmd = std::process::Command::new("git");
            cmd.arg("-C").arg(git.root()).args(args);
            for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_CEILING_DIRECTORIES"] {
                cmd.env_remove(var);
            }
            cmd.env("GIT_INDEX_FILE", &index)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped());
            let mut child = cmd.spawn().unwrap();
            if let Some(text) = stdin {
                use std::io::Write as _;
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(text.as_bytes())
                    .unwrap();
            }
            drop(child.stdin.take());
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        };
        let head = git.rev_parse("HEAD").unwrap();
        plumbing(&["read-tree", "HEAD"], None);
        let blob = plumbing(&["hash-object", "-w", "--stdin"], Some(content));
        plumbing(
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{path}"),
            ],
            None,
        );
        let tree = plumbing(&["write-tree"], None);
        let commit = plumbing(
            &["commit-tree", &tree, "-p", &head, "-m", "concurrent"],
            None,
        );
        plumbing(&["update-ref", "refs/heads/main", &commit, &head], None);
        commit
    }

    #[test]
    fn head_moving_mid_gate_reruns_on_the_new_parent_without_reverting_it() {
        let git = repo("adv-moving-head");
        fixture::write(git.root(), "a.rs", "a2\n");
        let moved = RefCell::new(None);
        let other = git.clone();
        let fake = Fake::new();
        *fake.during_gate.borrow_mut() = Box::new(|| {
            if moved.borrow().is_none() {
                *moved.borrow_mut() = Some(concurrent_commit(&other, "other.rs", "theirs\n"));
            }
        });
        let report = commit_in(&git, &request(&["a.rs"]), &fake);
        assert_eq!(report.outcome, Outcome::Committed, "report={report:#?}");
        assert_eq!(
            report.attempts, 2,
            "rule: HEAD moving during the gate forces a re-run"
        );
        let concurrent = moved.borrow().clone().unwrap();
        assert_eq!(
            git.rev_parse("HEAD~1").unwrap(),
            concurrent,
            "rule: committed on the new parent"
        );
        assert_eq!(
            git.run(&["show", "HEAD:other.rs"]).unwrap(),
            "theirs\n",
            "rule: the concurrent commit's file is never reverted"
        );
        assert_eq!(git.run(&["show", "HEAD:a.rs"]).unwrap(), "a2\n");
        let head_tree = git.rev_parse("HEAD^{tree}").unwrap();
        let gated = fake.gate_trees.borrow();
        assert_eq!(gated.len(), 2, "gated={gated:?}");
        assert_eq!(
            gated.last(),
            Some(&head_tree),
            "rule: the committed tree is the tree the deciding gate run tested (P11)"
        );
        assert_eq!(report.tree.as_deref(), Some(head_tree.as_str()));
    }

    #[test]
    fn a_checkout_that_keeps_moving_is_refused_after_the_attempt_limit() {
        let git = repo("adv-always-moving");
        let head = git.rev_parse("HEAD").unwrap();
        fixture::write(git.root(), "a.rs", "a2\n");
        let fake = Fake::new();
        let other = git.clone();
        let mut n = 0;
        *fake.during_gate.borrow_mut() = Box::new(move || {
            n += 1;
            concurrent_commit(&other, &format!("other{n}.rs"), "theirs\n");
        });
        let report = commit_in(&git, &request(&["a.rs"]), &fake);
        assert_eq!(report.outcome, Outcome::Refused, "report={report:#?}");
        assert_eq!(report.attempts, MAX_ATTEMPTS);
        let log = git
            .run(&["log", "--format=%s", &format!("{head}..HEAD")])
            .unwrap();
        assert!(
            log.lines().all(|subject| subject == "concurrent"),
            "rule: nothing of ours is committed; log={log}"
        );
        assert_eq!(
            git.staged_paths().unwrap(),
            Vec::<String>::new(),
            "our paths are unstaged"
        );
    }

    #[test]
    fn a_file_edited_mid_gate_is_not_committed() {
        let git = repo("adv-mid-gate-edit");
        fixture::write(git.root(), "a.rs", "a2\n");
        let fake = Fake::new();
        let root = git.root().to_path_buf();
        *fake.during_gate.borrow_mut() = Box::new(move || {
            fixture::write(&root, "a.rs", "a3 (edited during the gate)\n");
            fixture::write(&root, "b.rs", "someone else's edit\n");
        });
        let report = commit_in(&git, &request(&["a.rs"]), &fake);
        assert_eq!(report.outcome, Outcome::Committed, "report={report:#?}");
        assert_eq!(
            report.attempts, 1,
            "rule: unstaged edits do not change the candidate"
        );
        assert_eq!(
            git.run(&["show", "HEAD:a.rs"]).unwrap(),
            "a2\n",
            "rule: the staged, gated version is committed, never a later edit (§11.10)"
        );
        assert_eq!(git.run(&["show", "HEAD:b.rs"]).unwrap(), "b\n");
        assert_eq!(
            fs::read_to_string(git.root().join("a.rs")).unwrap(),
            "a3 (edited during the gate)\n",
            "rule: the later edit stays in the working tree, unstaged"
        );
    }

    #[test]
    fn foreign_staging_mid_gate_is_refused_and_left_alone() {
        let git = repo("adv-mid-gate-staging");
        let head = git.rev_parse("HEAD").unwrap();
        fixture::write(git.root(), "a.rs", "a2\n");
        let fake = Fake::new();
        let other = git.clone();
        *fake.during_gate.borrow_mut() = Box::new(move || {
            fixture::write(other.root(), "b.rs", "b2\n");
            other.stage(&["b.rs".to_owned()]).unwrap();
        });
        let report = commit_in(&git, &request(&["a.rs"]), &fake);
        assert_eq!(report.outcome, Outcome::Refused, "report={report:#?}");
        assert!(
            report.detail.as_deref().is_some_and(|d| d.contains("b.rs")),
            "report={report:#?}"
        );
        assert_eq!(git.rev_parse("HEAD").unwrap(), head, "nothing committed");
        assert_eq!(
            git.staged_paths().unwrap(),
            ["b.rs"],
            "rule: the wrapper never discards another agent's staging"
        );
    }

    #[test]
    fn a_leftover_planted_in_the_candidate_mid_gate_is_refused() {
        let git = repo("adv-leftover");
        let head = git.rev_parse("HEAD").unwrap();
        fixture::write(git.root(), "a.rs", "a2\n");
        let fake = Fake::new();
        let candidate = git.root().join(CANDIDATE_DIR);
        *fake.during_gate.borrow_mut() = Box::new(move || {
            fixture::write(&candidate, "tests/stray.rs", "#[test] fn stray() {}\n");
        });
        let report = commit_in(&git, &request(&["a.rs"]), &fake);
        assert_eq!(report.outcome, Outcome::Refused, "report={report:#?}");
        assert!(
            report
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("tests/stray.rs")),
            "rule: a leftover file in the candidate refuses the commit (§11.10 step 3); \
             report={report:#?}"
        );
        assert_eq!(git.rev_parse("HEAD").unwrap(), head);
    }

    #[test]
    fn no_branch_or_linked_worktree_is_ever_created() {
        let git = repo("adv-no-branches");
        let before = git.branches_and_worktrees().unwrap();
        fixture::write(git.root(), "a.rs", "a2\n");
        let report = commit_in(&git, &request(&["a.rs"]), &Fake::new());
        assert_eq!(report.outcome, Outcome::Committed, "report={report:#?}");
        let mut failing = Fake::new();
        failing.passed = false;
        fixture::write(git.root(), "b.rs", "b2\n");
        let _ = commit_in(&git, &request(&["b.rs"]), &failing);
        assert_eq!(
            git.branches_and_worktrees().unwrap(),
            before,
            "rule: the wrapper never creates a branch or a linked worktree (D-16)"
        );
    }
}
