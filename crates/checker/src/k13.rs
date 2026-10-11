//! K-13, non-vacuity (PLAN.md §10.16, T-13; D-73): a run that exercised nothing proves nothing,
//! so a run missing any of the following is `incomplete`, and incomplete fails (P10).
//!
//! 1. A write succeeded before the first fault ([`Kind::NoPreFaultWrite`]); with no fault
//!    selected, before the chaos phase.
//! 2. Every fault type the manifest selected was injected at least once
//!    ([`Kind::FaultNotInjected`]).
//! 3. Reads were served by more than one node in a five-voter run, and by every voter in a
//!    scripted 1- or 3-voter run ([`Kind::ReadsFromTooFewNodes`]).
//! 4. A write succeeded after the heal began: the catch-up marker, or in a drain heal the
//!    certified drain ([`Kind::NoPostHealWrite`]).
//! 5. Every logical write has a terminal resolution, or K-9 reported its liveness failure
//!    ([`Kind::WriteUnaccounted`]).
//! 6. Every applicable checker ([`APPLICABLE`]) ran ([`Kind::CheckerNotExecuted`]).
//! 7. The event budget was not exhausted and nothing overflowed
//!    ([`Kind::BudgetExhausted`], [`Kind::ObservationOverflowed`]).
//!
//! A reported `scenario invalid` is [`Kind::ScenarioInvalid`]: a generator defect, never a
//! pass. [`judge`] turns the reports into the overall verdict. An all-timeout run has no
//! successful write, so it fails condition 1.

use crate::heal::Completion;
use crate::report::{Kind, Report, Violation};
use crate::trace::Resolved;
use crate::vacuity::Vacuity;

/// The checkers whose execution K-13 requires.
pub const APPLICABLE: [&str; 12] = [
    "K-1", "K-2", "K-3", "K-4", "K-5", "K-6", "K-7", "K-8", "K-9", "K-10", "K-11", "K-12",
];

/// A verdict on a checker or a run (PLAN.md §10.2 Outputs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Everything required ran and held.
    Pass,
    /// A checker found a violation.
    Fail,
    /// The run did not exercise what it must: it fails (P10) but is not a checker's finding.
    Incomplete,
}

impl Verdict {
    /// Whether the verdict lets the run pass: only [`Verdict::Pass`] does.
    #[must_use]
    pub const fn passes(self) -> bool {
        match self {
            Self::Pass => true,
            Self::Fail | Self::Incomplete => false,
        }
    }
}

/// The verdicts of one run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judgement {
    /// The overall verdict.
    pub overall: Verdict,
    /// One verdict per checker, in the order the reports were given, K-13 last.
    pub checkers: Vec<(&'static str, Verdict)>,
}

fn violation(kind: Kind, request_id: &str, detail: String) -> Violation {
    Violation {
        kind,
        request_id: request_id.to_owned(),
        attempt: None,
        detail,
    }
}

/// Judges `view` against K-13, given the reports of the checkers that ran.
#[must_use]
pub fn k13(view: &Vacuity, reports: &[Report]) -> Report {
    let mut found = Vec::new();
    pre_fault_write(view, &mut found);
    faults(view, &mut found);
    reads(view, &mut found);
    post_heal_write(view, &mut found);
    resolution(view, reports, &mut found);
    executed(reports, &mut found);
    completeness(view, &mut found);
    Report {
        checker: "K-13",
        examined: 7,
        violations: found,
    }
}

fn pre_fault_write(view: &Vacuity, found: &mut Vec<Violation>) {
    let boundary = view
        .first_fault
        .or(view.chaos_at)
        .or(view.heal_at)
        .unwrap_or(view.records);
    if !view.write_successes.iter().any(|at| *at < boundary) {
        found.push(violation(
            Kind::NoPreFaultWrite,
            "",
            format!(
                "rule: a write succeeded before the first fault (T-13); no successful write \
                 before record {boundary}"
            ),
        ));
    }
}

fn faults(view: &Vacuity, found: &mut Vec<Violation>) {
    for fault in view.selected.difference(&view.injected) {
        found.push(violation(
            Kind::FaultNotInjected,
            "",
            format!("rule: every selected fault is injected (T-13); `{fault}` never started"),
        ));
    }
}

fn reads(view: &Vacuity, found: &mut Vec<Violation>) {
    let served: Vec<u64> = view.read_servers.keys().copied().collect();
    let wanted = if view.voters.len() >= 5 {
        None
    } else {
        Some(
            view.voters
                .difference(&view.read_servers.keys().copied().collect())
                .copied()
                .collect::<Vec<_>>(),
        )
    };
    let detail = match wanted {
        None if served.len() < 2 => {
            format!("five-voter run: reads served by nodes {served:?}, need at least two")
        }
        Some(missing) if !missing.is_empty() || view.voters.is_empty() || served.is_empty() => {
            format!("reads served by nodes {served:?}; voters {missing:?} served none")
        }
        None | Some(_) => return,
    };
    found.push(violation(
        Kind::ReadsFromTooFewNodes,
        "",
        format!("rule: reads are served by more than one node (T-13); {detail}"),
    ));
}

fn post_heal_write(view: &Vacuity, found: &mut Vec<Violation>) {
    let Some(heal) = view.heal_at else {
        found.push(violation(
            Kind::NoPostHealWrite,
            "",
            "rule: a write succeeds after heal (T-13); the run has no `heal` phase".to_owned(),
        ));
        return;
    };
    let landed = match view.completion {
        Completion::Marker | Completion::PrecreatedMarker => {
            view.markers.iter().any(|at| *at > heal)
        }
        Completion::Drain => view.drain.is_some_and(|at| at > heal),
    };
    if !landed {
        found.push(violation(
            Kind::NoPostHealWrite,
            "",
            format!(
                "rule: a write succeeds after heal (T-13); no marker applied or drain \
                 certified after record {heal}"
            ),
        ));
    }
}

fn resolution(view: &Vacuity, reports: &[Report], found: &mut Vec<Violation>) {
    let flagged = |id: &str| {
        reports
            .iter()
            .filter(|report| report.checker == "K-9")
            .flat_map(|report| &report.violations)
            .any(|v| v.kind == Kind::WriteUnresolved && v.request_id == id)
    };
    for (id, resolved) in &view.writes {
        let accounted = match resolved {
            Some(Resolved::Executed | Resolved::NotExecuted | Resolved::Refused) => true,
            Some(Resolved::Unresolved) | None => flagged(id),
        };
        if !accounted {
            found.push(violation(
                Kind::WriteUnaccounted,
                id,
                format!(
                    "rule: every admitted write resolves or is a liveness failure (T-13); \
                     resolution {resolved:?} and K-9 reported nothing"
                ),
            ));
        }
    }
}

fn executed(reports: &[Report], found: &mut Vec<Violation>) {
    for checker in APPLICABLE {
        if !reports
            .iter()
            .any(|report| report.checker == checker && report.examined > 0)
        {
            found.push(violation(
                Kind::CheckerNotExecuted,
                "",
                format!(
                    "rule: every applicable checker ran and examined a case (T-13); \
                     {checker} has no report, or it examined nothing"
                ),
            ));
        }
    }
}

fn completeness(view: &Vacuity, found: &mut Vec<Violation>) {
    if !view.complete {
        found.push(violation(
            Kind::RunIncomplete,
            "",
            "rule: a run the document marks incomplete is incomplete (P10); complete is false"
                .to_owned(),
        ));
    }
    if view.budget_exhausted {
        found.push(violation(
            Kind::BudgetExhausted,
            "",
            "rule: the event budget is intact (T-13); it was exhausted".to_owned(),
        ));
    }
    if !view.overflowed.is_empty() {
        found.push(violation(
            Kind::ObservationOverflowed,
            "",
            format!(
                "rule: required observations stay within their bounds (CD-9); overflowed: {:?}",
                view.overflowed
            ),
        ));
    }
    if view.scenario.0 != "valid" {
        found.push(violation(
            Kind::ScenarioInvalid,
            "",
            format!(
                "rule: scenario invalid is incomplete (D-73); status `{}`, violated {:?}",
                view.scenario.0, view.scenario.1
            ),
        ));
    }
}

/// The run's verdicts: [`Verdict::Fail`] if any checker found a violation, else
/// [`Verdict::Incomplete`] if K-13 did, else [`Verdict::Pass`]. `reports` holds the checkers'
/// reports; `non_vacuity` is [`k13`]'s.
#[must_use]
pub fn judge(reports: &[Report], non_vacuity: &Report) -> Judgement {
    let mut checkers: Vec<(&'static str, Verdict)> = reports
        .iter()
        .map(|report| {
            let verdict = if report.passed() {
                Verdict::Pass
            } else {
                Verdict::Fail
            };
            (report.checker, verdict)
        })
        .collect();
    let vacuous = if non_vacuity.passed() {
        Verdict::Pass
    } else {
        Verdict::Incomplete
    };
    checkers.push((non_vacuity.checker, vacuous));
    let overall = if checkers.iter().any(|(_, v)| *v == Verdict::Fail) {
        Verdict::Fail
    } else {
        vacuous
    };
    Judgement { overall, checkers }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const RUN: &str = include_str!("../../../schemas/observation/fixtures/run.v1.json");
    const DRAIN: &str = include_str!("../../../schemas/observation/fixtures/drain.v1.json");

    fn doc(text: &str) -> Value {
        serde_json::from_str(text).expect("fixture is JSON")
    }

    fn records(doc: &mut Value) -> &mut Vec<Value> {
        doc["records"].as_array_mut().expect("records")
    }

    fn reports() -> Vec<Report> {
        APPLICABLE
            .iter()
            .map(|checker| Report {
                checker,
                examined: 1,
                violations: Vec::new(),
            })
            .collect()
    }

    fn judged(doc: &Value, reports: &[Report]) -> Report {
        let view = Vacuity::from_value(doc).expect("view");
        k13(&view, reports)
    }

    fn kinds(report: &Report) -> Vec<Kind> {
        report.violations.iter().map(|v| v.kind).collect()
    }

    fn assert_only(report: &Report, kind: Kind, rule: &str) {
        assert_eq!(kinds(report), vec![kind], "rule: {rule}; {report:?}");
    }

    /// The defect `kind` alone, beside the `RunIncomplete` that `complete: false` adds.
    fn assert_with_incomplete(report: &Report, kind: Kind, rule: &str) {
        assert_eq!(
            kinds(report),
            vec![Kind::RunIncomplete, kind],
            "rule: {rule}; {report:?}"
        );
    }

    fn at(reports: &[Report], checker: &str) -> usize {
        reports
            .iter()
            .position(|report| report.checker == checker)
            .expect("checker in APPLICABLE")
    }

    // check: K-13
    #[test]
    fn a_run_that_exercised_everything_passes() {
        let report = judged(&doc(RUN), &reports());
        assert!(
            report.passed(),
            "rule: the full run is non-vacuous; {report:?}"
        );
        let judgement = judge(&reports(), &report);
        assert_eq!(
            judgement.overall,
            Verdict::Pass,
            "rule: all pass; {judgement:?}"
        );
    }

    #[test]
    fn a_drain_run_without_node_reads_is_incomplete() {
        let report = judged(&doc(DRAIN), &reports());
        assert_only(
            &report,
            Kind::ReadsFromTooFewNodes,
            "status lookups are not reads",
        );
    }

    #[test]
    fn an_all_timeout_run_fails() {
        let mut d = doc(RUN);
        for record in records(&mut d) {
            if record["kind"] == "response"
                && ["put", "delete", "cas"].contains(&record["op"].as_str().unwrap_or(""))
            {
                record["outcome"] = json!({"code": "unknown_outcome"});
            }
        }
        let report = judged(&d, &reports());
        assert!(
            report.has(Kind::NoPreFaultWrite),
            "rule: all-timeout fails; {report:?}"
        );
        let judgement = judge(&reports(), &report);
        assert!(
            !judgement.overall.passes(),
            "rule: incomplete fails (P10); {judgement:?}"
        );
    }

    #[test]
    fn a_selected_fault_never_injected_is_incomplete() {
        let mut d = doc(RUN);
        records(&mut d).retain(|r| !(r["kind"] == "fault" && r["fault"] == "stall"));
        let report = judged(&d, &reports());
        assert_only(
            &report,
            Kind::FaultNotInjected,
            "an unexercised selected fault",
        );
        assert_eq!(judge(&reports(), &report).overall, Verdict::Incomplete);
    }

    #[test]
    fn a_write_only_after_the_first_fault_is_incomplete() {
        let mut d = doc(RUN);
        let first = records(&mut d)
            .iter()
            .position(|r| r["kind"] == "fault")
            .expect("fault");
        for record in &mut records(&mut d)[..first] {
            if record["kind"] == "response" && record["op"] == "put" {
                record["outcome"] = json!({"code": "busy"});
            }
        }
        let report = judged(&d, &reports());
        assert_only(
            &report,
            Kind::NoPreFaultWrite,
            "writes before the first fault",
        );
    }

    #[test]
    fn reads_from_one_node_are_incomplete() {
        let mut d = doc(RUN);
        for record in records(&mut d) {
            if record["kind"] == "response"
                && ["get", "scan"].contains(&record["op"].as_str().unwrap_or(""))
                && record["outcome"]["served_by"].is_object()
            {
                record["outcome"]["served_by"]["node_id"] = json!(1);
            }
        }
        let report = judged(&d, &reports());
        assert_only(
            &report,
            Kind::ReadsFromTooFewNodes,
            "five voters need two servers",
        );
    }

    #[test]
    fn a_three_voter_run_needs_reads_from_every_voter() {
        let mut d = doc(RUN);
        d["voters"] = json!([1, 2, 3]);
        for record in records(&mut d) {
            if record["kind"] == "response"
                && ["get", "scan"].contains(&record["op"].as_str().unwrap_or(""))
                && record["outcome"]["served_by"]["node_id"] == 3
            {
                record["outcome"]["served_by"]["node_id"] = json!(2);
            }
        }
        let report = judged(&d, &reports());
        assert_only(
            &report,
            Kind::ReadsFromTooFewNodes,
            "voter 3 served no read",
        );
        let detail = &report.violations[0].detail;
        assert!(
            detail.contains("voters [3]"),
            "detail names voter 3: {detail}"
        );
    }

    #[test]
    fn a_heal_with_no_marker_is_incomplete() {
        let mut d = doc(RUN);
        records(&mut d).retain(|r| r["kind"] != "marker");
        let report = judged(&d, &reports());
        assert_only(&report, Kind::NoPostHealWrite, "no marker after heal");
    }

    #[test]
    fn a_drain_heal_with_no_drain_is_incomplete() {
        let mut d = doc(DRAIN);
        records(&mut d).retain(|r| r["kind"] != "drain");
        let report = judged(&d, &reports());
        assert!(
            report.has(Kind::NoPostHealWrite),
            "rule: a drain heal needs its drain; {report:?}"
        );
    }

    #[test]
    fn an_unresolved_write_needs_k9_to_report_it() {
        let mut d = doc(RUN);
        let id = {
            let record = records(&mut d)
                .iter_mut()
                .find(|r| r["kind"] == "resolution")
                .expect("resolution");
            record["resolved"] = json!("unresolved");
            record["evidence"] = json!([]);
            record["request_id"].as_str().expect("id").to_owned()
        };
        let report = judged(&d, &reports());
        assert_only(&report, Kind::WriteUnaccounted, "unresolved and unreported");
        let mut with_k9 = reports();
        with_k9[at(&reports(), "K-9")].violations.push(Violation {
            kind: Kind::WriteUnresolved,
            request_id: id,
            attempt: None,
            detail: "unresolved".to_owned(),
        });
        let report = judged(&d, &with_k9);
        assert!(
            report.passed(),
            "rule: K-9's liveness failure accounts for it; {report:?}"
        );
        assert_eq!(
            judge(&with_k9, &report).overall,
            Verdict::Fail,
            "rule: K-9's finding fails"
        );
    }

    #[test]
    fn a_write_without_a_resolution_is_unaccounted() {
        let mut d = doc(RUN);
        let mut dropped = false;
        records(&mut d).retain(|r| {
            let drop = r["kind"] == "resolution" && !dropped;
            dropped |= drop;
            !drop
        });
        let report = judged(&d, &reports());
        assert_only(&report, Kind::WriteUnaccounted, "no resolution record");
    }

    #[test]
    fn a_checker_that_did_not_run_is_incomplete() {
        let mut ran = reports();
        let missing = ran.remove(at(&reports(), "K-6")).checker;
        let report = judged(&doc(RUN), &ran);
        assert_only(&report, Kind::CheckerNotExecuted, "K-6 has no report");
        assert!(
            report.violations[0].detail.contains(missing),
            "names {missing}"
        );
    }

    #[test]
    fn an_exhausted_budget_fails() {
        let mut d = doc(RUN);
        d["complete"] = json!(false);
        d["event_budget"]["exhausted"] = json!(true);
        let report = judged(&d, &reports());
        assert_with_incomplete(&report, Kind::BudgetExhausted, "budget exhaustion");
        assert!(
            !judge(&reports(), &report).overall.passes(),
            "rule: incomplete fails (P10)"
        );
    }

    #[test]
    fn an_overflow_is_incomplete() {
        let mut d = doc(RUN);
        d["complete"] = json!(false);
        d["overflowed"] = json!(["records"]);
        let report = judged(&d, &reports());
        assert_with_incomplete(
            &report,
            Kind::ObservationOverflowed,
            "an overflowed observation",
        );
    }

    #[test]
    fn an_invalid_scenario_is_incomplete() {
        let mut d = doc(RUN);
        d["complete"] = json!(false);
        d["scenario"] = json!({"status": "invalid", "violated": "too many crashes"});
        let report = judged(&d, &reports());
        assert_with_incomplete(
            &report,
            Kind::ScenarioInvalid,
            "scenario invalid maps to incomplete (D-73)",
        );
        assert_eq!(judge(&reports(), &report).overall, Verdict::Incomplete);
    }

    #[test]
    fn a_checker_failure_outranks_incompleteness() {
        let mut ran = reports();
        let k1 = at(&ran, "K-1");
        ran[k1].violations.push(Violation {
            kind: Kind::MissingOnNode,
            request_id: String::new(),
            attempt: None,
            detail: "x".to_owned(),
        });
        let mut d = doc(RUN);
        d["complete"] = json!(false);
        d["event_budget"]["exhausted"] = json!(true);
        let report = judged(&d, &ran);
        let judgement = judge(&ran, &report);
        assert_eq!(
            judgement.overall,
            Verdict::Fail,
            "rule: a finding is a fail; {judgement:?}"
        );
        assert_eq!(
            judgement.checkers.last(),
            Some(&("K-13", Verdict::Incomplete))
        );
    }

    #[test]
    fn a_run_marked_incomplete_is_incomplete() {
        let mut d = doc(RUN);
        d["complete"] = json!(false);
        let report = judged(&d, &reports());
        assert_only(&report, Kind::RunIncomplete, "complete: false alone");
    }

    #[test]
    fn a_run_with_no_voters_is_incomplete() {
        let mut d = doc(RUN);
        d["voters"] = json!([]);
        let report = judged(&d, &reports());
        assert!(
            report.has(Kind::ReadsFromTooFewNodes),
            "rule: no voters, no reads; {report:?}"
        );
    }

    #[test]
    fn a_checker_that_examined_nothing_did_not_run() {
        let mut ran = reports();
        let k8 = at(&ran, "K-8");
        ran[k8].examined = 0;
        let report = judged(&doc(RUN), &ran);
        assert_only(&report, Kind::CheckerNotExecuted, "K-8 examined zero cases");
    }

    #[test]
    fn a_write_that_succeeds_only_in_setup_is_not_a_pre_fault_write() {
        let mut d = doc(RUN);
        let baseline = records(&mut d)
            .iter()
            .position(|r| r["kind"] == "phase" && r["phase"] == "baseline")
            .expect("baseline");
        for record in &mut records(&mut d)[baseline..] {
            if record["kind"] == "response" && record["op"] == "put" {
                record["outcome"] = json!({"code": "busy"});
            }
        }
        let report = judged(&d, &reports());
        assert!(
            report.has(Kind::NoPreFaultWrite),
            "rule: setup writes do not count; {report:?}"
        );
    }

    #[test]
    fn a_malformed_document_is_an_error() {
        let mut d = doc(RUN);
        d.as_object_mut().expect("object").remove("scenario");
        assert!(
            Vacuity::from_value(&d).is_err(),
            "rule: unreadable is never a pass (P10)"
        );
    }
}
