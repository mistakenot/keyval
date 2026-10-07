//! K-2, exactly-once (PLAN.md §10.5; C-6, F-6, DM-5): each request ID has at most one first
//! execution in the committed log, and every terminal execution response for it reports that
//! execution's index and decision. A conflicting reuse is never answered as an execution.
//!
//! First executions come from the reference model's replay of the committed log, so the check
//! is independent of any node's cache or attempt count. `request_id_conflict` answers and
//! non-terminal refusals are not terminal execution responses and are never examined. The
//! unit examined is one terminal execution response.

use std::collections::BTreeMap;

use model::RequestId;

use crate::history::{Claim, History};
use crate::report::{CheckError, Kind, Report, Violation};

/// Judges `history` against K-2.
///
/// # Errors
///
/// [`CheckError::Replay`] if the committed entries cannot be replayed.
pub fn k2(history: &History) -> Result<Report, CheckError> {
    let first = history.first_executions()?;
    let mut violations = Vec::new();
    let mut seen: BTreeMap<RequestId, Claim> = BTreeMap::new();
    let mut fresh: BTreeMap<RequestId, Claim> = BTreeMap::new();
    for claim in &history.claims {
        let claimed = claim.result;
        let violation = |kind, detail: String| Violation {
            kind,
            request_id: claimed.request_id.to_string(),
            attempt: Some(claim.attempt),
            detail,
        };
        if history
            .invoked
            .get(&claim.attempt)
            .is_some_and(|invoked| invoked.misuse)
        {
            violations.push(violation(
                Kind::MisuseAnsweredAsExecution,
                format!(
                    "rule: K-2 a conflicting reuse affects nothing and is answered \
                     request_id_conflict; attempt {} answered as an execution at index {}",
                    claim.attempt,
                    claimed.index.get()
                ),
            ));
            continue;
        }
        match first.get(&claimed.request_id) {
            None => violations.push(violation(
                Kind::ExecutedWithoutCommit,
                format!(
                    "rule: K-2 a terminal execution response reports an execution in the \
                     committed log; reported index {}, but no committed command has this \
                     request ID",
                    claimed.index.get()
                ),
            )),
            Some(executed) if executed != &claimed => violations.push(violation(
                Kind::ResponseContradictsLog,
                format!(
                    "rule: K-2 every terminal response reports the first execution's index \
                     and decision; expected {executed:?}, reported {claimed:?}"
                ),
            )),
            Some(_) => {}
        }
        if let Some(earlier) = seen.get(&claimed.request_id) {
            if earlier.result != claimed {
                violations.push(violation(
                    Kind::InconsistentResponses,
                    format!(
                        "rule: K-2 terminal responses for one request ID agree; attempt {} \
                         reported {:?}, attempt {} reported {claimed:?}",
                        earlier.attempt, earlier.result, claim.attempt
                    ),
                ));
            }
        } else {
            seen.insert(claimed.request_id, *claim);
        }
        if !claim.replayed {
            if let Some(earlier) = fresh.get(&claimed.request_id) {
                if earlier.result.index != claimed.index {
                    violations.push(violation(
                        Kind::TwoFirstExecutions,
                        format!(
                            "rule: K-2 at most one first execution per request ID; attempt {} \
                             reported a first execution at index {}, attempt {} at index {}",
                            earlier.attempt,
                            earlier.result.index.get(),
                            claim.attempt,
                            claimed.index.get()
                        ),
                    ));
                }
            } else {
                fresh.insert(claimed.request_id, *claim);
            }
        }
    }
    Ok(Report {
        checker: "K-2",
        examined: history.claims.len() as u64,
        violations,
    })
}

#[cfg(test)]
mod tests {
    // check: K-2
    use super::k2;
    use crate::fixtures::{A, B, Doc, conflict, ok_put, put, version_mismatch};
    use crate::history::History;
    use crate::report::Kind;

    const P2: &str = "02a1";
    const P3: &str = "03a1";

    fn executed_once() -> Doc {
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, P2, put(A, "k", "v"))
            .invoke(1, A, false)
            .put_ok(1, A, 2, false);
        doc
    }

    #[test]
    fn passes_a_single_execution_and_a_consistent_replay() {
        let mut doc = executed_once();
        doc.invoke(2, A, false).put_ok(2, A, 2, true);
        let report = k2(&doc.history()).unwrap();
        assert!(report.passed(), "rule: K-2 pass; report={report:#?}");
        assert_eq!(report.examined, 2, "two terminal responses examined");
    }

    #[test]
    fn passes_when_the_log_holds_the_same_command_twice() {
        let mut doc = executed_once();
        doc.commit_everywhere(3, P3, put(A, "k", "v"))
            .invoke(2, A, false)
            .put_ok(2, A, 2, true);
        let report = k2(&doc.history()).unwrap();
        assert!(
            report.passed(),
            "rule: a second copy in the log is a replay, not a second execution; \
             report={report:#?}"
        );
    }

    #[test]
    fn passes_a_conflicting_reuse_answered_with_a_conflict() {
        let mut doc = executed_once();
        doc.commit_everywhere(3, P3, put(A, "k", "other"))
            .invoke(2, A, true)
            .respond(2, "put", conflict(A, 2))
            .deliver(2);
        let report = k2(&doc.history()).unwrap();
        assert!(
            report.passed() && report.examined == 1,
            "rule: request_id_conflict answers are not examined; report={report:#?}"
        );
    }

    #[test]
    fn passes_a_consistent_terminal_refusal_and_its_replay() {
        let mut doc = Doc::with_noop();
        let cas = serde_json::json!({"request_id": B, "operation": {"op": "cas", "key": "k",
            "expected": {"kind": "absent"}, "value": "v"}});
        doc.commit_everywhere(2, P2, put(A, "k", "v"))
            .commit_everywhere(3, P3, cas)
            .invoke(1, B, false)
            .respond(1, "cas", version_mismatch(B, 3, 2, false))
            .invoke(2, B, false)
            .respond(2, "cas", version_mismatch(B, 3, 2, true));
        let report = k2(&doc.history()).unwrap();
        assert!(
            report.passed(),
            "rule: K-2 terminal refusals; report={report:#?}"
        );
    }

    #[test]
    fn fails_two_first_executions_of_one_request() {
        let mut doc = executed_once();
        doc.commit_everywhere(3, P3, put(A, "k", "v"))
            .invoke(2, A, false)
            .put_ok(2, A, 3, false);
        let report = k2(&doc.history()).unwrap();
        assert!(report.has(Kind::TwoFirstExecutions), "report={report:#?}");
        assert!(
            report.has(Kind::ResponseContradictsLog),
            "report={report:#?}"
        );
        assert!(
            report.has(Kind::InconsistentResponses),
            "report={report:#?}"
        );
    }

    #[test]
    fn fails_a_replay_reporting_another_index() {
        let mut doc = executed_once();
        doc.commit_everywhere(3, P3, put(A, "k", "v"))
            .invoke(2, A, false)
            .put_ok(2, A, 3, true);
        let report = k2(&doc.history()).unwrap();
        assert!(
            report.has(Kind::InconsistentResponses),
            "report={report:#?}"
        );
        assert!(!report.has(Kind::TwoFirstExecutions), "report={report:#?}");
    }

    #[test]
    fn fails_a_response_with_the_wrong_decision() {
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, P2, put(A, "k", "v"))
            .invoke(1, A, false)
            .respond(1, "put", ok_put(A, 2, 9, false));
        let report = k2(&doc.history()).unwrap();
        assert!(
            report.has(Kind::ResponseContradictsLog),
            "report={report:#?}"
        );
    }

    #[test]
    fn fails_an_execution_that_is_not_in_the_log() {
        let mut doc = Doc::with_noop();
        doc.invoke(1, A, false).put_ok(1, A, 2, false);
        let report = k2(&doc.history()).unwrap();
        assert!(
            report.has(Kind::ExecutedWithoutCommit),
            "report={report:#?}"
        );
    }

    #[test]
    fn fails_a_conflicting_reuse_answered_as_an_execution() {
        let mut doc = executed_once();
        doc.commit_everywhere(3, P3, put(A, "k", "other"))
            .invoke(2, A, true)
            .put_ok(2, A, 3, false);
        let report = k2(&doc.history()).unwrap();
        assert!(
            report.has(Kind::MisuseAnsweredAsExecution),
            "report={report:#?}"
        );
    }

    #[test]
    fn passes_the_fixture_runs_and_fails_the_same_run_with_a_second_execution() {
        for text in [
            include_str!("../../../schemas/observation/fixtures/run.v1.json"),
            include_str!("../../../schemas/observation/fixtures/drain.v1.json"),
        ] {
            let history = History::parse(text).unwrap();
            let report = k2(&history).unwrap();
            assert!(
                report.passed(),
                "rule: a correct run passes K-2; report={report:#?}"
            );
            assert!(report.examined > 0, "rule: the fixture answers a write");
            // The same run with its first first-execution response moved one index along.
            let mut value: serde_json::Value = serde_json::from_str(text).unwrap();
            let moved = value["records"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|r| r["kind"] == "response" && r["outcome"]["replayed"] == false)
                .unwrap();
            let index = moved["outcome"]["index"].as_u64().unwrap();
            moved["outcome"]["index"] = serde_json::json!(index + 1);
            let broken = k2(&History::from_value(&value).unwrap()).unwrap();
            assert!(
                broken.has(Kind::ResponseContradictsLog),
                "rule: K-2 catches a first execution reported at another index; \
                 report={broken:#?}"
            );
        }
    }
}
