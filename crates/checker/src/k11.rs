//! K-11, persistence ordering (PLAN.md §10.5; DUR-1, DUR-2, C-2): on the trace, every vote
//! grant, append acknowledgement, client success, published read and acceptance rests only on
//! durability that a **completed sync** had already provided. It catches an early release
//! deterministically, with no unlucky crash needed.
//!
//! The obligations are those of DUR-2: the **term and vote** (the hard state), and the
//! **entries**. The commit index is never an obligation and is never read here.
//!
//! A node's durable state is rebuilt from the records alone: its hard state and log as
//! `recovered` says for an incarnation's start, then, for each `sync`, the term, vote and log
//! **as they were when the sync was requested**. The log at a request is the `recovered` log
//! changed by the `append` and `truncate` records before it. A sync counts only when it
//! completes: a `requested` sync covers nothing, and a `stale` one (a completion that names a
//! request of an earlier incarnation, B-17) covers nothing either.
//!
//! A release is judged where it stands, against that durable state:
//!
//! - its hard state, if it names one, equals the durable hard state;
//! - its log, up to `last_index`, is in the node's current log, ends in `last_term`, and is
//!   entry for entry what is durable. After a truncation a replaced entry is uncovered until a
//!   later sync covers its replacement.
//!
//! An `accept` record is judged the same way against the accepted entry: the accepting node's
//! durable log holds that index in the entry's term.
//!
//! Each violation has the [`Kind`] of what was released: [`Kind::VoteBeforeSync`] (B-02),
//! [`Kind::AckBeforeSync`] (B-03), [`Kind::SuccessBeforeSync`], [`Kind::ReadBeforeSync`] and
//! [`Kind::AcceptBeforeSync`]. The unit examined is one release or acceptance.
//!
//! Limits: a release is judged against the durable state it names, so an obligation that is
//! too small is the node's claim that nothing more was needed; the checker holds it to the
//! rule that the claim is met, not to the right size. A pre-vote grant is not a release.

use std::collections::BTreeMap;

use crate::godview::{Durable, Event, GodView, Obligation, Released, Stamped, SyncState};
use crate::history::LogEntry;
use crate::report::{CheckError, Kind, Report, Violation};

/// The most violations a report lists (CD-9): the judge is an offline oracle whose other state
/// is bounded by the size of the document it reads, so only the report needs a cap.
const MAX_VIOLATIONS: usize = 256;

/// Judges `view` against K-11.
///
/// # Errors
///
/// [`CheckError::Malformed`] if the stream breaks a producer guarantee the judgement relies on:
/// an append that does not extend its log by one, a truncation of entries a log does not hold, a
/// sync that covers a log longer than the node's, a sync ID used twice, or a completion that
/// names no request of its own incarnation.
pub fn k11(view: &GodView) -> Result<Report, CheckError> {
    let mut judge = Judge::default();
    for stamped in &view.events {
        judge.observe(stamped)?;
    }
    Ok(Report {
        checker: "K-11",
        examined: judge.examined,
        violations: judge.violations,
    })
}

/// A node's log and what of it, with the hard state, a completed sync made durable.
#[derive(Default)]
struct Disk {
    /// The log in memory: the `recovered` log changed by `append` and `truncate`.
    log: Vec<LogEntry>,
    /// The hard state on disk.
    hard_state: (u64, Option<u64>),
    /// The log on disk.
    durable: Vec<LogEntry>,
    /// The sequence number of the newest request whose completion reached the disk.
    applied: u64,
}

/// What a requested sync covers: the hard state and the log as of the request.
struct Request {
    covers: Durable,
    log: Vec<LogEntry>,
    /// Position among all requests, in record order.
    sequence: u64,
}

#[derive(Default)]
struct Judge {
    disks: BTreeMap<u64, Disk>,
    /// The requested syncs by (node, incarnation, sync ID).
    requests: BTreeMap<(u64, u64, u64), Request>,
    violations: Vec<Violation>,
    examined: u64,
    /// How many syncs have been requested so far.
    sequence: u64,
}

impl Judge {
    fn flag(&mut self, kind: Kind, at: &str, detail: impl Into<String>) {
        if self.violations.len() >= MAX_VIOLATIONS {
            return;
        }
        self.violations.push(Violation {
            kind,
            request_id: String::new(),
            attempt: None,
            detail: format!("{} [{at}]", detail.into()),
        });
    }

    fn observe(&mut self, stamped: &Stamped) -> Result<(), CheckError> {
        let at = stamped.at.as_str();
        match &stamped.event {
            Event::Recovered {
                node,
                log,
                durable_term,
                durable_vote,
            } => {
                self.disks.insert(
                    *node,
                    Disk {
                        log: log.clone(),
                        hard_state: (*durable_term, *durable_vote),
                        durable: log.clone(),
                        // Requests made before the restart can no longer change the disk.
                        applied: self.sequence,
                    },
                );
            }
            Event::Append { node, index, entry } => {
                let disk = self.disks.entry(*node).or_default();
                if *index != disk.log.len() as u64 + 1 {
                    return Err(CheckError::malformed(
                        at,
                        format!(
                            "node {node} appended index {index} to a log of {} entries",
                            disk.log.len()
                        ),
                    ));
                }
                disk.log.push(entry.clone());
            }
            Event::Truncate { node, from } => {
                let disk = self.disks.entry(*node).or_default();
                let keep = usize::try_from(from.saturating_sub(1)).unwrap_or(usize::MAX);
                if keep > disk.log.len() {
                    return Err(CheckError::malformed(
                        at,
                        format!(
                            "node {node} truncated from {from} in a log of {} entries",
                            disk.log.len()
                        ),
                    ));
                }
                disk.log.truncate(keep);
            }
            Event::Sync {
                node,
                incarnation,
                id,
                state,
                covers,
            } => self.sync((*node, *incarnation, *id), *state, *covers, at)?,
            Event::Release {
                node,
                what,
                requires,
                ..
            } => self.release(*node, *what, *requires, at),
            Event::Accept {
                node,
                index,
                entry_term,
                ..
            } => self.accept(*node, *index, *entry_term, at),
            Event::Committed { .. } | Event::State { .. } => {}
        }
        Ok(())
    }

    fn sync(
        &mut self,
        (node, incarnation, id): (u64, u64, u64),
        state: SyncState,
        covers: Durable,
        at: &str,
    ) -> Result<(), CheckError> {
        match state {
            SyncState::Requested => {
                let disk = self.disks.entry(node).or_default();
                let end = usize::try_from(covers.last_index).unwrap_or(usize::MAX);
                if end > disk.log.len() {
                    return Err(CheckError::malformed(
                        at,
                        format!(
                            "sync {id} covers {end} entries of node {node}'s log of {}",
                            disk.log.len()
                        ),
                    ));
                }
                self.sequence += 1;
                let request = Request {
                    covers,
                    log: disk.log[..end].to_vec(),
                    sequence: self.sequence,
                };
                if self
                    .requests
                    .insert((node, incarnation, id), request)
                    .is_some()
                {
                    return Err(CheckError::malformed(
                        at,
                        format!(
                            "sync {id} requested twice by node {node} incarnation {incarnation}"
                        ),
                    ));
                }
            }
            SyncState::Completed => {
                let request = self.requests.get(&(node, incarnation, id)).ok_or_else(|| {
                    CheckError::malformed(
                        at,
                        format!(
                            "sync {id} completed on node {node} incarnation {incarnation} \
                                 without a request of that incarnation"
                        ),
                    )
                })?;
                let disk = self.disks.entry(node).or_default();
                // Completions may finish out of order: the hard state never moves back.
                if request.covers.term >= disk.hard_state.0 {
                    disk.hard_state = (request.covers.term, request.covers.vote);
                }
                // A sync requested earlier than one already completed adds nothing: the
                // later sync's log contains it, so the durable log never shrinks.
                if request.sequence > disk.applied {
                    disk.applied = request.sequence;
                    disk.durable.clone_from(&request.log);
                }
            }
            // A stale completion covers nothing (B-17): there is nothing to record.
            SyncState::Stale => {}
        }
        Ok(())
    }

    fn release(&mut self, node: u64, what: Released, requires: Obligation, at: &str) {
        self.examined += 1;
        let kind = match what {
            Released::VoteGrant => Kind::VoteBeforeSync,
            Released::AppendAck => Kind::AckBeforeSync,
            Released::ClientSuccess(_) => Kind::SuccessBeforeSync,
            Released::PublishedRead(_) => Kind::ReadBeforeSync,
        };
        let disk = self.disks.entry(node).or_default();
        let mut missing = Vec::new();
        if let Some(hard_state) = requires.hard_state
            && hard_state != disk.hard_state
        {
            missing.push(format!(
                "hard state {hard_state:?} where {:?} is durable",
                disk.hard_state
            ));
        }
        let end = usize::try_from(requires.last_index).unwrap_or(usize::MAX);
        if end > 0 {
            if disk.log.get(end - 1).map(|entry| entry.term) != Some(requires.last_term) {
                missing.push(format!(
                    "log index {end} in term {} where the node holds {:?}",
                    requires.last_term,
                    disk.log.get(end - 1).map(|entry| entry.term)
                ));
            } else if disk.durable.get(..end) != disk.log.get(..end) {
                missing.push(format!(
                    "log up to {end}, of which {} entries are durable and equal",
                    disk.durable
                        .iter()
                        .zip(&disk.log)
                        .take_while(|(durable, current)| durable == current)
                        .count()
                ));
            }
        }
        if !missing.is_empty() {
            self.flag(
                kind,
                at,
                format!(
                    "rule: a {what:?} depends only on durability a completed sync provided \
                     (DUR-2); node={node} missing: {}",
                    missing.join("; ")
                ),
            );
        }
    }

    fn accept(&mut self, node: u64, index: u64, entry_term: u64, at: &str) {
        self.examined += 1;
        let position = usize::try_from(index.saturating_sub(1)).unwrap_or(usize::MAX);
        let durable = self
            .disks
            .get(&node)
            .and_then(|disk| disk.durable.get(position))
            .map(|entry| entry.term);
        if index == 0 || durable != Some(entry_term) {
            self.flag(
                Kind::AcceptBeforeSync,
                at,
                format!(
                    "rule: an acceptance follows the sync that made the entry durable (DUR-1); \
                     node={node} index={index} entry_term={entry_term} durable_term={durable:?}"
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    // check: K-11
    use super::k11;
    use crate::godview::{GodView, Released, SyncState};
    use crate::raft_fixtures::Run;
    use crate::report::{CheckError, Kind, Report};

    fn judged(run: &Run) -> Report {
        k11(&run.view()).unwrap()
    }

    fn expect(run: &Run, kind: Kind) {
        let report = judged(run);
        assert!(
            report.violations.iter().any(|v| v.kind == kind),
            "rule: K-11 flags {kind}; report={report:#?}"
        );
    }

    fn expect_pass(run: &Run, what: &str) {
        let report = judged(run);
        assert!(
            report.passed(),
            "rule: K-11 passes {what}; report={report:#?}"
        );
    }

    fn malformed(run: &Run, what: &str) {
        let error = k11(&run.view()).unwrap_err();
        assert!(
            matches!(error, CheckError::Malformed { .. }),
            "rule: {what} is a malformed document, never a pass; error={error:?}"
        );
    }

    #[test]
    fn a_vote_granted_after_its_sync_completed_passes() {
        let mut run = Run::new();
        run.step().synced(2, 1, 1, Some(1), (0, 0));
        run.step()
            .release(2, Released::VoteGrant, Some((1, Some(1))), (0, 0));
        let report = judged(&run);
        assert!(
            report.passed(),
            "rule: K-11 passes a synced vote; report={report:#?}"
        );
        assert_eq!(report.examined, 1, "one release judged");
    }

    #[test]
    fn flags_a_vote_granted_before_any_sync_b02() {
        let mut run = Run::new();
        run.step()
            .release(2, Released::VoteGrant, Some((1, Some(1))), (0, 0));
        expect(&run, Kind::VoteBeforeSync);
    }

    #[test]
    fn flags_a_vote_whose_sync_covered_another_vote() {
        let mut run = Run::new();
        run.step().synced(2, 1, 1, None, (0, 0));
        run.step()
            .release(2, Released::VoteGrant, Some((1, Some(1))), (0, 0));
        expect(&run, Kind::VoteBeforeSync);
    }

    #[test]
    fn flags_a_vote_while_its_sync_is_only_requested() {
        let mut run = Run::new();
        run.step()
            .sync_record(2, 1, SyncState::Requested, 1, Some(1), (0, 0));
        run.step()
            .release(2, Released::VoteGrant, Some((1, Some(1))), (0, 0));
        expect(&run, Kind::VoteBeforeSync);
    }

    #[test]
    fn a_vote_recovered_as_durable_may_be_granted_again() {
        let mut run = Run::new();
        run.step().recover(3, 2, Some(1));
        run.step()
            .release(3, Released::VoteGrant, Some((2, Some(1))), (0, 0));
        expect_pass(&run, "a vote the recovery found on disk");
    }

    #[test]
    fn an_append_acknowledged_after_its_sync_passes() {
        let mut run = Run::new();
        run.step()
            .append(2, 1, 1, "aa")
            .synced(2, 1, 1, Some(1), (1, 1));
        run.step()
            .release(2, Released::AppendAck, Some((1, Some(1))), (1, 1));
        expect_pass(&run, "an ack after the sync completed");
    }

    #[test]
    fn flags_an_append_acknowledged_before_it_is_durable_b03() {
        let mut run = Run::new();
        run.step().append(2, 1, 1, "aa");
        run.step()
            .release(2, Released::AppendAck, Some((1, Some(1))), (1, 1));
        expect(&run, Kind::AckBeforeSync);
    }

    #[test]
    fn flags_an_ack_for_entries_appended_after_the_sync_was_requested() {
        let mut run = Run::new();
        run.step()
            .append(2, 1, 1, "aa")
            .synced(2, 1, 1, Some(1), (1, 1));
        run.step().append(2, 2, 1, "bb");
        run.step()
            .release(2, Released::AppendAck, Some((1, Some(1))), (2, 1));
        expect(&run, Kind::AckBeforeSync);
    }

    #[test]
    fn syncs_completing_out_of_order_never_shrink_the_durable_log() {
        let mut run = Run::new();
        run.step().append(2, 1, 1, "aa").sync_record(
            2,
            1,
            SyncState::Requested,
            1,
            Some(1),
            (1, 1),
        );
        run.step().append(2, 2, 1, "bb").sync_record(
            2,
            2,
            SyncState::Requested,
            1,
            Some(1),
            (2, 1),
        );
        // The later request completes first; the earlier one completes afterwards.
        run.step()
            .sync_record(2, 2, SyncState::Completed, 1, Some(1), (2, 1))
            .sync_record(2, 1, SyncState::Completed, 1, Some(1), (1, 1));
        run.step()
            .release(2, Released::AppendAck, Some((1, Some(1))), (2, 1));
        expect_pass(
            &run,
            "an ack of index 2 after sync 2 completed, then sync 1",
        );
    }

    #[test]
    fn a_stale_sync_completion_covers_nothing_b17() {
        let mut run = Run::new();
        run.step().append(2, 1, 1, "aa");
        run.step()
            .sync_record(2, 7, SyncState::Stale, 1, Some(1), (1, 1));
        run.step()
            .release(2, Released::AppendAck, Some((1, Some(1))), (1, 1));
        expect(&run, Kind::AckBeforeSync);
    }

    #[test]
    fn a_truncation_uncovers_the_replaced_entry_until_a_later_sync() {
        let mut run = Run::new();
        run.step()
            .append(2, 1, 1, "aa")
            .synced(2, 1, 1, Some(1), (1, 1));
        run.step().truncate(2, 1).append(2, 1, 2, "bb");
        run.step()
            .release(2, Released::AppendAck, Some((1, Some(1))), (1, 2));
        expect(&run, Kind::AckBeforeSync);
        run.step().synced(2, 2, 1, Some(1), (1, 2));
        run.step()
            .release(2, Released::AppendAck, Some((1, Some(1))), (1, 2));
        let report = judged(&run);
        assert_eq!(
            report.violations.len(),
            1,
            "rule: only the release before the later sync is flagged; report={report:#?}"
        );
    }

    #[test]
    fn a_release_naming_an_entry_the_node_does_not_hold_is_flagged() {
        let mut run = Run::new();
        run.step()
            .append(2, 1, 1, "aa")
            .synced(2, 1, 1, None, (1, 1));
        run.step().release(2, Released::AppendAck, None, (2, 1));
        run.step().release(2, Released::AppendAck, None, (1, 5));
        let report = judged(&run);
        assert_eq!(
            report.violations.len(),
            2,
            "rule: an index past the log and a term other than the entry's are both unmet; \
             report={report:#?}"
        );
    }

    #[test]
    fn a_client_success_needs_only_the_log() {
        let mut run = Run::new();
        run.step().append(1, 1, 1, "aa");
        run.step()
            .release(1, Released::ClientSuccess(1), None, (1, 1));
        expect(&run, Kind::SuccessBeforeSync);
        let mut run = Run::new();
        run.step()
            .append(1, 1, 1, "aa")
            .synced(1, 1, 7, Some(3), (1, 1));
        run.step()
            .release(1, Released::ClientSuccess(1), None, (1, 1));
        expect_pass(
            &run,
            "a success after the log sync, whatever the hard state",
        );
    }

    #[test]
    fn flags_a_published_read_before_its_log_is_durable() {
        let mut run = Run::new();
        run.step().append(1, 1, 1, "aa");
        run.step()
            .release(1, Released::PublishedRead(4), None, (1, 1));
        expect(&run, Kind::ReadBeforeSync);
    }

    #[test]
    fn a_read_of_an_empty_log_has_nothing_to_wait_for() {
        let mut run = Run::new();
        run.step()
            .release(1, Released::PublishedRead(4), None, (0, 0));
        expect_pass(&run, "a read that rests on no entries");
    }

    #[test]
    fn the_commit_index_is_never_an_obligation() {
        let mut run = Run::new();
        run.step()
            .append(1, 1, 1, "aa")
            .state(1, crate::godview::Role::Leader, 1, 1, 1);
        run.step().committed(1, 1, "aa");
        expect_pass(
            &run,
            "a commit index and a committed entry with no release at all",
        );
    }

    #[test]
    fn flags_an_acceptance_before_the_entry_is_durable() {
        let mut run = Run::new();
        run.step().append(2, 1, 1, "aa").accept(2, 1, 1, 1, "aa");
        expect(&run, Kind::AcceptBeforeSync);
        let mut run = Run::new();
        run.step()
            .append(2, 1, 1, "aa")
            .synced(2, 1, 1, None, (1, 1));
        run.step().accept(2, 1, 1, 1, "aa");
        expect_pass(&run, "an acceptance after the sync");
    }

    #[test]
    fn a_completion_without_a_request_is_malformed() {
        let mut run = Run::new();
        run.step()
            .sync_record(2, 1, SyncState::Completed, 1, None, (0, 0));
        malformed(
            &run,
            "a sync completion that names no request of its incarnation",
        );
    }

    #[test]
    fn a_sync_covering_more_than_the_log_is_malformed() {
        let mut run = Run::new();
        run.step()
            .sync_record(2, 1, SyncState::Requested, 1, None, (3, 1));
        malformed(&run, "a sync of entries the node's log does not hold");
    }

    #[test]
    fn a_sync_requested_twice_is_malformed() {
        let mut run = Run::new();
        run.step()
            .sync_record(2, 1, SyncState::Requested, 1, None, (0, 0));
        run.step()
            .sync_record(2, 1, SyncState::Requested, 1, None, (0, 0));
        malformed(&run, "a sync ID used twice");
    }

    #[test]
    fn passes_the_run_fixture_with_its_stale_sync() {
        let text = include_str!("../../../schemas/observation/fixtures/run.v1.json");
        let report = k11(&GodView::parse(text).unwrap()).unwrap();
        assert!(
            report.passed(),
            "rule: K-11 passes run.v1; report={report:#?}"
        );
        assert!(report.examined > 0, "rule: K-11 examined something");
    }

    #[test]
    fn passes_the_drain_fixture() {
        let text = include_str!("../../../schemas/observation/fixtures/drain.v1.json");
        let report = k11(&GodView::parse(text).unwrap()).unwrap();
        assert!(
            report.passed(),
            "rule: K-11 passes drain.v1; report={report:#?}"
        );
        assert!(report.examined > 0, "rule: K-11 examined something");
    }

    #[test]
    fn fails_a_fixture_whose_first_vote_names_a_vote_never_synced() {
        let text = include_str!("../../../schemas/observation/fixtures/run.v1.json");
        let mut doc: serde_json::Value = serde_json::from_str(text).unwrap();
        let records = doc["records"].as_array_mut().unwrap();
        let release = records
            .iter_mut()
            .find(|r| r["kind"] == "release" && r["what"] == "vote_grant")
            .unwrap();
        release["requires"]["hard_state"]["term"] = serde_json::json!(9999);
        let report = k11(&GodView::from_value(&doc).unwrap()).unwrap();
        assert!(
            report
                .violations
                .iter()
                .any(|v| v.kind == Kind::VoteBeforeSync),
            "rule: K-11 flags a vote before its sync in a mutated run.v1; report={report:#?}"
        );
    }

    #[test]
    fn fails_a_fixture_whose_syncs_all_turned_stale() {
        let text = include_str!("../../../schemas/observation/fixtures/drain.v1.json");
        let mut doc: serde_json::Value = serde_json::from_str(text).unwrap();
        for record in doc["records"].as_array_mut().unwrap() {
            if record["kind"] == "sync" && record["state"] == "completed" {
                record["state"] = serde_json::json!("stale");
            }
        }
        let report = k11(&GodView::from_value(&doc).unwrap()).unwrap();
        assert!(
            report.has(Kind::VoteBeforeSync) && report.has(Kind::AckBeforeSync),
            "rule: K-11 flags votes and acks that rest on stale syncs only in a mutated \
             drain.v1; report={report:#?}"
        );
    }
}
