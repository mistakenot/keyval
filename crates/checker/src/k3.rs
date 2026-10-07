//! K-3, real-time order (PLAN.md §10.5; C-1): over logical writes (attempts collapsed by
//! request ID, as in K-10), if write A's first terminal decision was observed by a client
//! before write B's first invocation, then `index(A) < index(B)`. A later retry returns its
//! original, smaller index, so retries never violate it.
//!
//! Indices come from the reference model's replay of the committed log, never from a response
//! (§10.14): a node that reports the wrong index is K-2's and K-4's to catch, and cannot hide
//! an order violation here. A decision counts as observed at the first delivered copy of a
//! terminal execution response (a write's own or a known `status` answer), and a write counts
//! as invoked at its first non-misuse write attempt; both are positions in the document's
//! records, its event order. Writes that never executed have no index and are not judged
//! here. The unit examined is one logical write that executed and was invoked.

use std::collections::BTreeMap;

use model::RequestId;

use crate::history::History;
use crate::report::{CheckError, Kind, Report, Violation};

/// Judges `history` against K-3.
///
/// # Errors
///
/// [`CheckError::Replay`] if the committed entries cannot be replayed.
pub fn k3(history: &History) -> Result<Report, CheckError> {
    let first = history.first_executions()?;
    // Each logical write's first invocation: record position and attempt.
    let mut invoked: BTreeMap<RequestId, (u64, u64)> = BTreeMap::new();
    for (attempt, invoke) in &history.invoked {
        if !invoke.write || invoke.misuse {
            continue;
        }
        if let Some(id) = invoke.request_id {
            let entry = invoked.entry(id).or_insert((invoke.position, *attempt));
            if invoke.position < entry.0 {
                *entry = (invoke.position, *attempt);
            }
        }
    }
    // Each logical write's first observed terminal decision: record position and attempt.
    let mut observed: BTreeMap<RequestId, (u64, u64)> = BTreeMap::new();
    for claim in &history.claims {
        if history
            .invoked
            .get(&claim.attempt)
            .is_some_and(|invoke| invoke.misuse)
        {
            continue;
        }
        if let Some(position) = history.first_delivery.get(&claim.attempt) {
            let entry = observed
                .entry(claim.result.request_id)
                .or_insert((*position, claim.attempt));
            if *position < entry.0 {
                *entry = (*position, claim.attempt);
            }
        }
    }
    // Sweep the document's event order: the largest index among writes already observed
    // terminal must stay below the index of every write invoked after.
    let mut events: Vec<(u64, Event)> = Vec::new();
    for (id, (position, attempt)) in &invoked {
        if let Some(result) = first.get(id) {
            events.push((
                *position,
                Event::Invoke {
                    id: *id,
                    attempt: *attempt,
                    index: result.index.get(),
                },
            ));
        }
    }
    for (id, (position, _)) in &observed {
        if let Some(result) = first.get(id) {
            events.push((
                *position,
                Event::Observe {
                    id: *id,
                    index: result.index.get(),
                },
            ));
        }
    }
    events.sort_by_key(|(position, _)| *position);
    let mut examined = 0;
    let mut latest: Option<(RequestId, u64)> = None;
    let mut violations = Vec::new();
    for (position, event) in events {
        match event {
            Event::Observe { id, index } => {
                if latest.is_none_or(|(_, seen)| index > seen) {
                    latest = Some((id, index));
                }
            }
            Event::Invoke { id, attempt, index } => {
                examined += 1;
                if let Some((earlier, seen)) = latest.filter(|(_, seen)| *seen >= index) {
                    violations.push(Violation {
                        kind: Kind::RealTimeOrderViolated,
                        request_id: id.to_string(),
                        attempt: Some(attempt),
                        detail: format!(
                            "rule: K-3 a write invoked after another's terminal decision was \
                             observed executes at a larger index; write {id} was invoked at \
                             record {position} and executed at index {index}, but write \
                             {earlier} (index {seen}) was observed terminal before"
                        ),
                    });
                }
            }
        }
    }
    Ok(Report {
        checker: "K-3",
        examined,
        violations,
    })
}

enum Event {
    Invoke {
        id: RequestId,
        attempt: u64,
        index: u64,
    },
    Observe {
        id: RequestId,
        index: u64,
    },
}

#[cfg(test)]
mod tests {
    // check: K-3
    use super::k3;
    use crate::fixtures::{A, B, Doc, put};
    use crate::report::Kind;

    const C: &str = "000000000000000000000000000000a3";
    const P2: &str = "02a1";
    const P3: &str = "03a1";
    const P4: &str = "04a1";

    /// Two writes committed in order: A at index 2, B at index 3 (the no-op is index 1).
    fn two_writes() -> Doc {
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, P2, put(A, "k", "a"))
            .commit_everywhere(3, P3, put(B, "k", "b"));
        doc
    }

    #[test]
    fn sequential_writes_in_index_order_pass() {
        let mut doc = two_writes();
        doc.invoke(1, A, false).put_ok(1, A, 2, false);
        doc.invoke(2, B, false).put_ok(2, B, 3, false);
        let report = k3(&doc.history()).unwrap();
        assert!(report.passed(), "rule: K-3 ordered writes pass: {report:?}");
        assert_eq!(
            report.examined, 2,
            "both executed writes were invoked: {report:?}"
        );
    }

    #[test]
    fn concurrent_writes_in_either_order_pass() {
        // B is invoked before A's decision is observed, so either index order is legal.
        let mut doc = two_writes();
        doc.invoke(1, A, false).invoke(2, B, false);
        doc.put_ok(2, B, 3, false).put_ok(1, A, 2, false);
        let report = k3(&doc.history()).unwrap();
        assert!(
            report.passed(),
            "rule: K-3 overlapping writes are unordered: {report:?}"
        );
    }

    #[test]
    fn a_later_write_at_a_smaller_index_fails() {
        // A (index 3) is observed terminal, then B (index 2) is invoked.
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, P2, put(B, "k", "b"))
            .commit_everywhere(3, P3, put(A, "k", "a"));
        doc.invoke(1, A, false).put_ok(1, A, 3, false);
        doc.invoke(2, B, false).put_ok(2, B, 2, false);
        let report = k3(&doc.history()).unwrap();
        assert!(
            report.has(Kind::RealTimeOrderViolated),
            "rule: K-3 B was invoked after A was observed terminal; B={B} index 2, A={A} \
             index 3: {report:?}"
        );
        assert_eq!(
            report.violations.len(),
            1,
            "one violation, for B: {report:?}"
        );
        assert_eq!(report.violations[0].request_id, B);
    }

    #[test]
    fn a_retry_returning_its_original_smaller_index_passes() {
        // A executes at 2 and is observed; B is invoked and executes at 3; A is then retried
        // and answers index 2 (a replay): retries are not new writes.
        let mut doc = two_writes();
        doc.invoke(1, A, false).put_ok(1, A, 2, false);
        doc.invoke(2, B, false).put_ok(2, B, 3, false);
        doc.invoke(3, A, false).put_ok(3, A, 2, true);
        let report = k3(&doc.history()).unwrap();
        assert!(
            report.passed(),
            "rule: K-3 a retry never violates: {report:?}"
        );
        assert_eq!(
            report.examined, 2,
            "the retry is not a third write: {report:?}"
        );
    }

    #[test]
    fn an_undelivered_answer_is_not_an_observation() {
        // A's answer was lost: the client never observed the decision, so B may precede it.
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, P2, put(B, "k", "b"))
            .commit_everywhere(3, P3, put(A, "k", "a"));
        doc.invoke(1, A, false)
            .respond(1, "put", crate::fixtures::ok_put(A, 3, 3, false));
        doc.invoke(2, B, false).put_ok(2, B, 2, false);
        let report = k3(&doc.history()).unwrap();
        assert!(
            report.passed(),
            "rule: K-3 an undelivered answer is unobserved: {report:?}"
        );
    }

    #[test]
    fn indices_come_from_the_log_not_from_the_response() {
        // The responses claim the right order, but the log executed B first.
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, P2, put(B, "k", "b"))
            .commit_everywhere(3, P3, put(A, "k", "a"));
        doc.invoke(1, A, false).put_ok(1, A, 2, false);
        doc.invoke(2, B, false).put_ok(2, B, 3, false);
        let report = k3(&doc.history()).unwrap();
        assert!(
            report.has(Kind::RealTimeOrderViolated),
            "rule: K-3 judges the model's indices, not reported ones: {report:?}"
        );
    }

    #[test]
    fn a_write_without_an_execution_is_not_judged() {
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, P2, put(A, "k", "a"));
        doc.invoke(1, A, false).put_ok(1, A, 2, false);
        // B has no execution of its own; judging B alone is K-2's job, so it is skipped.
        doc.invoke(2, B, false).put_ok(2, B, 2, false);
        let report = k3(&doc.history()).unwrap();
        assert!(
            report.passed(),
            "rule: K-3 skips writes without an execution: {report:?}"
        );
        assert_eq!(report.examined, 1, "only A executed: {report:?}");
    }

    #[test]
    fn a_status_answer_observes_the_decision() {
        // A's own response is lost, but a later status lookup delivers the result.
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, P2, put(B, "k", "b"))
            .commit_everywhere(3, P3, put(A, "k", "a"));
        doc.invoke(1, A, false)
            .respond(1, "put", crate::fixtures::ok_put(A, 3, 3, false));
        doc.status_known(2, A, 3, 3);
        doc.invoke(3, B, false).put_ok(3, B, 2, false);
        let report = k3(&doc.history()).unwrap();
        assert!(
            report.has(Kind::RealTimeOrderViolated),
            "rule: K-3 a delivered known status answer is a terminal observation: {report:?}"
        );
    }

    #[test]
    fn a_misuse_attempt_is_neither_an_invocation_nor_an_observation() {
        let mut doc = two_writes();
        doc.invoke(1, A, false).put_ok(1, A, 2, false);
        doc.invoke(2, B, false).put_ok(2, B, 3, false);
        // A conflicting reuse of A, invoked last and answered with a conflict.
        doc.invoke(3, A, true);
        let report = k3(&doc.history()).unwrap();
        assert!(
            report.passed(),
            "rule: K-3 a misuse attempt is no new write: {report:?}"
        );
        assert_eq!(
            report.examined, 2,
            "misuse adds no logical write: {report:?}"
        );
    }

    #[test]
    fn three_writes_judge_against_the_latest_observed_index() {
        // A (2) and B (4) are observed before C (3) is invoked: C is behind B.
        let mut doc = Doc::with_noop();
        doc.commit_everywhere(2, P2, put(A, "k", "a"))
            .commit_everywhere(3, P3, put(C, "k", "c"))
            .commit_everywhere(4, P4, put(B, "k", "b"));
        doc.invoke(1, A, false).put_ok(1, A, 2, false);
        doc.invoke(2, B, false).put_ok(2, B, 4, false);
        doc.invoke(3, C, false).put_ok(3, C, 3, false);
        let report = k3(&doc.history()).unwrap();
        assert!(
            report.has(Kind::RealTimeOrderViolated),
            "rule: K-3 C (3) after B (4) observed: {report:?}"
        );
        assert_eq!(
            report.violations[0].request_id, C,
            "C is the late write: {report:?}"
        );
    }

    #[test]
    fn empty_history_passes_with_nothing_examined() {
        let report = k3(&Doc::with_noop().history()).unwrap();
        assert!(report.passed() && report.examined == 0, "{report:?}");
    }
}
