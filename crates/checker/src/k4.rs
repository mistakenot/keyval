//! K-4, model-replay results (PLAN.md §10.5; C-1, C-5, F-1, F-3, F-5, DM-3, DM-6, F-6):
//! replaying the committed log through the reference model reproduces every reported write
//! result: the index, the version (DM-3), `existed` (F-3), a CAS mismatch with the correct
//! current version (F-5) and capacity outcomes (DM-6). A `request_id_conflict` names the
//! original execution's index and answers only a conflicting reuse (F-6).
//!
//! Expected results come from the model's replay, never from a node (§10.14). Misuse attempts
//! are K-2's. The unit examined is one terminal execution response or one conflict answer.

use std::collections::BTreeMap;

use model::{Decision, RequestId, TerminalResult};

use crate::history::History;
use crate::report::{CheckError, Kind, Report, Violation};

/// Judges `history` against K-4.
///
/// # Errors
///
/// [`CheckError::Replay`] if the committed entries cannot be replayed.
pub fn k4(history: &History) -> Result<Report, CheckError> {
    let first = history.first_executions()?;
    let mut violations = Vec::new();
    let mut examined = 0;
    for claim in &history.claims {
        if history
            .invoked
            .get(&claim.attempt)
            .is_some_and(|invoked| invoked.misuse)
        {
            continue;
        }
        examined += 1;
        let reported = claim.result;
        let violation = |kind, detail: String| Violation {
            kind,
            request_id: reported.request_id.to_string(),
            attempt: Some(claim.attempt),
            detail,
        };
        let Some(expected) = first.get(&reported.request_id) else {
            violations.push(violation(
                Kind::ReportedWithoutCommand,
                format!(
                    "rule: K-4 a reported result is the model's replay of a committed command; \
                     reported {reported:?}, but no committed command has this request ID"
                ),
            ));
            continue;
        };
        if expected.index != reported.index {
            violations.push(violation(
                Kind::ReportedIndexWrong,
                format!(
                    "rule: K-4 the reported index is the first execution's; expected {}, \
                     reported {}",
                    expected.index.get(),
                    reported.index.get()
                ),
            ));
        }
        if let Some(kind) = differs(expected.decision, reported.decision) {
            violations.push(violation(
                kind,
                format!(
                    "rule: K-4 the reported decision is the model's; expected {:?}, \
                     reported {:?}",
                    expected.decision, reported.decision
                ),
            ));
        }
    }
    examined += history.conflicts.len() as u64;
    conflicts(history, &first, &mut violations);
    Ok(Report {
        checker: "K-4",
        examined,
        violations,
    })
}

/// F-6: each `request_id_conflict` answers a conflicting reuse and names the original index.
fn conflicts(
    history: &History,
    first: &BTreeMap<RequestId, TerminalResult>,
    violations: &mut Vec<Violation>,
) {
    for conflict in &history.conflicts {
        let violation = |kind, detail: String| Violation {
            kind,
            request_id: conflict.request_id.to_string(),
            attempt: Some(conflict.attempt),
            detail,
        };
        if !history
            .invoked
            .get(&conflict.attempt)
            .is_some_and(|invoked| invoked.misuse)
        {
            violations.push(violation(
                Kind::ConflictWithoutMisuse,
                format!(
                    "rule: K-4 request_id_conflict answers only a conflicting reuse; attempt {} \
                     is not one",
                    conflict.attempt
                ),
            ));
        }
        match first.get(&conflict.request_id) {
            None => violations.push(violation(
                Kind::ConflictWithoutOriginal,
                format!(
                    "rule: K-4 a conflict needs an original execution; reported original index \
                     {}, but no committed command has this request ID",
                    conflict.original_index.get()
                ),
            )),
            Some(original) if original.index != conflict.original_index => {
                violations.push(violation(
                    Kind::ConflictWrongOriginal,
                    format!(
                        "rule: K-4 a conflict names the first execution's index; expected {}, \
                         reported {}",
                        original.index.get(),
                        conflict.original_index.get()
                    ),
                ));
            }
            Some(_) => {}
        }
    }
}

/// How a reported decision differs from the model's, or `None` if it does not.
pub fn differs(expected: Decision, reported: Decision) -> Option<Kind> {
    match (expected, reported) {
        (Decision::Set { version: a }, Decision::Set { version: b }) => {
            (a != b).then_some(Kind::ReportedVersionWrong)
        }
        (Decision::Deleted { existed: a }, Decision::Deleted { existed: b }) => {
            (a != b).then_some(Kind::ReportedExistedWrong)
        }
        (Decision::VersionMismatch { current: a }, Decision::VersionMismatch { current: b }) => {
            (a != b).then_some(Kind::ReportedCurrentVersionWrong)
        }
        (Decision::CapacityExceeded { limit: a }, Decision::CapacityExceeded { limit: b }) => {
            (a != b).then_some(Kind::ReportedCapacityWrong)
        }
        (Decision::Set { .. }, Decision::VersionMismatch { .. })
        | (Decision::VersionMismatch { .. }, Decision::Set { .. }) => {
            Some(Kind::ReportedCasOutcomeWrong)
        }
        (Decision::CapacityExceeded { .. }, Decision::Set { .. })
        | (Decision::CapacityExceeded { .. }, Decision::Deleted { .. })
        | (Decision::CapacityExceeded { .. }, Decision::VersionMismatch { .. })
        | (Decision::Set { .. }, Decision::CapacityExceeded { .. })
        | (Decision::Deleted { .. }, Decision::CapacityExceeded { .. })
        | (Decision::VersionMismatch { .. }, Decision::CapacityExceeded { .. }) => {
            Some(Kind::ReportedCapacityWrong)
        }
        (Decision::Set { .. }, Decision::Deleted { .. })
        | (Decision::Deleted { .. }, Decision::Set { .. })
        | (Decision::Deleted { .. }, Decision::VersionMismatch { .. })
        | (Decision::VersionMismatch { .. }, Decision::Deleted { .. }) => {
            Some(Kind::ReportedDecisionWrong)
        }
    }
}

#[cfg(test)]
mod tests {
    // check: K-4
    use super::k4;
    use crate::fixtures::{
        A, B, Doc, capacity_exceeded, cas, conflict, delete, ok_delete, ok_put, put,
        version_mismatch, version_mismatch_absent,
    };
    use crate::report::Kind;

    const P2: &str = "02a1";
    const P3: &str = "03a1";

    /// A at index 2 puts key `k` (version 2).
    fn put_a() -> Doc {
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, P2, put(A, "k", "v"));
        doc
    }

    #[test]
    fn a_correct_put_and_its_replay_pass() {
        let mut doc = put_a();
        doc.invoke(1, A, false).put_ok(1, A, 2, false);
        doc.invoke(2, A, false).put_ok(2, A, 2, true);
        let report = k4(&doc.history()).unwrap();
        assert!(report.passed(), "rule: K-4 pass; {report:#?}");
        assert_eq!(report.examined, 2, "two results examined: {report:?}");
    }

    #[test]
    fn a_wrong_version_fails() {
        let mut doc = put_a();
        doc.invoke(1, A, false)
            .respond(1, "put", ok_put(A, 2, 7, false))
            .deliver(1);
        let report = k4(&doc.history()).unwrap();
        assert!(
            report.has(Kind::ReportedVersionWrong),
            "rule: DM-3 version = execution index (2), reported 7: {report:#?}"
        );
    }

    #[test]
    fn a_wrong_index_fails() {
        let mut doc = put_a();
        doc.invoke(1, A, false).put_ok(1, A, 3, false);
        let report = k4(&doc.history()).unwrap();
        assert!(
            report.has(Kind::ReportedIndexWrong),
            "rule: K-4 the executed index is 2, reported 3: {report:#?}"
        );
    }

    #[test]
    fn a_result_without_a_command_fails() {
        let mut doc = put_a();
        doc.invoke(1, B, false).put_ok(1, B, 2, false);
        let report = k4(&doc.history()).unwrap();
        assert!(
            report.has(Kind::ReportedWithoutCommand),
            "rule: K-4 B never committed: {report:#?}"
        );
    }

    #[test]
    fn a_status_answer_is_judged_like_a_response() {
        let mut doc = put_a();
        doc.status_known(1, A, 2, 2);
        assert!(
            k4(&doc.history()).unwrap().passed(),
            "a right status answer passes"
        );
        let mut doc = put_a();
        doc.status_known(1, A, 2, 9);
        assert!(
            k4(&doc.history()).unwrap().has(Kind::ReportedVersionWrong),
            "rule: DM-3 a status answer's version is judged too"
        );
    }

    #[test]
    fn delete_existed_is_judged() {
        let mut doc = put_a();
        doc.commit_everywhere(3, P3, delete(B, "k"));
        doc.invoke(1, B, false)
            .respond(1, "delete", ok_delete(B, 3, true, false))
            .deliver(1);
        assert!(
            k4(&doc.history()).unwrap().passed(),
            "existed=true is right"
        );
        let mut wrong = put_a();
        wrong.commit_everywhere(3, P3, delete(B, "k"));
        wrong
            .invoke(1, B, false)
            .respond(1, "delete", ok_delete(B, 3, false, false))
            .deliver(1);
        assert!(
            k4(&wrong.history())
                .unwrap()
                .has(Kind::ReportedExistedWrong),
            "rule: F-3 the key existed, reported existed=false"
        );
    }

    #[test]
    fn a_delete_reported_as_a_set_fails() {
        let mut doc = put_a();
        doc.commit_everywhere(3, P3, delete(B, "k"));
        doc.invoke(1, B, false)
            .respond(1, "delete", ok_put(B, 3, 3, false))
            .deliver(1);
        assert!(
            k4(&doc.history()).unwrap().has(Kind::ReportedDecisionWrong),
            "rule: K-4 a delete is not answered with a version"
        );
    }

    #[test]
    fn cas_mismatch_reports_the_current_version() {
        // The key holds version 2; B expects 5.
        let mut doc = put_a();
        doc.commit_everywhere(3, P3, cas(B, "k", 5, "w"));
        doc.invoke(1, B, false)
            .respond(1, "cas", version_mismatch(B, 3, 2, false))
            .deliver(1);
        assert!(
            k4(&doc.history()).unwrap().passed(),
            "mismatch with current 2 is right"
        );
        let mut wrong = put_a();
        wrong.commit_everywhere(3, P3, cas(B, "k", 5, "w"));
        wrong
            .invoke(1, B, false)
            .respond(1, "cas", version_mismatch(B, 3, 9, false))
            .deliver(1);
        assert!(
            k4(&wrong.history())
                .unwrap()
                .has(Kind::ReportedCurrentVersionWrong),
            "rule: F-5 the current version is 2, reported 9"
        );
    }

    #[test]
    fn cas_on_an_absent_key_reports_no_current_version() {
        let mut doc = put_a();
        doc.commit_everywhere(3, P3, cas(B, "j", 5, "w"));
        doc.invoke(1, B, false)
            .respond(1, "cas", version_mismatch_absent(B, 3))
            .deliver(1);
        assert!(
            k4(&doc.history()).unwrap().passed(),
            "absent key: current null is right"
        );
        let mut wrong = put_a();
        wrong.commit_everywhere(3, P3, cas(B, "j", 5, "w"));
        wrong
            .invoke(1, B, false)
            .respond(1, "cas", version_mismatch(B, 3, 2, false))
            .deliver(1);
        assert!(
            k4(&wrong.history())
                .unwrap()
                .has(Kind::ReportedCurrentVersionWrong),
            "rule: F-5 the key is absent, reported current 2"
        );
    }

    #[test]
    fn cas_success_against_a_mismatch_fails_both_ways() {
        let mut doc = put_a();
        doc.commit_everywhere(3, P3, cas(B, "k", 5, "w"));
        doc.invoke(1, B, false)
            .respond(1, "cas", ok_put(B, 3, 3, false))
            .deliver(1);
        assert!(
            k4(&doc.history())
                .unwrap()
                .has(Kind::ReportedCasOutcomeWrong),
            "rule: F-5 expected 5, key at 2: a success is wrong"
        );
        let mut matching = put_a();
        matching.commit_everywhere(3, P3, cas(B, "k", 2, "w"));
        matching
            .invoke(1, B, false)
            .respond(1, "cas", version_mismatch(B, 3, 2, false))
            .deliver(1);
        assert!(
            k4(&matching.history())
                .unwrap()
                .has(Kind::ReportedCasOutcomeWrong),
            "rule: F-5 expected 2, key at 2: a mismatch is wrong"
        );
    }

    #[test]
    fn capacity_outcomes_are_judged() {
        // One live key is allowed: B's new key exceeds it.
        let mut doc = Doc::with_noop();
        doc.max_live_keys(1)
            .commit_everywhere(2, P2, put(A, "k", "v"))
            .commit_everywhere(3, P3, put(B, "j", "w"));
        doc.invoke(1, B, false)
            .respond(1, "put", capacity_exceeded(B, 3, "max_live_keys"))
            .deliver(1);
        assert!(
            k4(&doc.history()).unwrap().passed(),
            "DM-6 capacity refusal is right"
        );
        let mut wrong = Doc::with_noop();
        wrong
            .max_live_keys(1)
            .commit_everywhere(2, P2, put(A, "k", "v"))
            .commit_everywhere(3, P3, put(B, "j", "w"));
        wrong.invoke(1, B, false).put_ok(1, B, 3, false);
        assert!(
            k4(&wrong.history())
                .unwrap()
                .has(Kind::ReportedCapacityWrong),
            "rule: DM-6 the model refused for capacity, reported a success"
        );
        let mut limit = Doc::with_noop();
        limit
            .max_live_keys(1)
            .commit_everywhere(2, P2, put(A, "k", "v"))
            .commit_everywhere(3, P3, put(B, "j", "w"));
        limit
            .invoke(1, B, false)
            .respond(1, "put", capacity_exceeded(B, 3, "max_live_bytes"))
            .deliver(1);
        assert!(
            k4(&limit.history())
                .unwrap()
                .has(Kind::ReportedCapacityWrong),
            "rule: DM-6 the limit hit is max_live_keys, reported max_live_bytes"
        );
    }

    #[test]
    fn a_conflict_naming_the_original_passes() {
        let mut doc = put_a();
        doc.invoke(1, A, false).put_ok(1, A, 2, false);
        doc.invoke(2, A, true)
            .respond(2, "put", conflict(A, 2))
            .deliver(2);
        let report = k4(&doc.history()).unwrap();
        assert!(
            report.passed(),
            "rule: F-6 conflict names index 2: {report:#?}"
        );
        assert_eq!(
            report.examined, 2,
            "one result and one conflict: {report:?}"
        );
    }

    #[test]
    fn a_conflict_naming_the_wrong_original_fails() {
        let mut doc = put_a();
        doc.invoke(1, A, true)
            .respond(1, "put", conflict(A, 3))
            .deliver(1);
        assert!(
            k4(&doc.history()).unwrap().has(Kind::ConflictWrongOriginal),
            "rule: F-6 the original executed at 2, conflict names 3"
        );
    }

    #[test]
    fn a_conflict_without_an_original_fails() {
        let mut doc = put_a();
        doc.invoke(1, B, true)
            .respond(1, "put", conflict(B, 2))
            .deliver(1);
        assert!(
            k4(&doc.history())
                .unwrap()
                .has(Kind::ConflictWithoutOriginal),
            "rule: F-6 B never executed"
        );
    }

    #[test]
    fn a_conflict_for_an_honest_attempt_fails() {
        let mut doc = put_a();
        doc.invoke(1, A, false)
            .respond(1, "put", conflict(A, 2))
            .deliver(1);
        assert!(
            k4(&doc.history()).unwrap().has(Kind::ConflictWithoutMisuse),
            "rule: F-6 attempt 1 is not a conflicting reuse"
        );
    }

    #[test]
    fn a_misuse_attempt_answered_as_an_execution_is_left_to_k2() {
        let mut doc = put_a();
        doc.invoke(1, A, true).put_ok(1, A, 5, false);
        let report = k4(&doc.history()).unwrap();
        assert!(report.passed() && report.examined == 0, "{report:#?}");
    }

    #[test]
    fn empty_history_passes_with_nothing_examined() {
        let report = k4(&Doc::with_noop().history()).unwrap();
        assert!(report.passed() && report.examined == 0, "{report:?}");
    }
}
