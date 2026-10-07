//! K-1, durability (PLAN.md §10.5; C-2, DUR-1): every success-acknowledged write has its first
//! execution in the final committed log, at its reported index with its reported effect, and
//! every recoverable node's final log holds that entry.
//!
//! An acknowledged success is a terminal `ok` response (a write's own, or inside a known
//! `status` answer) of an attempt whose response reached its client. The unit examined is one
//! such acknowledgement. A later legitimate overwrite of the key is fine: only the first
//! execution's entry is judged, never the key's current value.

use std::collections::BTreeMap;

use model::Decision;

use crate::history::History;
use crate::report::{CheckError, Kind, Report, Violation};

/// Judges `history` against K-1.
///
/// # Errors
///
/// [`CheckError::Replay`] if the committed entries cannot be replayed.
pub fn k1(history: &History) -> Result<Report, CheckError> {
    let first = history.first_executions()?;
    let payloads: BTreeMap<u64, &str> = history
        .committed
        .iter()
        .map(|c| (c.index.get(), c.payload.as_str()))
        .collect();
    let mut examined = 0;
    let mut violations = Vec::new();
    for claim in &history.claims {
        let acknowledged = match claim.result.decision {
            Decision::Set { .. } | Decision::Deleted { .. } => {
                history.delivered.contains(&claim.attempt)
            }
            Decision::VersionMismatch { .. } | Decision::CapacityExceeded { .. } => false,
        };
        if !acknowledged {
            continue;
        }
        examined += 1;
        let claimed = claim.result;
        let violation = |kind, detail: String| Violation {
            kind,
            request_id: claimed.request_id.to_string(),
            attempt: Some(claim.attempt),
            detail,
        };
        let Some(executed) = first.get(&claimed.request_id) else {
            violations.push(violation(
                Kind::AcknowledgedNeverCommitted,
                format!(
                    "rule: K-1 an acknowledged success executes in the committed log; \
                     acknowledged at index {}, but no committed command has this request ID",
                    claimed.index.get()
                ),
            ));
            continue;
        };
        if executed.index != claimed.index {
            violations.push(violation(
                Kind::AcknowledgedAtWrongIndex,
                format!(
                    "rule: K-1 the reported index is the first execution's; expected {}, \
                     acknowledged {}",
                    executed.index.get(),
                    claimed.index.get()
                ),
            ));
            continue;
        }
        if executed.decision != claimed.decision {
            violations.push(violation(
                Kind::AcknowledgedWithWrongEffect,
                format!(
                    "rule: K-1 the reported effect is the first execution's; expected \
                     {:?}, acknowledged {:?}",
                    executed.decision, claimed.decision
                ),
            ));
            continue;
        }
        let want = payloads.get(&executed.index.get()).copied().unwrap_or("");
        for node in history.recoverable() {
            let held = history
                .final_logs
                .get(&node)
                .and_then(|log| log.get(&executed.index.get()));
            match held {
                None => violations.push(violation(
                    Kind::MissingOnNode,
                    format!(
                        "rule: K-1 every recoverable node holds the acknowledged entry; node \
                         {node} has no entry at index {}",
                        executed.index.get()
                    ),
                )),
                Some(entry) if entry.payload != want => violations.push(violation(
                    Kind::DivergentOnNode,
                    format!(
                        "rule: K-1 every recoverable node holds the acknowledged entry; node \
                         {node} holds payload {:?} at index {}, committed {want:?}",
                        entry.payload,
                        executed.index.get()
                    ),
                )),
                Some(_) => {}
            }
        }
    }
    Ok(Report {
        checker: "K-1",
        examined,
        violations,
    })
}

#[cfg(test)]
mod tests {
    // check: K-1
    use serde_json::json;

    use super::k1;
    use crate::fixtures::{A, B, Doc, delete, ok_delete, ok_put, put, version_mismatch};
    use crate::history::History;
    use crate::report::{CheckError, Kind};

    const P2: &str = "02a1";
    const P3: &str = "03a2";

    /// A history in which `A` PUT k=v executed at index 2 on every node.
    fn acknowledged_put() -> Doc {
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, P2, put(A, "k", "v"))
            .invoke(1, A, false)
            .put_ok(1, A, 2, false);
        doc
    }

    #[test]
    fn passes_an_acknowledged_write_present_on_every_node() {
        let report = k1(&acknowledged_put().history()).unwrap();
        assert!(report.passed(), "rule: K-1 pass; report={report:#?}");
        assert_eq!(report.examined, 1, "one acknowledgement examined");
    }

    #[test]
    fn passes_after_a_legal_overwrite_of_the_acknowledged_key() {
        let mut doc = acknowledged_put();
        doc.commit_everywhere(3, P3, put(B, "k", "w"))
            .invoke(2, B, false)
            .put_ok(2, B, 3, false);
        let report = k1(&doc.history()).unwrap();
        assert!(
            report.passed() && report.examined == 2,
            "rule: K-1 a later overwrite of the key is legitimate; report={report:#?}"
        );
    }

    #[test]
    fn passes_a_delete_acknowledged_with_the_decided_existed() {
        let mut doc = acknowledged_put();
        doc.commit_everywhere(3, P3, delete(B, "k"))
            .invoke(2, B, false)
            .respond(2, "delete", ok_delete(B, 3, true, false))
            .deliver(2);
        let report = k1(&doc.history()).unwrap();
        assert!(
            report.passed(),
            "rule: K-1 delete effect; report={report:#?}"
        );
    }

    #[test]
    fn passes_a_replayed_success_at_the_original_index() {
        let mut doc = acknowledged_put();
        doc.invoke(2, A, false).put_ok(2, A, 2, true);
        let report = k1(&doc.history()).unwrap();
        assert!(
            report.passed() && report.examined == 2,
            "rule: K-1 a replay acknowledges the first execution; report={report:#?}"
        );
    }

    #[test]
    fn a_permanently_lost_node_need_not_hold_the_entry() {
        let mut doc = Doc::with_noop();
        doc.append(1, 2, P2)
            .append(2, 2, P2)
            .commit(2, P2, put(A, "k", "v"))
            .lose(3)
            .invoke(1, A, false)
            .put_ok(1, A, 2, false);
        let report = k1(&doc.history()).unwrap();
        assert!(
            report.passed(),
            "rule: K-1 judges recoverable nodes only; report={report:#?}"
        );
    }

    #[test]
    fn an_acknowledgement_that_never_reached_its_client_is_not_examined() {
        let mut doc = Doc::with_noop();
        doc.invoke(1, A, false)
            .respond(1, "put", ok_put(A, 2, 2, false));
        let report = k1(&doc.history()).unwrap();
        assert_eq!(report.examined, 0, "rule: acknowledged means delivered");
        assert!(report.passed(), "report={report:#?}");
    }

    #[test]
    fn a_refusal_is_not_a_success_to_judge() {
        let mut doc = Doc::with_noop();
        doc.invoke(1, A, false)
            .respond(1, "put", version_mismatch(A, 2, 1, false))
            .deliver(1);
        let report = k1(&doc.history()).unwrap();
        assert_eq!(report.examined, 0, "rule: K-1 judges successes only");
    }

    #[test]
    fn fails_a_lost_acknowledged_write() {
        let mut doc = Doc::with_noop();
        doc.invoke(1, A, false).put_ok(1, A, 2, false);
        let report = k1(&doc.history()).unwrap();
        assert!(
            report.has(Kind::AcknowledgedNeverCommitted),
            "rule: K-1 an acknowledged write is in the committed log; report={report:#?}"
        );
    }

    #[test]
    fn fails_an_acknowledged_write_missing_after_a_node_restart() {
        let mut doc = acknowledged_put();
        doc.recover(2, &[(1, "")]);
        let report = k1(&doc.history()).unwrap();
        assert!(report.has(Kind::MissingOnNode), "report={report:#?}");
        let missing = &report.violations[0];
        assert!(
            missing.detail.contains("node 2"),
            "rule: the violation names the node; violation={missing:#?}"
        );
    }

    #[test]
    fn fails_an_acknowledged_write_truncated_from_a_node() {
        let mut doc = acknowledged_put();
        doc.truncate(3, 2);
        let report = k1(&doc.history()).unwrap();
        assert!(report.has(Kind::MissingOnNode), "report={report:#?}");
    }

    #[test]
    fn fails_other_bytes_at_the_acknowledged_index() {
        let mut doc = acknowledged_put();
        doc.recover(1, &[(1, ""), (2, "02ff")]);
        let report = k1(&doc.history()).unwrap();
        assert!(report.has(Kind::DivergentOnNode), "report={report:#?}");
    }

    #[test]
    fn fails_an_index_that_is_not_the_first_execution() {
        let mut doc = acknowledged_put();
        doc.invoke(2, A, false).put_ok(2, A, 3, true);
        let report = k1(&doc.history()).unwrap();
        assert!(
            report.has(Kind::AcknowledgedAtWrongIndex),
            "report={report:#?}"
        );
    }

    #[test]
    fn fails_a_version_that_is_not_the_decided_one() {
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, P2, put(A, "k", "v"))
            .invoke(1, A, false)
            .respond(1, "put", ok_put(A, 2, 7, false))
            .deliver(1);
        let report = k1(&doc.history()).unwrap();
        assert!(
            report.has(Kind::AcknowledgedWithWrongEffect),
            "report={report:#?}"
        );
    }

    #[test]
    fn fails_a_delete_acknowledged_with_the_wrong_existed() {
        let mut doc = acknowledged_put();
        doc.commit_everywhere(3, P3, delete(B, "k"))
            .invoke(2, B, false)
            .respond(2, "delete", ok_delete(B, 3, false, false))
            .deliver(2);
        let report = k1(&doc.history()).unwrap();
        assert!(
            report.has(Kind::AcknowledgedWithWrongEffect),
            "report={report:#?}"
        );
    }

    #[test]
    fn fails_a_success_for_a_command_the_model_decided_was_a_mismatch() {
        let mut doc = Doc::with_noop();
        let cas = json!({"request_id": A, "operation": {"op": "cas", "key": "k",
            "expected": {"kind": "version", "version": 1}, "value": "v"}});
        doc.commit_everywhere(2, P2, cas)
            .invoke(1, A, false)
            .put_ok(1, A, 2, false);
        let report = k1(&doc.history()).unwrap();
        assert!(
            report.has(Kind::AcknowledgedWithWrongEffect),
            "rule: the model, not the node, decides the effect; report={report:#?}"
        );
    }

    #[test]
    fn rejects_a_document_that_is_not_json() {
        assert!(matches!(History::parse("{"), Err(CheckError::NotJson(_))));
    }

    #[test]
    fn rejects_a_document_missing_its_records() {
        let mut value = Doc::new().value();
        value.as_object_mut().unwrap().remove("records");
        let error = History::from_value(&value).unwrap_err();
        assert!(
            matches!(&error, CheckError::Malformed { at, .. } if at == ".records"),
            "rule: a field the checker needs is reported, not skipped; error={error:?}"
        );
    }

    #[test]
    fn rejects_committed_entries_with_a_gap() {
        let mut doc = Doc::new();
        doc.commit(2, P2, put(A, "k", "v"));
        let error = k1(&doc.history()).unwrap_err();
        assert!(
            matches!(error, CheckError::Replay { index: 2, .. }),
            "rule: the committed log replays from index 1; error={error:?}"
        );
    }

    #[test]
    fn passes_the_fixture_runs_and_fails_the_same_run_with_a_lost_entry() {
        for text in [
            include_str!("../../../schemas/observation/fixtures/run.v1.json"),
            include_str!("../../../schemas/observation/fixtures/drain.v1.json"),
        ] {
            let history = History::parse(text).unwrap();
            let report = k1(&history).unwrap();
            assert!(
                report.passed(),
                "rule: a correct run passes K-1; report={report:#?}"
            );
            assert!(
                report.examined > 0,
                "rule: the fixture acknowledges a write"
            );
            // The same run, with the first acknowledged entry removed from node 1's log
            // at the end: a lost acknowledged write.
            let mut value: serde_json::Value = serde_json::from_str(text).unwrap();
            let target = report_target(&history);
            value["records"].as_array_mut().unwrap().push(json!({
                "kind": "truncate", "seq": 0, "time": 0,
                "at": {"node": 1, "incarnation": 0}, "from_index": target}));
            let broken = k1(&History::from_value(&value).unwrap()).unwrap();
            assert!(
                broken.has(Kind::MissingOnNode),
                "rule: K-1 catches a truncated acknowledged entry; from_index={target}; \
                 report={broken:#?}"
            );
        }
    }

    /// The lowest index any acknowledged success reports.
    fn report_target(history: &History) -> u64 {
        history
            .claims
            .iter()
            .filter(|claim| history.delivered.contains(&claim.attempt))
            .map(|claim| claim.result.index.get())
            .min()
            .unwrap()
    }
}
