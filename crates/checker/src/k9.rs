//! K-9, liveness within the healthy envelope (PLAN.md §10.5, §9.4; A-1, A-3, A-4): after the
//! heal every outstanding logical write reaches a terminal resolution, and the heal completes
//! within its budget.
//!
//! - **Resolution.** Every logical write (a request ID some write invocation carried) has a
//!   resolution, and none is `unresolved` ([`Kind::WriteUnresolved`]). K-12 judges that each has
//!   exactly one and that the evidence is honest; here a missing or `unresolved` one is the
//!   liveness failure.
//! - **Completion.** In a marker heal every recoverable node applies the marker, and the marker
//!   is committed, no later than the heal phase's start plus `heal_budget_ticks` × `tick_us`
//!   ([`Kind::HealIncomplete`], [`Kind::HealLate`]). In a drain heal the certified drain is
//!   recorded within that budget. A skipped catch-up (B-18) is a node that never applies it.
//! - **Quiescence.** Client activity stops ([`Kind::NotQuiescent`]): a `quiescence` record
//!   exists and nothing but status lookups is invoked after it.
//! - **Absence is certified.** A write resolved `not_executed` rests on status lookups that
//!   answered after the barrier (the certified drain, or quiescence in a marker heal) from
//!   every recoverable node ([`Kind::AbsenceNotCertified`]); one node's `unknown` proves
//!   nothing.
//!
//! The unit examined is one logical write plus the heal's completion. Limits: a status lookup's
//! answer (`unknown`) is judged by K-12, not here, and the checker does not re-derive whether
//! the budget was a fair one: [`Heal::deadline`] is the manifest's.

use crate::heal::{Completion, Heal};
use crate::report::{CheckError, Kind, Report, Violation};
use crate::trace::Resolved;

/// Judges `heal` against K-9.
///
/// # Errors
///
/// [`CheckError::Malformed`] if the run has no `heal` phase record: the budget has no start.
pub fn k9(heal: &Heal) -> Result<Report, CheckError> {
    let deadline = heal
        .deadline()
        .ok_or_else(|| CheckError::malformed("records", "no `heal` phase record"))?;
    let mut found = Vec::new();
    resolutions(heal, deadline, &mut found);
    completion(heal, deadline, &mut found);
    quiescence(heal, &mut found);
    absence(heal, &mut found);
    Ok(Report {
        checker: "K-9",
        examined: heal.writes.len() as u64 + 1,
        violations: found,
    })
}

fn violation(kind: Kind, request_id: &str, attempt: Option<u64>, detail: String) -> Violation {
    Violation {
        kind,
        request_id: request_id.to_owned(),
        attempt,
        detail,
    }
}

fn resolutions(heal: &Heal, deadline: u64, found: &mut Vec<Violation>) {
    for id in heal.writes.keys() {
        let mine: Vec<_> = heal
            .resolutions
            .iter()
            .filter(|resolution| &resolution.request_id == id)
            .collect();
        if mine.is_empty() {
            found.push(violation(
                Kind::WriteUnresolved,
                id,
                None,
                "rule: K-9 every outstanding logical write reaches a terminal resolution; this \
                 one has no resolution"
                    .to_owned(),
            ));
        }
        for resolution in mine {
            if resolution.time > deadline {
                found.push(violation(
                    Kind::HealLate,
                    id,
                    None,
                    format!(
                        "rule: K-9 every outstanding logical write resolves within the heal \
                         budget; records[{}] resolves it at time {} after the deadline {deadline}",
                        resolution.position, resolution.time
                    ),
                ));
            }
            if resolution.resolved == Resolved::Unresolved {
                found.push(violation(
                    Kind::WriteUnresolved,
                    id,
                    None,
                    format!(
                        "rule: K-9 every outstanding logical write reaches a terminal \
                         resolution; records[{}] leaves it unresolved",
                        resolution.position
                    ),
                ));
            }
        }
    }
}

fn completion(heal: &Heal, deadline: u64, found: &mut Vec<Violation>) {
    let none = String::new();
    match heal.completion {
        Completion::Marker | Completion::PrecreatedMarker => {
            for node in heal.recoverable() {
                if !heal.markers.iter().any(|marker| marker.node == node) {
                    found.push(violation(
                        Kind::HealIncomplete,
                        &none,
                        None,
                        format!(
                            "rule: K-9 the run waits until every recoverable node applied the \
                             catch-up marker; node {node} never did (B-18)"
                        ),
                    ));
                }
            }
            for marker in heal
                .markers
                .iter()
                .filter(|marker| heal.recoverable().any(|node| node == marker.node))
            {
                if marker.time > deadline {
                    found.push(violation(
                        Kind::HealLate,
                        &none,
                        None,
                        format!(
                            "rule: K-9 the marker is applied within the heal budget; node {} \
                             applied it at time {} after the deadline {deadline} \
                             (records[{}])",
                            marker.node, marker.time, marker.position
                        ),
                    ));
                }
                match heal.committed_at.get(&marker.index) {
                    None => found.push(violation(
                        Kind::HealIncomplete,
                        &none,
                        None,
                        format!(
                            "rule: K-9 the catch-up marker commits; index {} applied by node \
                             {} was never recorded committed (records[{}])",
                            marker.index, marker.node, marker.position
                        ),
                    )),
                    Some(&time) if time > deadline => found.push(violation(
                        Kind::HealLate,
                        &none,
                        None,
                        format!(
                            "rule: K-9 the marker commits within the heal budget; index {} \
                             committed at time {time} after the deadline {deadline}",
                            marker.index
                        ),
                    )),
                    Some(_) => {}
                }
            }
        }
        Completion::Drain => match &heal.drain {
            None => found.push(violation(
                Kind::HealIncomplete,
                &none,
                None,
                "rule: K-9 a drain heal certifies its drain within the budget; none was \
                 recorded"
                    .to_owned(),
            )),
            Some(drain) if drain.time > deadline => found.push(violation(
                Kind::HealLate,
                &none,
                None,
                format!(
                    "rule: K-9 the drain is certified within the heal budget; it was recorded \
                     at time {} after the deadline {deadline} (records[{}])",
                    drain.time, drain.position
                ),
            )),
            Some(_) => {}
        },
    }
}

fn quiescence(heal: &Heal, found: &mut Vec<Violation>) {
    if heal.quiescence.is_none() {
        found.push(violation(
            Kind::NotQuiescent,
            "",
            None,
            "rule: K-9 client activity stops before the heal is judged; no `quiescence` \
             record"
                .to_owned(),
        ));
    }
    for (position, attempt) in &heal.late_invocations {
        found.push(violation(
            Kind::NotQuiescent,
            "",
            Some(*attempt),
            format!(
                "rule: K-9 after quiescence only status lookups are invoked; records[{position}] \
                 invokes attempt {attempt}, which is not one"
            ),
        ));
    }
}

fn absence(heal: &Heal, found: &mut Vec<Violation>) {
    let barrier = match heal.completion {
        Completion::Drain => heal.drain.as_ref().map(|drain| drain.position),
        Completion::Marker | Completion::PrecreatedMarker => {
            heal.quiescence.map(|(position, _)| position)
        }
    };
    for resolution in &heal.resolutions {
        if resolution.resolved != Resolved::NotExecuted {
            continue;
        }
        let Some(barrier) = barrier else {
            found.push(violation(
                Kind::AbsenceNotCertified,
                &resolution.request_id,
                None,
                format!(
                    "rule: K-9 absence is proven only after the drain or quiescence; \
                     records[{}] resolves the write `not_executed` but the run has no such \
                     barrier",
                    resolution.position
                ),
            ));
            continue;
        };
        let answered: Vec<(u64, u64)> = resolution
            .evidence
            .iter()
            .filter_map(|attempt| heal.responses.get(attempt).map(|r| (r.node, r.position)))
            .collect();
        for node in heal.recoverable() {
            let after = answered
                .iter()
                .any(|(answered_by, position)| *answered_by == node && *position > barrier);
            if !after {
                found.push(violation(
                    Kind::AbsenceNotCertified,
                    &resolution.request_id,
                    None,
                    format!(
                        "rule: K-9 `not_executed` rests on a status lookup answered by every \
                         recoverable node after records[{barrier}]; records[{}] has none from \
                         node {node} (answered (node, position) = {answered:?})",
                        resolution.position
                    ),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // check: K-9
    use super::k9;
    use crate::heal_fixtures::Run;
    use crate::report::{CheckError, Kind};

    #[test]
    fn a_healthy_marker_run_passes() {
        let report = k9(&Run::marker().heal()).unwrap();
        assert!(report.passed(), "rule: K-9 pass; {report:#?}");
        assert_eq!(
            report.examined, 2,
            "one write plus the completion: {report:?}"
        );
    }

    #[test]
    fn a_healthy_drain_run_passes() {
        let report = k9(&Run::drain().heal()).unwrap();
        assert!(report.passed(), "rule: K-9 drain pass; {report:#?}");
    }

    #[test]
    fn an_unresolved_write_fails() {
        let report = k9(&Run::marker_unresolved().heal()).unwrap();
        assert_eq!(
            report.violations.len(),
            1,
            "rule: K-9 a liveness timeout leaves the write unresolved; {report:#?}"
        );
        assert!(report.has(Kind::WriteUnresolved), "{report:#?}");
        assert_eq!(
            report.violations[0].request_id,
            Run::WRITE,
            "names the write: {report:#?}"
        );
    }

    #[test]
    fn a_write_with_no_resolution_fails() {
        let report = k9(&Run::marker_unrecorded().heal()).unwrap();
        assert!(
            report.has(Kind::WriteUnresolved),
            "rule: K-9 every write is resolved; {report:#?}"
        );
    }

    #[test]
    fn a_skipped_catch_up_fails() {
        let report = k9(&Run::marker_without(&[4]).heal()).unwrap();
        assert_eq!(
            report.violations.len(),
            1,
            "rule: K-9 node 4 never applied the marker (B-18); {report:#?}"
        );
        assert!(report.has(Kind::HealIncomplete), "{report:#?}");
    }

    #[test]
    fn a_lost_node_need_not_apply_the_marker() {
        let mut run = Run::marker_without(&[5]);
        run.lose(5);
        let report = k9(&run.heal()).unwrap();
        assert!(
            report.passed(),
            "rule: K-9 excluded nodes owe nothing; {report:#?}"
        );
    }

    #[test]
    fn a_marker_applied_after_the_budget_fails() {
        let mut run = Run::marker();
        run.marker_late(2, 0, 3, "d");
        let report = k9(&run.heal()).unwrap();
        assert!(
            report.has(Kind::HealLate),
            "rule: K-9 the marker applies within the 30-tick budget; {report:#?}"
        );
    }

    #[test]
    fn a_marker_applied_exactly_at_the_deadline_passes() {
        let report = k9(&Run::marker_applied_at_deadline().heal()).unwrap();
        assert!(
            report.passed(),
            "rule: K-9 the budget is inclusive; {report:#?}"
        );
    }

    #[test]
    fn a_marker_committed_after_the_budget_fails() {
        let report = k9(&Run::marker_committed_late().heal()).unwrap();
        assert!(
            report.has(Kind::HealLate),
            "rule: K-9 the marker commits within the budget; {report:#?}"
        );
    }

    #[test]
    fn a_marker_that_never_committed_fails() {
        let report = k9(&Run::marker_uncommitted().heal()).unwrap();
        assert!(
            report.has(Kind::HealIncomplete),
            "rule: K-9 the marker commits; {report:#?}"
        );
    }

    #[test]
    fn a_drain_after_the_budget_fails() {
        let report = k9(&Run::drain_late().heal()).unwrap();
        assert!(
            report.has(Kind::HealLate),
            "rule: K-9 the drain is certified within the budget; {report:#?}"
        );
    }

    #[test]
    fn a_drain_heal_without_a_drain_is_incomplete() {
        let report = k9(&Run::drain_none().heal()).unwrap();
        assert!(report.has(Kind::HealIncomplete), "{report:#?}");
    }

    #[test]
    fn a_run_that_never_quiesced_fails() {
        let report = k9(&Run::marker_without_quiescence().heal()).unwrap();
        assert!(
            report.has(Kind::NotQuiescent),
            "rule: K-9 client activity stops; {report:#?}"
        );
    }

    #[test]
    fn a_write_invoked_after_quiescence_fails() {
        let mut run = Run::marker();
        run.late_put(40, Run::OTHER);
        let report = k9(&run.heal()).unwrap();
        assert!(
            report.has(Kind::NotQuiescent),
            "rule: K-9 only status lookups follow quiescence; {report:#?}"
        );
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.kind == Kind::NotQuiescent && v.attempt == Some(40)),
            "names attempt 40: {report:#?}"
        );
    }

    #[test]
    fn a_status_lookup_after_quiescence_is_allowed() {
        let mut run = Run::marker();
        run.late_status(41, Run::WRITE);
        let report = k9(&run.heal()).unwrap();
        assert!(
            report.passed(),
            "rule: K-9 status lookups pass; {report:#?}"
        );
    }

    #[test]
    fn an_absence_proven_by_every_recoverable_node_after_the_drain_passes() {
        let report = k9(&Run::drain_with_absence(&[1, 2, 3, 4, 5], true).heal()).unwrap();
        assert!(report.passed(), "rule: K-9 certified absence; {report:#?}");
    }

    #[test]
    fn an_absence_missing_a_recoverable_node_fails() {
        let report = k9(&Run::drain_with_absence(&[1, 2, 3, 4], true).heal()).unwrap();
        assert_eq!(
            report.violations.len(),
            1,
            "rule: K-9 node 5 never answered the lookup; {report:#?}"
        );
        assert!(report.has(Kind::AbsenceNotCertified), "{report:#?}");
    }

    #[test]
    fn an_absence_proven_before_the_drain_fails() {
        let report = k9(&Run::drain_with_absence(&[1, 2, 3, 4, 5], false).heal()).unwrap();
        assert!(
            report.has(Kind::AbsenceNotCertified),
            "rule: K-9 the lookups must follow the drain; {report:#?}"
        );
    }

    #[test]
    fn an_absence_need_not_hear_from_a_lost_node() {
        let mut run = Run::drain_with_absence(&[1, 2, 3, 4], true);
        run.lose(5);
        let report = k9(&run.heal()).unwrap();
        assert!(
            report.passed(),
            "rule: K-9 excluded nodes owe no lookup; {report:#?}"
        );
    }

    #[test]
    fn an_absence_with_no_evidence_fails() {
        let report = k9(&Run::drain_with_absence(&[], true).heal()).unwrap();
        assert!(
            report.has(Kind::AbsenceNotCertified),
            "rule: K-9 no lookup proves nothing; {report:#?}"
        );
    }

    #[test]
    fn a_write_resolved_after_the_deadline_fails() {
        let report = k9(&Run::marker_resolved_late().heal()).unwrap();
        assert_eq!(
            report.violations.len(),
            1,
            "rule: K-9 a write that settles after the heal budget is a liveness failure; \
             {report:#?}"
        );
        assert!(report.has(Kind::HealLate), "{report:#?}");
    }

    #[test]
    fn a_run_without_a_heal_phase_cannot_be_judged() {
        let error = k9(&Run::marker_without_heal_phase().heal()).unwrap_err();
        let CheckError::Malformed { .. } = error else {
            panic!("rule: K-9 the budget needs the heal's start, never a silent pass; {error:?}");
        };
    }
}
