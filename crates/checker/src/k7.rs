//! K-7, convergence (PLAN.md §10.5; A-4): after the heal every recoverable node has applied the
//! catch-up marker and holds one state digest, computed at its application of the marker
//! (trailing election no-ops legitimately leave final applied indices unequal).
//!
//! A `drain` heal (DM-7 profiles) has no marker. It is judged on the certified drain instead:
//! every recoverable node has last log index = commit index = applied index, the same on all of
//! them, with one digest, over at least the manifest's quiet period.
//!
//! The recoverable nodes are the voters less the permanently lost; the lost are excluded and
//! [`Heal::excluded`] lists them. The unit examined is one recoverable node.

use std::collections::{BTreeMap, BTreeSet};

use crate::heal::{Completion, Drain, Heal};
use crate::report::{CheckError, Kind, Report, Violation};

/// Judges `heal` against K-7.
///
/// # Errors
///
/// None today: the signature matches the other checkers'.
pub fn k7(heal: &Heal) -> Result<Report, CheckError> {
    let recoverable: BTreeSet<u64> = heal.recoverable().collect();
    let violations = match heal.completion {
        Completion::Marker | Completion::PrecreatedMarker => marker_violations(heal, &recoverable),
        Completion::Drain => drain_violations(heal, &recoverable),
    };
    Ok(Report {
        checker: "K-7",
        examined: recoverable.len() as u64,
        violations,
    })
}

const fn violation(kind: Kind, detail: String) -> Violation {
    Violation {
        kind,
        request_id: String::new(),
        attempt: None,
        detail,
    }
}

fn marker_violations(heal: &Heal, recoverable: &BTreeSet<u64>) -> Vec<Violation> {
    let mut found = Vec::new();
    let applied: BTreeSet<u64> = heal.markers.iter().map(|marker| marker.node).collect();
    for node in recoverable.difference(&applied) {
        found.push(violation(
            Kind::MarkerMissing,
            format!(
                "rule: K-7 every recoverable node applies the catch-up marker; node {node} \
                 never did (recoverable {recoverable:?})"
            ),
        ));
    }
    // Only recoverable nodes judge: a lost node's record says nothing about convergence.
    let mut by_index: BTreeMap<u64, Vec<(u64, u64, &str)>> = BTreeMap::new();
    for marker in &heal.markers {
        if recoverable.contains(&marker.node) {
            by_index.entry(marker.index).or_default().push((
                marker.node,
                marker.incarnation,
                marker.digest.as_str(),
            ));
        }
    }
    if by_index.len() > 1 {
        found.push(violation(
            Kind::MarkerIndexDiffers,
            format!(
                "rule: K-7 there is one catch-up marker; recoverable nodes applied markers at \
                 indices {:?}",
                by_index.keys().collect::<Vec<_>>()
            ),
        ));
    }
    for (index, rows) in &by_index {
        let digests: BTreeSet<&str> = rows.iter().map(|(_, _, digest)| *digest).collect();
        if digests.len() > 1 {
            found.push(violation(
                Kind::DigestDiffers,
                format!(
                    "rule: K-7 identical state digests at the marker's application; marker \
                     index {index}, (node, incarnation, digest) = {rows:?}"
                ),
            ));
        }
    }
    found
}

fn drain_violations(heal: &Heal, recoverable: &BTreeSet<u64>) -> Vec<Violation> {
    let Some(drain) = &heal.drain else {
        if recoverable.is_empty() {
            return Vec::new();
        }
        return vec![violation(
            Kind::DrainMissing,
            format!(
                "rule: K-7 a drain heal converges in a certified drain; none was recorded \
                 (recoverable {recoverable:?})"
            ),
        )];
    };
    let mut found = Vec::new();
    let covered: BTreeSet<u64> = drain.nodes.iter().map(|row| row.node).collect();
    for node in recoverable.difference(&covered) {
        found.push(violation(
            Kind::DrainNotSettled,
            format!(
                "rule: K-7 the drain covers every recoverable node; node {node} is not in it \
                 (covers {covered:?})"
            ),
        ));
    }
    found.extend(settled_violations(drain, heal, recoverable));
    found
}

fn settled_violations(drain: &Drain, heal: &Heal, recoverable: &BTreeSet<u64>) -> Vec<Violation> {
    let mut found = Vec::new();
    if drain.quiet_ticks < heal.quiet_ticks {
        found.push(violation(
            Kind::DrainNotSettled,
            format!(
                "rule: K-7 the drain holds over the manifest's quiet period; it held {} ticks \
                 of {}",
                drain.quiet_ticks, heal.quiet_ticks
            ),
        ));
    }
    let rows: Vec<_> = drain
        .nodes
        .iter()
        .filter(|row| recoverable.contains(&row.node))
        .collect();
    for row in &rows {
        if row.last_index != row.commit || row.commit != row.applied {
            found.push(violation(
                Kind::DrainNotSettled,
                format!(
                    "rule: K-7 last log index = commit index = applied index on a drained node; \
                     node {} has last {} commit {} applied {}",
                    row.node, row.last_index, row.commit, row.applied
                ),
            ));
        }
    }
    let applied: BTreeSet<u64> = rows.iter().map(|row| row.applied).collect();
    if applied.len() > 1 {
        found.push(violation(
            Kind::DrainNotSettled,
            format!(
                "rule: K-7 every recoverable node drains to the same applied index; saw \
                 (node, applied) = {:?}",
                rows.iter()
                    .map(|row| (row.node, row.applied))
                    .collect::<Vec<_>>()
            ),
        ));
    }
    if let (Some(&drained), Some(&committed)) = (
        applied.iter().next_back(),
        heal.committed_at.keys().next_back(),
    ) && committed > drained
    {
        found.push(violation(
            Kind::DrainNotSettled,
            format!(
                "rule: K-7 a drain leaves no committed entry unapplied; index {committed} was \
                 committed but the drain applied only up to {drained}"
            ),
        ));
    }
    let digests: BTreeSet<&str> = rows.iter().map(|row| row.digest.as_str()).collect();
    if digests.len() > 1 {
        found.push(violation(
            Kind::DigestDiffers,
            format!(
                "rule: K-7 identical state digests across the drained nodes; saw (node, \
                 digest) = {:?}",
                rows.iter()
                    .map(|row| (row.node, row.digest.as_str()))
                    .collect::<Vec<_>>()
            ),
        ));
    }
    found
}

#[cfg(test)]
mod tests {
    // check: K-7
    use super::k7;
    use crate::heal_fixtures::Run;
    use crate::report::Kind;

    #[test]
    fn a_converged_marker_run_passes() {
        let report = k7(&Run::marker().heal()).unwrap();
        assert!(report.passed(), "rule: K-7 pass; {report:#?}");
        assert_eq!(report.examined, 5, "five recoverable nodes: {report:?}");
    }

    #[test]
    fn a_node_that_skipped_the_marker_fails() {
        let run = Run::marker_without(&[4]);
        let report = k7(&run.heal()).unwrap();
        assert_eq!(
            report.violations.len(),
            1,
            "rule: K-7 only node 4 skipped catch-up (B-18); {report:#?}"
        );
        assert!(report.has(Kind::MarkerMissing), "{report:#?}");
        assert!(
            report.violations[0].detail.contains("node 4"),
            "names the node: {report:#?}"
        );
    }

    #[test]
    fn a_marker_with_no_application_at_all_fails_for_every_node() {
        let run = Run::marker_without(&[1, 2, 3, 4, 5]);
        let report = k7(&run.heal()).unwrap();
        assert_eq!(
            report.violations.len(),
            5,
            "rule: K-7 each recoverable node is missing the marker; {report:#?}"
        );
    }

    #[test]
    fn a_digest_that_differs_at_the_marker_fails() {
        let run = Run::marker_with_digests(&[("a", 1), ("a", 2), ("b", 3), ("a", 4), ("a", 5)]);
        let report = k7(&run.heal()).unwrap();
        assert_eq!(report.violations.len(), 1, "one digest split: {report:#?}");
        assert!(
            report.has(Kind::DigestDiffers),
            "rule: K-7 node 3 holds another digest; {report:#?}"
        );
    }

    #[test]
    fn markers_at_different_indices_fail() {
        let mut run = Run::marker();
        run.marker_at(5, 1, 9, "d");
        let report = k7(&run.heal()).unwrap();
        assert!(
            report.has(Kind::MarkerIndexDiffers),
            "rule: K-7 one marker, two indices; {report:#?}"
        );
    }

    #[test]
    fn a_restarted_node_that_reapplies_the_marker_with_the_same_digest_passes() {
        let mut run = Run::marker();
        run.marker_at(2, 1, 3, "d");
        let report = k7(&run.heal()).unwrap();
        assert!(
            report.passed(),
            "rule: K-7 a second incarnation applying the same marker agrees; {report:#?}"
        );
    }

    #[test]
    fn a_reapplication_with_another_digest_fails() {
        let mut run = Run::marker();
        run.marker_at(2, 1, 3, "other");
        let report = k7(&run.heal()).unwrap();
        assert!(
            report.has(Kind::DigestDiffers),
            "rule: K-7 incarnation 1 of node 2 disagrees; {report:#?}"
        );
    }

    #[test]
    fn a_lost_node_is_excluded_and_listed() {
        let mut run = Run::marker_without(&[5]);
        run.lose(5);
        let heal = run.heal();
        let report = k7(&heal).unwrap();
        assert!(
            report.passed(),
            "rule: K-7 a permanently lost node need not apply the marker; {report:#?}"
        );
        assert_eq!(report.examined, 4, "four recoverable nodes: {report:?}");
        assert_eq!(heal.excluded(), vec![5], "the lost node is listed");
    }

    #[test]
    fn a_lost_node_s_own_marker_record_does_not_judge() {
        let mut run = Run::marker_with_digests(&[("d", 1), ("d", 2), ("d", 3), ("d", 4), ("x", 5)]);
        run.lose(5);
        let report = k7(&run.heal()).unwrap();
        assert!(
            report.passed(),
            "rule: K-7 excluded nodes are not compared; {report:#?}"
        );
    }

    #[test]
    fn a_node_lost_after_all_five_applied_is_still_excluded_from_the_count() {
        let mut run = Run::marker();
        run.lose(3);
        let report = k7(&run.heal()).unwrap();
        assert_eq!(report.examined, 4, "{report:?}");
        assert!(report.passed(), "{report:#?}");
    }

    #[test]
    fn a_precreated_marker_is_judged_like_a_marker() {
        let run = Run::precreated_without(&[2]);
        let report = k7(&run.heal()).unwrap();
        assert!(report.has(Kind::MarkerMissing), "{report:#?}");
    }

    #[test]
    fn a_certified_drain_passes() {
        let report = k7(&Run::drain().heal()).unwrap();
        assert!(report.passed(), "rule: K-7 drain pass; {report:#?}");
        assert_eq!(report.examined, 5, "{report:?}");
    }

    #[test]
    fn a_drain_heal_without_a_drain_fails() {
        let report = k7(&Run::drain_none().heal()).unwrap();
        assert!(
            report.has(Kind::DrainMissing),
            "rule: K-7 a drain heal needs the certified drain; {report:#?}"
        );
    }

    #[test]
    fn a_drain_with_a_pending_suffix_fails() {
        let report = k7(&Run::drain_rows(&[
            (1, 6, 5, 5, "d"),
            (2, 5, 5, 5, "d"),
            (3, 5, 5, 5, "d"),
            (4, 5, 5, 5, "d"),
            (5, 5, 5, 5, "d"),
        ])
        .heal())
        .unwrap();
        assert_eq!(
            report.violations.len(),
            1,
            "rule: K-7 last = commit = applied; node 1 has an uncommitted entry; {report:#?}"
        );
        assert!(report.has(Kind::DrainNotSettled), "{report:#?}");
    }

    #[test]
    fn a_drain_whose_nodes_applied_different_indices_fails() {
        let report = k7(&Run::drain_rows(&[
            (1, 5, 5, 5, "d"),
            (2, 5, 5, 5, "d"),
            (3, 4, 4, 4, "d"),
            (4, 5, 5, 5, "d"),
            (5, 5, 5, 5, "d"),
        ])
        .heal())
        .unwrap();
        assert!(
            report.has(Kind::DrainNotSettled),
            "rule: K-7 identical applied index; {report:#?}"
        );
    }

    #[test]
    fn a_drain_with_different_digests_fails() {
        let report = k7(&Run::drain_rows(&[
            (1, 5, 5, 5, "d"),
            (2, 5, 5, 5, "d"),
            (3, 5, 5, 5, "x"),
            (4, 5, 5, 5, "d"),
            (5, 5, 5, 5, "d"),
        ])
        .heal())
        .unwrap();
        assert!(
            report.has(Kind::DigestDiffers),
            "rule: K-7 identical digests; {report:#?}"
        );
    }

    #[test]
    fn a_drain_that_leaves_out_a_recoverable_node_fails() {
        let report = k7(&Run::drain_rows(&[
            (1, 5, 5, 5, "d"),
            (2, 5, 5, 5, "d"),
            (3, 5, 5, 5, "d"),
            (4, 5, 5, 5, "d"),
        ])
        .heal())
        .unwrap();
        assert!(
            report.has(Kind::DrainNotSettled),
            "rule: K-7 the drain covers node 5; {report:#?}"
        );
    }

    #[test]
    fn a_drain_may_leave_out_a_lost_node() {
        let mut run = Run::drain_rows(&[
            (1, 5, 5, 5, "d"),
            (2, 5, 5, 5, "d"),
            (3, 5, 5, 5, "d"),
            (4, 5, 5, 5, "d"),
        ]);
        run.lose(5);
        let report = k7(&run.heal()).unwrap();
        assert!(
            report.passed(),
            "rule: K-7 lost nodes excluded; {report:#?}"
        );
        assert_eq!(report.examined, 4, "{report:?}");
    }

    #[test]
    fn a_drain_shorter_than_the_quiet_period_fails() {
        let report = k7(&Run::drain_quiet(1).heal()).unwrap();
        assert!(
            report.has(Kind::DrainNotSettled),
            "rule: K-7 the drain holds over the manifest's 4-tick quiet period; {report:#?}"
        );
    }

    #[test]
    fn a_run_with_no_recoverable_node_examines_nothing() {
        let mut run = Run::marker_without(&[1, 2, 3, 4, 5]);
        for node in 1..=5 {
            run.lose(node);
        }
        let report = k7(&run.heal()).unwrap();
        assert!(report.passed(), "{report:#?}");
        assert_eq!(report.examined, 0, "{report:?}");
    }

    #[test]
    fn a_drain_that_leaves_a_committed_entry_unapplied_fails() {
        let report = k7(&Run::drain_with_unapplied_commit(6).heal()).unwrap();
        assert_eq!(
            report.violations.len(),
            1,
            "rule: K-7 index 6 committed after the drain applied up to 5; {report:#?}"
        );
        assert!(report.has(Kind::DrainNotSettled), "{report:#?}");
    }
}
