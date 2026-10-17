//! What each gate tier selects, and what passing means (PLAN.md §10.10, §10.11; W46.1).
//!
//! This module is pure: it decides, from data, so fixtures can pin every rule without
//! running a gate.
//!
//! - **Contents.** [`plan`] gives each tier the `checks.toml` tiers it runs, how it picks
//!   crates, its exact replay-pair count, its seed budget and whether it may carry a
//!   `planned` check. Counts and budgets are measurements the run reports; a wall-clock
//!   or percentage cutoff never decides a verdict (§10.11).
//! - **Crate selection.** [`select`] gives T0 the changed crates plus every workspace
//!   member that depends on them, directly or not (dev and build edges included), and
//!   every other tier the whole workspace.
//! - **T2 reuse.** T2 does not re-run conformance, real-file probes or bug variants: their
//!   latest T1 evidence stands (§10.11). [`reuse_split`] separates those checks from the
//!   rest and [`reusable_evidence`] finds the T1 run that vouches for them, or says why
//!   none does.
//! - **Pass criteria.** [`judge`] turns a tier's measured facts into failures. A fact the
//!   run did not measure is `None`: a tier below T3 reports it, T3 fails on it, because
//!   T3 alone requires every listed content (§10.10 "Staged contents").
//! - **T0 duration.** [`sustained_overrun`] reads the recent T0 receipts. T0 is never
//!   failed or cut off for being slow; a sustained overrun is a finding that wants a bead.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::gate::GateTier;
use crate::metadata::Metadata;

/// Replay pairs T0 runs (§10.10).
pub const T0_REPLAY_PAIRS: u32 = 2;
/// Replay pairs T1 runs.
pub const T1_REPLAY_PAIRS: u32 = 16;
/// Replay pairs T2 runs.
pub const T2_REPLAY_PAIRS: u32 = 64;
/// Replay pairs T3 runs.
pub const T3_REPLAY_PAIRS: u32 = 100;

/// Fresh swarm seeds T0 aims at; the budget is tuned to the T0 time target, so it is a
/// target, not a floor.
pub const T0_SEED_TARGET: u64 = 50;
/// Fresh swarm seeds T1 aims at ("initially 2,000", sized to fit).
pub const T1_SEED_TARGET: u64 = 2_000;
/// Fresh swarm seeds T2 aims at ("initially 10,000").
pub const T2_SEED_TARGET: u64 = 10_000;
/// Fresh swarm seeds on the release build that T3 requires at least.
pub const T3_SEED_MINIMUM: u64 = 100_000;

/// Consecutive T0 receipts over the alert threshold that make an overrun "sustained".
///
/// §10.11 says "sustained" without a number. One slow run on a loaded machine proves
/// nothing, and the rule must not be flaky, so it takes three in a row, all recorded in
/// receipts. Changing this number is a plan question, not a tuning knob.
pub const SUSTAINED_RUNS: usize = 3;

/// How a tier picks the crates whose tests it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Selection {
    /// The changed crates and every workspace member that depends on one (T0).
    ChangedWithReverseDependencies,
    /// Every workspace member (T1, T2, T3).
    WholeWorkspace,
}

/// A suite whose latest T1 evidence T2 reuses instead of running it again (§10.11).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Suite {
    /// The black-box conformance suite (`CONF-…`).
    Conformance,
    /// Real-file crash probes and the lifecycle smoke.
    RealFileProbes,
    /// Deliberate bug variants (`BUG-VARIANT…`).
    BugVariants,
}

/// Which reusable suite a check ID belongs to, if any.
///
/// Matching is by ID because `checks.toml` has no suite column; a new suite check needs a
/// name that matches here, and the fixtures below pin the current ones.
#[must_use]
pub fn suite_of(check_id: &str) -> Option<Suite> {
    if check_id.starts_with("CONF-") {
        Some(Suite::Conformance)
    } else if ["CRASH-PROBES", "LIFECYCLE-SMOKE"].contains(&check_id) {
        Some(Suite::RealFileProbes)
    } else if check_id.starts_with("BUG-VARIANT") {
        Some(Suite::BugVariants)
    } else {
        None
    }
}

/// One tier's rules. Invariants: `registry_tiers` lists the `checks.toml` tiers the tier
/// runs, in order, and always contains the tier's own; a tier that reuses suites reuses
/// exactly the three of [`Suite`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct TierPlan {
    /// The gate tier.
    pub tier: GateTier,
    /// The tier's name in `checks.toml` and the plan: `T0`…`T3`.
    pub name: &'static str,
    /// The `checks.toml` tiers whose `implemented` rows this tier runs, in order.
    pub registry_tiers: &'static [&'static str],
    /// How it picks crates.
    pub selection: Selection,
    /// Replay pairs it requires, exactly.
    pub replay_pairs: u32,
    /// Fresh swarm seeds it aims at.
    pub seed_target: u64,
    /// Fresh swarm seeds it requires at least; only T3 has a floor.
    pub seed_minimum: Option<u64>,
    /// Suites it takes from the latest T1 run instead of running.
    pub reuses_t1: &'static [Suite],
    /// Whether a `planned` check inside its contents is tolerated (reported). T3 alone
    /// accepts none.
    pub accepts_planned: bool,
    /// Whether it materialises its own per-run tree (T1, T2, T3); T0 runs in the commit
    /// candidate (§10.10 "Tree isolation").
    pub per_run_tree: bool,
}

const REUSED: &[Suite] = &[
    Suite::Conformance,
    Suite::RealFileProbes,
    Suite::BugVariants,
];

/// The rules of `tier`.
#[must_use]
pub const fn plan(tier: GateTier) -> TierPlan {
    match tier {
        GateTier::Fast => TierPlan {
            tier,
            name: "T0",
            registry_tiers: &["T0"],
            selection: Selection::ChangedWithReverseDependencies,
            replay_pairs: T0_REPLAY_PAIRS,
            seed_target: T0_SEED_TARGET,
            seed_minimum: None,
            reuses_t1: &[],
            accepts_planned: true,
            per_run_tree: false,
        },
        GateTier::Integration => TierPlan {
            tier,
            name: "T1",
            registry_tiers: &["T0", "T1"],
            selection: Selection::WholeWorkspace,
            replay_pairs: T1_REPLAY_PAIRS,
            seed_target: T1_SEED_TARGET,
            seed_minimum: None,
            reuses_t1: &[],
            accepts_planned: true,
            per_run_tree: true,
        },
        GateTier::Campaign => TierPlan {
            tier,
            name: "T2",
            registry_tiers: &["T0", "T1", "T2"],
            selection: Selection::WholeWorkspace,
            replay_pairs: T2_REPLAY_PAIRS,
            seed_target: T2_SEED_TARGET,
            seed_minimum: None,
            reuses_t1: REUSED,
            accepts_planned: true,
            per_run_tree: true,
        },
        GateTier::Release => TierPlan {
            tier,
            name: "T3",
            registry_tiers: &["T0", "T1", "T2", "T3"],
            selection: Selection::WholeWorkspace,
            replay_pairs: T3_REPLAY_PAIRS,
            seed_target: T3_SEED_MINIMUM,
            seed_minimum: Some(T3_SEED_MINIMUM),
            reuses_t1: &[],
            accepts_planned: false,
            per_run_tree: true,
        },
    }
}

/// A workspace member as selection sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// Package name.
    pub name: String,
    /// Repository-relative directory, `/`-separated, without a trailing slash.
    pub dir: String,
    /// Names of the workspace members it depends on, of any kind.
    pub depends_on: Vec<String>,
}

/// The members of the workspace under `root`, from `cargo metadata`.
#[must_use]
pub fn members(metadata: &Metadata, root: &Path) -> Vec<Member> {
    let all = metadata.members();
    let names: BTreeSet<&str> = all.iter().map(|p| p.name.as_str()).collect();
    all.iter()
        .map(|package| Member {
            name: package.name.clone(),
            dir: package
                .dir()
                .strip_prefix(root)
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default(),
            depends_on: package
                .dependencies
                .iter()
                .filter(|dep| names.contains(dep.name.as_str()))
                .map(|dep| dep.name.clone())
                .collect(),
        })
        .collect()
}

/// Why a selection is what it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The tier always takes the whole workspace.
    TierSelectsWholeWorkspace,
    /// No changed path was given, so nothing can be left out.
    NoChangedPaths,
    /// A changed path is outside every member and can affect all of them.
    SharedPathChanged,
    /// Only the changed members and their reverse dependencies.
    ChangedMembers,
    /// Every changed path is documentation or tracker data that no crate builds from.
    NothingBuilt,
}

/// The crates a tier tests.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Selected {
    /// Member names, sorted.
    pub crates: Vec<String>,
    /// Whether this is every member.
    pub whole_workspace: bool,
    /// Why.
    pub reason: Reason,
}

/// Paths that no crate is built or tested from: generated reports and nested prose.
fn builds_nothing(path: &str) -> bool {
    // Root-level prose (`PLAN.md`, `AGENTS.md`) and the tracker export are read by xtask's
    // own checks (SPEC, AGENTS-RULES, WORKGRAPH, DEV-BEADS), so a change to them selects
    // everything; only nested prose and generated reports build nothing.
    path.starts_with("reports/")
        || (path.contains('/')
            && Path::new(path)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md")))
}

/// The member owning `path`: the one with the longest directory prefix.
fn owner<'a>(members: &'a [Member], path: &str) -> Option<&'a Member> {
    members
        .iter()
        .filter(|m| {
            !m.dir.is_empty()
                && path
                    .strip_prefix(m.dir.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
        .max_by_key(|m| m.dir.len())
}

/// Picks the crates `plan`'s tier tests for `changed` paths.
///
/// T0 takes each changed path's owning member plus all members that depend on one, until
/// no more join. A path outside every member (a manifest, the lockfile, the registry, the
/// API spec, a corpus) can change any crate's result, so it selects everything; so does an
/// empty `changed`, because nothing can then be left out safely (P10). A change to
/// prose or tracker data alone selects nothing.
#[must_use]
pub fn select(plan: &TierPlan, members: &[Member], changed: &[String]) -> Selected {
    let everything = |reason| Selected {
        crates: sorted_names(members.iter().map(|m| m.name.as_str())),
        whole_workspace: true,
        reason,
    };
    match plan.selection {
        Selection::WholeWorkspace => return everything(Reason::TierSelectsWholeWorkspace),
        Selection::ChangedWithReverseDependencies => {}
    }
    if changed.is_empty() {
        return everything(Reason::NoChangedPaths);
    }
    let mut hit: BTreeSet<&str> = BTreeSet::new();
    for path in changed {
        match owner(members, path) {
            Some(member) => {
                hit.insert(member.name.as_str());
            }
            None if builds_nothing(path) => {}
            None => return everything(Reason::SharedPathChanged),
        }
    }
    if hit.is_empty() {
        return Selected {
            crates: Vec::new(),
            whole_workspace: false,
            reason: Reason::NothingBuilt,
        };
    }
    loop {
        let before = hit.len();
        for member in members {
            if member
                .depends_on
                .iter()
                .any(|dep| hit.contains(dep.as_str()))
            {
                hit.insert(member.name.as_str());
            }
        }
        if hit.len() == before {
            break;
        }
    }
    let crates = sorted_names(hit.into_iter());
    Selected {
        whole_workspace: crates.len() == members.len(),
        crates,
        reason: Reason::ChangedMembers,
    }
}

fn sorted_names<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let set: BTreeSet<&str> = names.collect();
    set.into_iter().map(str::to_owned).collect()
}

/// The checks a tier runs itself, and those it takes from T1 evidence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReuseSplit {
    /// Checks to run.
    pub run: Vec<String>,
    /// Checks whose latest T1 result stands, with their suite.
    pub reused: Vec<(String, Suite)>,
}

/// Separates `ids` (in order) into those `plan`'s tier runs and those it reuses.
#[must_use]
pub fn reuse_split(plan: &TierPlan, ids: &[String]) -> ReuseSplit {
    let mut split = ReuseSplit::default();
    for id in ids {
        match suite_of(id).filter(|suite| plan.reuses_t1.contains(suite)) {
            Some(suite) => split.reused.push((id.clone(), suite)),
            None => split.run.push(id.clone()),
        }
    }
    split
}

/// What a finished T1 run vouches for.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Evidence {
    /// The T1 run ID.
    pub run_id: String,
    /// The tree the run judged.
    pub tree_id: String,
    /// When it finished (RFC 3339 UTC, so text order is time order).
    pub finished_at: String,
    /// Whether the run as a whole passed.
    pub passed: bool,
    /// The checks that passed in it.
    pub passed_checks: BTreeSet<String>,
}

/// The newest passing T1 run that covers every check in `needed`; otherwise why none does.
///
/// A newer T1 run that failed or lacks a check hides older ones: "latest" means latest,
/// and an older green run must not paper over a newer red one.
///
/// # Errors
///
/// Returns the reason when there is no T1 run, the latest failed, or it lacks a check.
pub fn reusable_evidence<'a>(
    runs: &'a [Evidence],
    needed: &[(String, Suite)],
) -> Result<&'a Evidence, String> {
    let latest = runs
        .iter()
        .max_by(|a, b| a.finished_at.cmp(&b.finished_at))
        .ok_or_else(|| "no T1 run exists whose evidence T2 could reuse".to_owned())?;
    if !latest.passed {
        return Err(format!(
            "the latest T1 run {} did not pass, so its conformance, probe and bug-variant \
             evidence cannot stand",
            latest.run_id
        ));
    }
    let missing: Vec<&str> = needed
        .iter()
        .map(|(id, _)| id.as_str())
        .filter(|id| !latest.passed_checks.contains(*id))
        .collect();
    if missing.is_empty() {
        Ok(latest)
    } else {
        Err(format!(
            "the latest T1 run {} has no passing result for {}",
            latest.run_id,
            missing.join(", ")
        ))
    }
}

/// What a tier run measured. `None` means the run did not measure it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// `planned` checks inside the tier's contents.
    pub planned_in_contents: Vec<String>,
    /// Replay pairs that ran.
    pub replay_pairs: Option<u32>,
    /// Fresh swarm seeds that ran on the tier's build.
    pub fresh_seeds: Option<u64>,
    /// Required coverage cells that are zero.
    pub zero_coverage_cells: Option<Vec<String>>,
    /// Checker violations found.
    pub checker_violations: Option<u64>,
    /// Runs that did not finish.
    pub incomplete_runs: Option<u64>,
}

/// The pass criteria (§10.11) that a tier's measured facts break; empty when none does.
///
/// Min-case enforcement and zero-test refusal are the per-check steps' job; this judges
/// what only the whole run knows. Counts are compared exactly where the plan fixes them
/// (replay pairs) and as a floor where it does (T3's seeds); T1 and T2's seed budgets are
/// sized to fit, so they are reported, never failed. A fact a tier below T3 did not
/// measure is not a failure, because the tier runs only contents whose checks exist;
/// T3 fails on every unmeasured one.
#[must_use]
pub fn judge(plan: &TierPlan, facts: &Facts) -> Vec<String> {
    let mut failures = Vec::new();
    let all_required = !plan.accepts_planned;
    if all_required {
        failures.extend(facts.planned_in_contents.iter().map(|id| {
            format!(
                "{} accepts no planned check, but {id} is planned (§10.10)",
                plan.name
            )
        }));
    }
    match facts.replay_pairs {
        Some(n) if n != plan.replay_pairs => failures.push(format!(
            "{} requires exactly {} replay pairs, ran {n}",
            plan.name, plan.replay_pairs
        )),
        None if all_required => failures.push(format!(
            "{} requires {} replay pairs, none were measured",
            plan.name, plan.replay_pairs
        )),
        Some(_) | None => {}
    }
    match (facts.fresh_seeds, plan.seed_minimum) {
        (Some(n), Some(min)) if n < min => failures.push(format!(
            "{} requires at least {min} fresh seeds on the release build, ran {n}",
            plan.name
        )),
        (None, Some(min)) => failures.push(format!(
            "{} requires at least {min} fresh seeds, none were measured",
            plan.name
        )),
        (Some(_) | None, _) => {}
    }
    match &facts.zero_coverage_cells {
        Some(cells) if !cells.is_empty() => failures.push(format!(
            "required coverage cells are zero: {}",
            cells.join(", ")
        )),
        None if all_required => {
            failures.push("required coverage cells were not measured".to_owned());
        }
        Some(_) | None => {}
    }
    if plan.tier != GateTier::Fast {
        for (what, n) in [
            ("checker violations", facts.checker_violations),
            ("incomplete runs", facts.incomplete_runs),
        ] {
            match n {
                Some(0) => {}
                Some(n) => failures.push(format!("{} found {n} {what}, requires 0", plan.name)),
                None if all_required => {
                    failures.push(format!("{what} were not measured"));
                }
                None => {}
            }
        }
    }
    failures
}

/// A T0 overrun that lasted, which wants a bead and does not fail anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Overrun {
    /// Consecutive most recent T0 receipts over the alert threshold.
    pub consecutive: usize,
    /// The run IDs, newest first.
    pub run_ids: Vec<String>,
}

/// One T0 receipt's duration verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct T0Timing {
    /// Run ID.
    pub run_id: String,
    /// Finish time (RFC 3339 UTC).
    pub finished_at: String,
    /// Whether the run exceeded the alert threshold.
    pub over_alert: bool,
}

/// The sustained overrun in `history`, if its newest [`SUSTAINED_RUNS`] receipts are all
/// over the alert threshold. The order of `history` does not matter.
#[must_use]
pub fn sustained_overrun(history: &[T0Timing]) -> Option<Overrun> {
    let mut newest: Vec<&T0Timing> = history.iter().collect();
    newest.sort_by(|a, b| b.finished_at.cmp(&a.finished_at));
    let window: Vec<&T0Timing> = newest.into_iter().take(SUSTAINED_RUNS).collect();
    (window.len() == SUSTAINED_RUNS && window.iter().all(|t| t.over_alert)).then(|| Overrun {
        consecutive: window.len(),
        run_ids: window.iter().map(|t| t.run_id.clone()).collect(),
    })
}

#[cfg(test)]
mod tests;
