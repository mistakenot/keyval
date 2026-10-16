//! Judging a finished run (PLAN.md §10.5, §10.16; T-13, P10): the `keyval.observation/v1`
//! document of the run goes through K-1 to K-12 and K-13, and the result is a verdict with
//! the checkers that ran.
//!
//! The checkers are the independent oracles of the `checker` crate. This module only hands
//! them the run's document and collects what they say. A checker that cannot read the
//! document is a reason the run is incomplete, never a pass (P10).

use checker::{
    CheckError, GodView, Heal, History, Reads, Report, Trace, Vacuity, Verdict, k1, k2, k3, k4, k5,
    k6, k7, k8, k9, k10, k11, k12, k13,
};
use serde_json::Value;

/// How a run was judged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judged {
    /// The overall verdict: [`Verdict::Pass`] only if every checker ran and held and the run
    /// was not vacuous.
    pub overall: Verdict,
    /// The checkers that ran, K-13 last; a checker missing here is in `incomplete`.
    pub ran: Vec<&'static str>,
    /// One line per violation found, naming its checker, rule and detail.
    pub failures: Vec<String>,
    /// Why the run is incomplete: unreadable views and K-13's findings.
    pub incomplete: Vec<String>,
}

/// `check` on `view`, or the error that kept the view from being read.
fn on<V>(
    view: &Result<V, CheckError>,
    check: fn(&V) -> Result<Report, CheckError>,
) -> Result<Report, CheckError> {
    view.as_ref().map_err(Clone::clone).and_then(check)
}

/// The record kinds only the node side can produce. A document with none of a kind the
/// checkers need says nothing about the system, so judging it would report the recorder's gap
/// as the system's violation.
const NODE_SIDE: [&str; 5] = ["recovered", "append", "committed", "node_state", "accept"];

/// The [`NODE_SIDE`] kinds that `doc` holds no record of.
fn missing_node_side(doc: &Value) -> Vec<&'static str> {
    let records = doc["records"].as_array().map_or(&[][..], Vec::as_slice);
    NODE_SIDE
        .iter()
        .copied()
        .filter(|kind| !records.iter().any(|record| record["kind"] == *kind))
        .collect()
}

/// Judges the run whose observation document is `doc`.
///
/// A document without the node side's records is [`Verdict::Incomplete`] and no checker runs
/// on it (P10): its gaps are the recorder's, not the system's.
#[must_use]
pub fn judge(doc: &Value) -> Judged {
    let missing = missing_node_side(doc);
    if !missing.is_empty() {
        return Judged {
            overall: Verdict::Incomplete,
            ran: Vec::new(),
            failures: Vec::new(),
            incomplete: vec![format!(
                "the run's observation holds no node-side records of kind {missing:?}, so no \
                 checker can judge it (the node-side recorder is not wired to the simulator); \
                 an unjudged run is incomplete, never a pass (P10)"
            )],
        };
    }
    let mut incomplete = Vec::new();
    let mut reports: Vec<Report> = Vec::new();
    let mut keep = |name: &'static str, result: Result<Report, CheckError>| match result {
        Ok(report) => reports.push(report),
        Err(error) => incomplete.push(format!("{name} could not judge the run: {error:?}")),
    };
    let history = History::from_value(doc);
    let reads = Reads::from_value(doc);
    let heal = Heal::from_value(doc);
    let god = GodView::from_value(doc);
    let trace = Trace::from_value(doc);
    keep("K-1", on(&history, k1));
    keep("K-2", on(&history, k2));
    keep("K-3", on(&history, k3));
    keep("K-4", on(&history, k4));
    keep("K-5", on(&reads, k5));
    keep("K-6", on(&reads, k6));
    keep("K-7", on(&heal, k7));
    keep("K-8", on(&god, k8));
    keep("K-9", on(&heal, k9));
    keep("K-10", k10(doc));
    keep("K-11", on(&god, k11));
    keep("K-12", on(&trace, k12));
    let failures: Vec<String> = reports
        .iter()
        .flat_map(|report| {
            report.violations.iter().map(|violation| {
                format!(
                    "{} {:?}: {}",
                    report.checker, violation.kind, violation.detail
                )
            })
        })
        .collect();
    let mut ran: Vec<&'static str> = reports.iter().map(|report| report.checker).collect();
    let mut failed = !failures.is_empty();
    match Vacuity::from_value(doc) {
        Ok(view) => {
            let vacuity = k13(&view, &reports);
            ran.push(vacuity.checker);
            for violation in &vacuity.violations {
                incomplete.push(format!("K-13 {:?}: {}", violation.kind, violation.detail));
            }
        }
        Err(error) => incomplete.push(format!("K-13 could not judge the run: {error:?}")),
    }
    failed |= reports.iter().any(|report| !report.passed());
    let overall = if failed {
        Verdict::Fail
    } else if incomplete.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Incomplete
    };
    Judged {
        overall,
        ran,
        failures,
        incomplete,
    }
}
