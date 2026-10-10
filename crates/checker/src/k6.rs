//! K-6, read monotonicity (PLAN.md §10.5; C-4): within one node incarnation, the
//! `applied_index` that reads report never decreases in the controller's response-generation
//! order (the `generation` of each `response` record), not in the order clients received them.
//!
//! A restart begins a new incarnation, and a new incarnation may start lower: the incarnation
//! is the unit the rule holds within. The unit examined is one read answered with `served_by`.

use std::collections::BTreeMap;

use crate::reads::{Answer, Query, Reads};
use crate::report::{CheckError, Kind, Report, Violation};

/// Judges `reads` against K-6.
///
/// # Errors
///
/// None today: the signature matches the other checkers'.
pub fn k6(reads: &Reads) -> Result<Report, CheckError> {
    let mut by_incarnation: BTreeMap<(u64, u64), Vec<&Answer>> = BTreeMap::new();
    for answer in &reads.answers {
        by_incarnation
            .entry((answer.node, answer.incarnation))
            .or_default()
            .push(answer);
    }
    let mut found: Vec<(u64, Violation)> = Vec::new();
    for ((node, incarnation), mut answers) in by_incarnation {
        answers.sort_by_key(|answer| (answer.generation, answer.position));
        let mut highest: Option<&Answer> = None;
        for answer in answers {
            if let Some(before) = highest
                && answer.applied_index < before.applied_index
            {
                found.push((
                    answer.position,
                    Violation {
                        kind: Kind::ReadRegressed,
                        request_id: match &answer.query {
                            Query::Status { request_id } => request_id.to_string(),
                            Query::Get { .. } | Query::Scan { .. } => String::new(),
                        },
                        attempt: Some(answer.attempt),
                        detail: format!(
                            "rule: K-6 applied_index never decreases within one incarnation in \
                             generation order; node {node} incarnation {incarnation} reported \
                             {} at generation {} (attempt {}) after reporting {} at generation \
                             {} (attempt {})",
                            answer.applied_index,
                            answer.generation,
                            answer.attempt,
                            before.applied_index,
                            before.generation,
                            before.attempt
                        ),
                    },
                ));
            }
            if highest.is_none_or(|before| answer.applied_index > before.applied_index) {
                highest = Some(answer);
            }
        }
    }
    found.sort_by_key(|(position, _)| *position);
    Ok(Report {
        checker: "K-6",
        examined: reads.answers.len() as u64,
        violations: found.into_iter().map(|(_, v)| v).collect(),
    })
}

#[cfg(test)]
mod tests {
    // check: K-6
    use super::k6;
    use crate::fixtures::{Doc, get_found, get_op};
    use crate::report::Kind;

    /// A GET answered by `who` = (node, incarnation, generation) reporting `applied`.
    fn answer(doc: &mut Doc, attempt: u64, who: (u64, u64, u64), applied: u64) {
        doc.read(
            attempt,
            get_op("k"),
            who,
            get_found("v", applied.max(1), who.0, applied),
        );
    }

    #[test]
    fn a_non_decreasing_sequence_passes() {
        let mut doc = Doc::with_noop();
        for (attempt, applied) in [(1, 1), (2, 3), (3, 3), (4, 9)] {
            answer(&mut doc, attempt, (1, 0, attempt), applied);
        }
        let report = k6(&doc.reads()).unwrap();
        assert!(report.passed(), "rule: K-6 pass; {report:#?}");
        assert_eq!(report.examined, 4, "four reads examined: {report:?}");
    }

    #[test]
    fn a_regression_within_an_incarnation_fails() {
        let mut doc = Doc::with_noop();
        answer(&mut doc, 1, (1, 0, 1), 5);
        answer(&mut doc, 2, (1, 0, 2), 3);
        let report = k6(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ReadRegressed),
            "rule: C-4 applied 3 at generation 2 after 5 at generation 1: {report:#?}"
        );
        assert_eq!(
            report.violations[0].attempt,
            Some(2),
            "the later answer: {report:#?}"
        );
    }

    #[test]
    fn order_is_generation_order_not_record_order() {
        // The records list generation 2 (applied 5) before generation 1 (applied 3): the
        // controller generated 3 then 5, so there is no regression.
        let mut doc = Doc::with_noop();
        answer(&mut doc, 1, (1, 0, 2), 5);
        answer(&mut doc, 2, (1, 0, 1), 3);
        let report = k6(&doc.reads()).unwrap();
        assert!(
            report.passed(),
            "rule: K-6 generation order governs; {report:#?}"
        );
    }

    #[test]
    fn a_regression_in_generation_order_hidden_by_record_order_fails() {
        let mut doc = Doc::with_noop();
        answer(&mut doc, 1, (1, 0, 2), 3);
        answer(&mut doc, 2, (1, 0, 1), 5);
        let report = k6(&doc.reads()).unwrap();
        assert!(
            report.has(Kind::ReadRegressed),
            "rule: K-6 generation 1 reported 5, generation 2 reported 3: {report:#?}"
        );
    }

    #[test]
    fn a_new_incarnation_may_start_lower() {
        let mut doc = Doc::with_noop();
        answer(&mut doc, 1, (1, 0, 1), 5);
        answer(&mut doc, 2, (1, 1, 1), 2);
        answer(&mut doc, 3, (1, 1, 2), 4);
        let report = k6(&doc.reads()).unwrap();
        assert!(
            report.passed(),
            "rule: K-6 a restart begins a new incarnation; {report:#?}"
        );
    }

    #[test]
    fn a_regression_after_a_restart_within_the_new_incarnation_fails() {
        let mut doc = Doc::with_noop();
        answer(&mut doc, 1, (1, 0, 1), 5);
        answer(&mut doc, 2, (1, 1, 1), 4);
        answer(&mut doc, 3, (1, 1, 2), 2);
        let report = k6(&doc.reads()).unwrap();
        assert_eq!(
            report.violations.len(),
            1,
            "rule: K-6 only attempt 3: {report:#?}"
        );
        assert_eq!(
            report.violations[0].attempt,
            Some(3),
            "rule: K-6 attempt 3: {report:#?}"
        );
    }

    #[test]
    fn nodes_are_judged_independently() {
        let mut doc = Doc::with_noop();
        answer(&mut doc, 1, (1, 0, 1), 5);
        answer(&mut doc, 2, (2, 0, 1), 2);
        answer(&mut doc, 3, (2, 0, 2), 3);
        let report = k6(&doc.reads()).unwrap();
        assert!(
            report.passed(),
            "rule: K-6 node 2 may trail node 1; {report:#?}"
        );
    }

    #[test]
    fn a_history_without_reads_passes_with_nothing_examined() {
        let report = k6(&Doc::with_noop().reads()).unwrap();
        assert!(report.passed(), "rule: K-6 vacuous pass; {report:#?}");
        assert_eq!(report.examined, 0, "no reads: {report:?}");
    }
}
