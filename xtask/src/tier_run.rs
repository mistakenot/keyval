//! What a tiered gate run reads from and adds to its receipts (PLAN.md §10.10, §10.11;
//! W46.1): the T0 timing history, the T1 evidence T2 reuses, the protected-artifact
//! hashes, and the `tier_plan` section of the receipt.
//!
//! The decisions themselves live in [`crate::tier_plan`]; this module only reads receipts
//! and files and shapes the report. Durations and coverage are recorded as measurements,
//! never turned into a flaky percentage or a wall-clock cutoff.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::gate::{self, GateTier};
use crate::tier_plan::{Evidence, Overrun, ReuseSplit, Selected, Suite, T0Timing, TierPlan};

/// Files besides the policy files that a run must leave untouched: the plan, the API
/// contract and the registry (rules 7, 12; P9).
const PROTECTED_EXTRA: &[&str] = &[
    "PLAN.md",
    "AGENTS.md",
    "api/openapi.yaml",
    "registry/checks.toml",
    "registry/requirements.toml",
    "registry/workgraph.toml",
];

/// The `tier_plan` section of a v6 receipt.
#[derive(Debug, Serialize)]
pub struct TierReport {
    /// The tier's name in the plan: `T0`…`T3`.
    pub name: &'static str,
    /// The `checks.toml` tiers whose rows the run staged.
    pub registry_tiers: Vec<&'static str>,
    /// The crates the plan selects for the changed paths. Live runs still test every
    /// workspace member, a superset (see `tested`).
    pub selection: Selected,
    /// Whether the run tested every workspace member rather than only `selection`.
    pub tested_whole_workspace: bool,
    /// Replay pairs the tier requires, exactly.
    pub replay_pairs_required: u32,
    /// The tier's initial seed target, which is sized to fit and never a floor.
    pub seed_target: u64,
    /// Fresh release-build seeds the tier requires at least (T3 only).
    pub seed_minimum: Option<u64>,
    /// Checks taken from earlier T1 evidence instead of re-run (T2).
    pub reused: Vec<Reused>,
    /// Why reuse was refused, if it was.
    pub reuse_refused: Option<String>,
    /// What the run measured; `null` is unmeasured.
    pub measured: Measured,
    /// The pass criteria the run broke (§10.11).
    pub failures: Vec<String>,
    /// A sustained T0 overrun, which wants a bead and fails nothing.
    pub overrun: Option<Overrun>,
    /// The protected-artifact hashes before and after the run.
    pub protected: Protected,
}

/// One check whose latest T1 result stands.
#[derive(Debug, Serialize)]
pub struct Reused {
    /// The check ID.
    pub check: String,
    /// Its suite.
    pub suite: Suite,
    /// The T1 run that passed it.
    pub run_id: String,
    /// The tree that run judged.
    pub tree_id: String,
}

/// The facts a run measured, as they enter the pass criteria.
#[derive(Debug, Default, Serialize)]
pub struct Measured {
    /// `planned` checks within the tier's contents.
    pub planned_in_contents: Vec<String>,
    /// Replay pairs that ran.
    pub replay_pairs: Option<u32>,
    /// Fresh seeds on the tier's build.
    pub fresh_seeds: Option<u64>,
    /// Required coverage cells that are zero.
    pub zero_coverage_cells: Option<Vec<String>>,
    /// Checker violations.
    pub checker_violations: Option<u64>,
    /// Runs that did not finish.
    pub incomplete_runs: Option<u64>,
}

/// The protected files' hashes around a run.
#[derive(Debug, Default, Serialize)]
pub struct Protected {
    /// Before the first step.
    pub before: BTreeMap<String, String>,
    /// After the last step.
    pub after: BTreeMap<String, String>,
    /// Whether the two agree.
    pub unchanged: bool,
}

/// Hashes the policy files, the regression corpus and the plan, contract and registry.
#[must_use]
pub fn protected_hashes(root: &Path) -> BTreeMap<String, String> {
    let mut hashes: BTreeMap<String, String> = gate::policy_hashes(root)
        .into_iter()
        .map(|(name, hash)| (name.to_owned(), hash))
        .collect();
    for &name in PROTECTED_EXTRA {
        let hash = fs::read(root.join(name)).map_or_else(
            |_| "missing".to_owned(),
            |bytes| format!("sha256:{}", gate::sha256_hex(&bytes)),
        );
        hashes.insert(name.to_owned(), hash);
    }
    hashes
}

/// The paths whose hash differs between `before` and `after`, sorted.
#[must_use]
pub fn changed_hashes(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<String> {
    let names: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    names
        .into_iter()
        .filter(|name| before.get(*name) != after.get(*name))
        .cloned()
        .collect()
}

/// Receipts read per tier (CD-9): the newest by name, which sorts by start time. T0 needs only
/// its last few runs and T2 only the latest T1 runs, so older receipts are never parsed.
const MAX_RECEIPTS: usize = 256;

/// The largest receipt file read (CD-9); a bigger one is not evidence.
const MAX_RECEIPT_BYTES: u64 = 8 * 1024 * 1024;

/// The gate receipts of `tier` in `receipts`, oldest first, each as parsed JSON. A file
/// that is not a gate receipt, is too large, or cannot be read is skipped: it is not
/// evidence.
fn gate_receipts(receipts: &Path, tier: GateTier) -> Vec<Value> {
    let name = gate::tier_name(tier);
    let prefix = format!("gate-{name}-");
    let Ok(entries) = fs::read_dir(receipts) else {
        return Vec::new();
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let file = entry.file_name();
            let file = file.to_string_lossy();
            file.starts_with(&prefix) && file.ends_with(".json")
        })
        .collect();
    files.sort_by_key(fs::DirEntry::file_name);
    let newest = files.len().saturating_sub(MAX_RECEIPTS);
    let mut found: Vec<Value> = files
        .into_iter()
        .skip(newest)
        .filter(|entry| entry.metadata().is_ok_and(|m| m.len() <= MAX_RECEIPT_BYTES))
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .filter_map(|text| serde_json::from_str::<Value>(&text).ok())
        .filter(|value| value["kind"] == "gate" && value["tier"] == name)
        .collect();
    found.sort_by(|a, b| a["finished_at"].as_str().cmp(&b["finished_at"].as_str()));
    found
}

/// The duration verdict of each T0 receipt in `receipts`.
#[must_use]
pub fn t0_history(receipts: &Path) -> Vec<T0Timing> {
    gate_receipts(receipts, GateTier::Fast)
        .iter()
        .filter_map(|value| {
            Some(T0Timing {
                run_id: value["run_id"].as_str()?.to_owned(),
                finished_at: value["finished_at"].as_str()?.to_owned(),
                over_alert: value["budget"]["over_alert"].as_bool()?,
            })
        })
        .collect()
}

/// Every T1 run in `receipts` as T2 evidence: whether it passed and which of its steps did.
#[must_use]
pub fn t1_evidence(receipts: &Path) -> Vec<Evidence> {
    gate_receipts(receipts, GateTier::Integration)
        .iter()
        .filter_map(|value| {
            let passed_checks = value["steps"]
                .as_array()?
                .iter()
                .filter(|step| step["status"] == "passed")
                .filter_map(|step| step["id"].as_str().map(str::to_owned))
                .collect();
            Some(Evidence {
                run_id: value["run_id"].as_str()?.to_owned(),
                tree_id: value["tree"]["id"].as_str()?.to_owned(),
                finished_at: value["finished_at"].as_str()?.to_owned(),
                passed: value["result"] == "passed",
                passed_checks,
            })
        })
        .collect()
}

/// What [`crate::tier_plan`] decided before the steps ran.
#[derive(Debug)]
pub struct Prepared {
    /// The tier's rules.
    pub plan: TierPlan,
    /// The crates selected for the changed paths.
    pub selection: Selected,
    /// The checks to run and those to reuse.
    pub split: ReuseSplit,
    /// The T1 run vouching for the reused checks, or why none does.
    pub evidence: Result<Option<Evidence>, String>,
}

impl Prepared {
    /// The report's `reused` entries, empty if the evidence was refused.
    #[must_use]
    pub fn reused(&self) -> Vec<Reused> {
        match &self.evidence {
            Ok(Some(found)) => self
                .split
                .reused
                .iter()
                .map(|(check, suite)| Reused {
                    check: check.clone(),
                    suite: *suite,
                    run_id: found.run_id.clone(),
                    tree_id: found.tree_id.clone(),
                })
                .collect(),
            Ok(None) | Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_dir;

    fn receipt(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }

    // check: TIER-RUN
    #[test]
    fn protected_hashes_cover_the_plan_contract_and_registry_and_notice_an_edit() {
        let root = scratch_dir("tier-run-protected");
        fs::write(root.join("PLAN.md"), "plan\n").unwrap();
        let before = protected_hashes(&root);
        for name in PROTECTED_EXTRA {
            assert!(before.contains_key(*name), "rule: {name} is protected");
        }
        assert_eq!(before["api/openapi.yaml"], "missing");
        assert_eq!(changed_hashes(&before, &before), Vec::<String>::new());
        fs::write(root.join("PLAN.md"), "plan, weakened\n").unwrap();
        let after = protected_hashes(&root);
        assert_eq!(
            changed_hashes(&before, &after),
            ["PLAN.md"],
            "rule: an edit to a protected file is reported by name"
        );
    }

    #[test]
    fn t1_evidence_reads_only_t1_gate_receipts_and_their_passed_steps() {
        let dir = scratch_dir("tier-run-t1");
        receipt(
            &dir,
            "gate-integration-a.json",
            r#"{"kind":"gate","tier":"integration","run_id":"r1","finished_at":"2026-10-17T01:00:00Z",
                "result":"passed","tree":{"id":"t"},
                "steps":[{"id":"CONF","status":"passed"},{"id":"X","status":"failed"}]}"#,
        );
        receipt(
            &dir,
            "gate-fast-b.json",
            r#"{"kind":"gate","tier":"fast","run_id":"r0","finished_at":"2026-10-17T02:00:00Z",
                "result":"passed","tree":{"id":"t"},"steps":[]}"#,
        );
        receipt(&dir, "gate-integration-c.json", "not json");
        receipt(
            &dir,
            "other.json",
            r#"{"kind":"gate","tier":"integration"}"#,
        );
        let found = t1_evidence(&dir);
        assert_eq!(found.len(), 1, "rule: only T1 gate receipts are evidence");
        assert_eq!(found[0].run_id, "r1");
        assert!(found[0].passed);
        assert_eq!(
            found[0]
                .passed_checks
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["CONF"]
        );
    }

    #[test]
    fn only_the_newest_receipts_and_no_oversized_file_are_read() {
        let dir = scratch_dir("tier-run-bounds");
        for n in 0..MAX_RECEIPTS + 5 {
            let body = format!(
                r#"{{"kind":"gate","tier":"fast","run_id":"r{n:04}","finished_at":"2026-10-17T00:{:02}:{:02}Z","budget":{{"over_alert":false}}}}"#,
                n / 60,
                n % 60
            );
            receipt(&dir, &format!("gate-fast-{n:04}.json"), &body);
        }
        let big = "x".repeat(usize::try_from(MAX_RECEIPT_BYTES).unwrap() + 1);
        receipt(&dir, "gate-fast-9999.json", &big);
        let history = t0_history(&dir);
        assert_eq!(
            history.len(),
            MAX_RECEIPTS - 1,
            "rule: at most {MAX_RECEIPTS} receipts are read (CD-9) and the oversized one is skipped"
        );
        assert_eq!(
            history.first().map(|t| t.run_id.as_str()),
            Some("r0006"),
            "rule: the oldest receipts are the ones left unread"
        );
    }

    #[test]
    fn t0_history_reads_the_duration_verdict_in_time_order() {
        let dir = scratch_dir("tier-run-t0");
        receipt(
            &dir,
            "gate-fast-2.json",
            r#"{"kind":"gate","tier":"fast","run_id":"b","finished_at":"2026-10-17T02:00:00Z","budget":{"over_alert":true}}"#,
        );
        receipt(
            &dir,
            "gate-fast-1.json",
            r#"{"kind":"gate","tier":"fast","run_id":"a","finished_at":"2026-10-17T01:00:00Z","budget":{"over_alert":false}}"#,
        );
        let history = t0_history(&dir);
        let order: Vec<(&str, bool)> = history
            .iter()
            .map(|t| (t.run_id.as_str(), t.over_alert))
            .collect();
        assert_eq!(order, [("a", false), ("b", true)]);
        assert_eq!(t0_history(&dir.join("missing")).len(), 0);
    }
}
