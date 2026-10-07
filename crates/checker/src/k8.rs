//! K-8, Raft safety (PLAN.md §10.5; C-1, C-2): on the god view after every step, election
//! safety, log matching, leader completeness, state-machine safety and leader append-only.
//!
//! Commitment is judged by **commit certificates**, recomputed here from the `accept` records
//! and never taken from the Raft library or from a node's own commit index: an entry of term T
//! is certified once a majority of the voters accepted it **during term T**, and the prefix
//! before it is certified with it. An entry of an old term that sits on a majority of disks is
//! not thereby committed: its acceptances happened in later terms, so they never count. A
//! certificate is kept for good, so the loss of a certifying node never revokes it.
//!
//! The god view is rebuilt from the records alone: a node's log is its `recovered` log changed
//! by its `append` and `truncate` records, and its state is its latest `node_state`. The
//! harness's own `committed` records are a cross-check, not evidence. The unit examined is one
//! god-view record (a `recovered`, `append`, `truncate`, `accept`, `committed` or `node_state`).
//!
//! Limits: an acceptance counts by its recorded terms (`acceptance_term == entry_term`), not by
//! checking that the entry came from the term's leader, and the payload digest is trusted, not
//! recomputed. A vote counts only once durable: one lost with a crash may be cast again.
//! `committed` records are judged where they stand and must follow their certificate; the
//! node-state checks wait for the end of the step.
//!
//! Properties, each with its violation [`Kind`]:
//!
//! - **Election safety**: at most one node leads a term ([`Kind::TwoLeadersInTerm`]), and no node
//!   votes for two candidates in one term ([`Kind::DoubleVote`], the evidence of B-16). Leaders
//!   that overlap in time in different terms are allowed.
//! - **Log matching**, on entry bytes and term, never on value hashes: two logs holding the same
//!   term at an index hold the same bytes ([`Kind::SameTermDifferentBytes`]) and the same entries
//!   before it ([`Kind::PrefixDiffersUnderSameTerm`]).
//! - **Leader completeness**: a leader's log, when it takes office in term T, holds every entry
//!   certified in an earlier term ([`Kind::LeaderMissingCommitted`]).
//! - **State-machine safety**: no node commits or applies past the certified prefix
//!   ([`Kind::CommitBeyondCertificate`]), a node's log agrees with the certified entries within
//!   its commit index ([`Kind::NodeDivergesFromCertified`]), two nodes at one applied index hold
//!   one state digest ([`Kind::StateDigestDiverges`]), and no two entries are certified at one
//!   index ([`Kind::ConflictingCertificates`]). A `committed` record with no certificate behind
//!   it is [`Kind::CommittedWithoutCertificate`].
//! - **Leader append-only**: a node never removes log entries while it leads
//!   ([`Kind::LeaderTruncated`]).

use std::collections::{BTreeMap, BTreeSet};

use crate::godview::{Event, GodView, NodeState, Process, Role, Stamped};
use crate::history::LogEntry;
use crate::report::{CheckError, Kind, Report, Violation};

/// Judges `view` against K-8.
///
/// # Errors
///
/// [`CheckError::Malformed`] if the stream breaks a producer guarantee the judgement relies on:
/// an append that does not extend its log by one, a truncation of entries a log does not hold, an
/// acceptance by a node that is not a voter or of an entry its log does not hold.
pub fn k8(view: &GodView) -> Result<Report, CheckError> {
    let mut judge = Judge::new(view);
    let mut current: Option<(u64, String)> = None;
    for stamped in &view.events {
        if let Some((seq, at)) = &current
            && *seq != stamped.seq
        {
            judge.end_step(*seq, at);
        }
        judge.observe(stamped)?;
        current = Some((stamped.seq, stamped.at.clone()));
    }
    if let Some((seq, at)) = &current {
        judge.end_step(*seq, at);
    }
    Ok(Report {
        checker: "K-8",
        examined: judge.examined,
        violations: judge.violations,
    })
}

/// The most violations a report lists (CD-9): the judge is an offline oracle whose other state
/// is bounded by the size of the document it reads, so only the report needs a cap, and a
/// run that breaks safety this often has failed already.
const MAX_VIOLATIONS: usize = 256;

/// A certificate group: the entry's index, its term and the digest of its payload.
type GroupKey = (u64, u64, String);

struct Judge {
    voters: BTreeSet<u64>,
    majority: usize,
    logs: BTreeMap<u64, Vec<LogEntry>>,
    states: BTreeMap<u64, NodeState>,
    leaders: BTreeMap<u64, BTreeSet<u64>>,
    elected: BTreeSet<(u64, u64)>,
    votes: BTreeMap<(u64, u64), u64>,
    digests: BTreeMap<u64, (String, u64)>,
    groups: BTreeMap<GroupKey, BTreeSet<u64>>,
    certified_groups: BTreeSet<GroupKey>,
    /// The certified prefix: `certified[i]` is the entry at index `i + 1`.
    certified: Vec<LogEntry>,
    /// Per node pair, how many leading entries are known equal (a cache, never evidence).
    agree: BTreeMap<(u64, u64), usize>,
    reported_pairs: BTreeSet<(u64, u64)>,
    /// Per node, how many leading entries are known equal to the certified ones.
    verified: BTreeMap<u64, usize>,
    reported_divergence: BTreeSet<(u64, usize)>,
    reported_leaders: BTreeSet<(u64, u64)>,
    dirty: BTreeSet<u64>,
    violations: Vec<Violation>,
    examined: u64,
}

impl Judge {
    fn new(view: &GodView) -> Self {
        Self {
            voters: view.voters.clone(),
            majority: view.voters.len() / 2 + 1,
            logs: BTreeMap::new(),
            states: BTreeMap::new(),
            leaders: BTreeMap::new(),
            elected: BTreeSet::new(),
            votes: BTreeMap::new(),
            digests: BTreeMap::new(),
            groups: BTreeMap::new(),
            certified_groups: BTreeSet::new(),
            certified: Vec::new(),
            agree: BTreeMap::new(),
            reported_pairs: BTreeSet::new(),
            verified: BTreeMap::new(),
            reported_divergence: BTreeSet::new(),
            reported_leaders: BTreeSet::new(),
            dirty: BTreeSet::new(),
            violations: Vec::new(),
            examined: 0,
        }
    }

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
        self.examined += 1;
        let at = stamped.at.as_str();
        match &stamped.event {
            Event::Recovered {
                node,
                log,
                durable_term,
                durable_vote,
            } => {
                // A vote that was never made durable is lost with the crash and may be cast
                // again; only the durable one binds the node (B-16 is a durable vote forgotten).
                self.votes
                    .retain(|(voter, term), _| voter != node || term < durable_term);
                if let Some(candidate) = durable_vote {
                    self.votes.insert((*node, *durable_term), *candidate);
                }
                self.logs.insert(*node, log.clone());
                self.verified.insert(*node, 0);
                self.cap_agreement(*node, 0);
                self.dirty.insert(*node);
                self.log_matching(*node, at);
            }
            Event::Append { node, index, entry } => {
                let log = self.logs.entry(*node).or_default();
                if *index != log.len() as u64 + 1 {
                    return Err(CheckError::malformed(
                        at,
                        format!(
                            "node {node} appended index {index} to a log of {} entries",
                            log.len()
                        ),
                    ));
                }
                log.push(entry.clone());
                self.dirty.insert(*node);
                self.log_matching(*node, at);
            }
            Event::Truncate { node, from } => self.truncate(*node, *from, at)?,
            Event::Accept {
                node,
                index,
                entry_term,
                acceptance_term,
                digest,
            } => self.accept(*node, (*index, *entry_term, digest), *acceptance_term, at)?,
            Event::Committed {
                index,
                term,
                payload,
            } => self.committed(*index, *term, payload, at),
            Event::State { node, state } => self.state(*node, state, at),
        }
        Ok(())
    }

    fn truncate(&mut self, node: u64, from: u64, at: &str) -> Result<(), CheckError> {
        if let Some(state) = self.states.get(&node)
            && state.role == Role::Leader
            && [Process::Running, Process::Paused].contains(&state.process)
        {
            let term = state.term;
            self.flag(
                Kind::LeaderTruncated,
                at,
                format!(
                    "rule: K-8 leader append-only; node {node} removed its log from index \
                     {from} while it led term {term}"
                ),
            );
        }
        let len = self.logs.get(&node).map_or(0, Vec::len);
        let keep = usize::try_from(from)
            .ok()
            .and_then(|from| from.checked_sub(1))
            .filter(|keep| *keep < len)
            .ok_or_else(|| {
                CheckError::malformed(
                    at,
                    format!("node {node} truncated from index {from} of a log of {len} entries"),
                )
            })?;
        if let Some(log) = self.logs.get_mut(&node) {
            log.truncate(keep);
        }
        if let Some(verified) = self.verified.get_mut(&node) {
            *verified = (*verified).min(keep);
        }
        self.cap_agreement(node, keep);
        self.dirty.insert(node);
        Ok(())
    }

    fn accept(
        &mut self,
        node: u64,
        (index, entry_term, digest): (u64, u64, &String),
        acceptance_term: u64,
        at: &str,
    ) -> Result<(), CheckError> {
        if !self.voters.contains(&node) {
            return Err(CheckError::malformed(
                at,
                format!("node {node} accepted an entry but is not a voter"),
            ));
        }
        // Acceptance outside the certifying term counts for nothing (§10.5).
        if acceptance_term != entry_term {
            return Ok(());
        }
        let key = (index, entry_term, digest.clone());
        let group = self.groups.entry(key.clone()).or_default();
        group.insert(node);
        if group.len() >= self.majority && self.certified_groups.insert(key) {
            self.certify(node, index, entry_term, at)?;
        }
        Ok(())
    }

    /// Certifies the entry at `index` and the prefix before it, from the log of the replica
    /// whose acceptance completed the majority.
    fn certify(
        &mut self,
        node: u64,
        index: u64,
        entry_term: u64,
        at: &str,
    ) -> Result<(), CheckError> {
        let held = usize::try_from(index)
            .ok()
            .filter(|index| *index > 0)
            .and_then(|index| {
                self.logs
                    .get(&node)
                    .filter(|log| log.get(index - 1).is_some_and(|e| e.term == entry_term))
                    .map(|log| log[..index].to_vec())
            })
            .ok_or_else(|| {
                CheckError::malformed(
                    at,
                    format!(
                        "node {node} accepted index {index} of term {entry_term} that its log \
                         does not hold"
                    ),
                )
            })?;
        for (position, entry) in held.into_iter().enumerate() {
            match self.certified.get(position) {
                None => self.certified.push(entry),
                Some(known) if *known == entry => {}
                Some(known) => {
                    let detail = format!(
                        "rule: K-8 one entry is certified per index; index {} is certified as \
                         {known:?} and again, through node {node}'s log, as {entry:?}",
                        position + 1
                    );
                    self.flag(Kind::ConflictingCertificates, at, detail);
                }
            }
        }
        // A new certificate binds the leaders already in office too.
        let elected: Vec<(u64, u64)> = self.elected.iter().copied().collect();
        for (leader, term) in elected {
            self.leader_completeness(leader, term, at);
        }
        Ok(())
    }

    fn committed(&mut self, index: u64, term: u64, payload: &str, at: &str) {
        let supported = usize::try_from(index)
            .ok()
            .and_then(|index| index.checked_sub(1))
            .and_then(|position| self.certified.get(position))
            .is_some_and(|entry| entry.term == term && entry.payload == payload);
        if !supported {
            self.flag(
                Kind::CommittedWithoutCertificate,
                at,
                format!(
                    "rule: K-8 an entry is committed only by a commit certificate; the harness \
                     committed index {index} (term {term}, payload {payload:?}) with {} entries \
                     certified",
                    self.certified.len()
                ),
            );
        }
    }

    fn state(&mut self, node: u64, state: &NodeState, at: &str) {
        if [Process::Running, Process::Paused].contains(&state.process)
            && state.role == Role::Leader
        {
            let leaders = self.leaders.entry(state.term).or_default();
            if leaders.insert(node) && leaders.len() > 1 {
                let all: Vec<u64> = leaders.iter().copied().collect();
                self.flag(
                    Kind::TwoLeadersInTerm,
                    at,
                    format!(
                        "rule: K-8 election safety, at most one leader per term; nodes {all:?} \
                         all led term {}",
                        state.term
                    ),
                );
            }
        }
        if let Some(candidate) = state.vote {
            let known = *self.votes.entry((node, state.term)).or_insert(candidate);
            if known != candidate {
                self.flag(
                    Kind::DoubleVote,
                    at,
                    format!(
                        "rule: K-8 election safety, one vote per term; node {node} voted for \
                         {known} and then for {candidate} in term {}",
                        state.term
                    ),
                );
            }
        }
        if let Some((digest, other)) = self.digests.get(&state.applied) {
            if *digest != state.digest {
                let detail = format!(
                    "rule: K-8 state-machine safety, one state per applied index; node {node} \
                     at applied index {} has digest {}, node {other} had {digest}",
                    state.applied, state.digest
                );
                self.flag(Kind::StateDigestDiverges, at, detail);
            }
        } else {
            self.digests
                .insert(state.applied, (state.digest.clone(), node));
        }
        if state.process == Process::Running
            && state.role == Role::Leader
            && self.elected.insert((node, state.term))
        {
            self.leader_completeness(node, state.term, at);
        }
        self.states.insert(node, state.clone());
        self.dirty.insert(node);
    }

    /// A leader taking office in `term` holds every entry certified in an earlier term.
    fn leader_completeness(&mut self, node: u64, term: u64, at: &str) {
        let log = self.logs.get(&node);
        let missing = self.certified.iter().enumerate().find(|(position, entry)| {
            entry.term < term && log.and_then(|l| l.get(*position)) != Some(*entry)
        });
        if let Some((position, entry)) = missing
            && self.reported_leaders.insert((node, term))
        {
            let held = log.and_then(|l| l.get(position)).cloned();
            let detail = format!(
                "rule: K-8 leader completeness; node {node} took office in term {term} without \
                 the entry certified at index {} ({entry:?}); its log holds {held:?} there",
                position + 1
            );
            self.flag(Kind::LeaderMissingCommitted, at, detail);
        }
    }

    /// Log matching between `node` and every other node (on bytes and term).
    fn log_matching(&mut self, node: u64, at: &str) {
        let others: Vec<u64> = self.logs.keys().copied().filter(|o| *o != node).collect();
        for other in others {
            let key = (node.min(other), node.max(other));
            if self.reported_pairs.contains(&key) {
                continue;
            }
            let (Some(a), Some(b)) = (self.logs.get(&key.0), self.logs.get(&key.1)) else {
                continue;
            };
            let common = a.len().min(b.len());
            let mut agree = self.agree.get(&key).copied().unwrap_or(0).min(common);
            while agree < common && a.get(agree) == b.get(agree) {
                agree += 1;
            }
            let same_term =
                (agree..common).find(|i| a.get(*i).map(|e| e.term) == b.get(*i).map(|e| e.term));
            let finding = same_term.map(|i| {
                (
                    if i == agree {
                        Kind::SameTermDifferentBytes
                    } else {
                        Kind::PrefixDiffersUnderSameTerm
                    },
                    format!(
                        "rule: K-8 log matching on entry bytes and term; nodes {} and {} both \
                         hold term {} at index {}, but their logs first differ at index {}: \
                         {:?} against {:?}",
                        key.0,
                        key.1,
                        a.get(i).map_or(0, |e| e.term),
                        i + 1,
                        agree + 1,
                        a.get(agree),
                        b.get(agree)
                    ),
                )
            });
            self.agree.insert(key, agree);
            if let Some((kind, detail)) = finding {
                self.reported_pairs.insert(key);
                self.flag(kind, at, detail);
            }
        }
    }

    fn cap_agreement(&mut self, node: u64, cap: usize) {
        for (key, agree) in &mut self.agree {
            if key.0 == node || key.1 == node {
                *agree = (*agree).min(cap);
            }
        }
    }

    /// The state-machine checks that compare a node with the certified prefix, once the step's
    /// records are all in (a certificate and the commit it supports share a step's causal order
    /// only loosely, so a step is judged whole).
    fn end_step(&mut self, seq: u64, at: &str) {
        let dirty = std::mem::take(&mut self.dirty);
        let certified = self.certified.len() as u64;
        for node in dirty {
            let Some(state) = self.states.get(&node).cloned() else {
                continue;
            };
            if state.process != Process::Running {
                continue;
            }
            if state.commit > certified || state.applied > certified {
                self.flag(
                    Kind::CommitBeyondCertificate,
                    at,
                    format!(
                        "rule: K-8 an entry is committed only by a commit certificate; after \
                         step {seq} node {node} (term {}) has commit index {} and applied index \
                         {}, but only {certified} entries are certified",
                        state.term, state.commit, state.applied
                    ),
                );
            }
            let limit = usize::try_from(state.commit.max(state.applied))
                .unwrap_or(usize::MAX)
                .min(self.certified.len());
            let mut verified = self.verified.get(&node).copied().unwrap_or(0);
            while verified < limit {
                let held = self.logs.get(&node).and_then(|log| log.get(verified));
                if held == self.certified.get(verified) {
                    verified += 1;
                    continue;
                }
                if self.reported_divergence.insert((node, verified)) {
                    let detail = format!(
                        "rule: K-8 state-machine safety; after step {seq} node {node} has commit \
                         index {} but its log holds {held:?} at index {}, where {:?} is certified",
                        state.commit,
                        verified + 1,
                        self.certified.get(verified)
                    );
                    self.flag(Kind::NodeDivergesFromCertified, at, detail);
                }
                break;
            }
            self.verified.insert(node, verified);
        }
    }
}

#[cfg(test)]
mod tests {
    // check: K-8
    use super::k8;
    use crate::godview::{GodView, Process, Role};
    use crate::raft_fixtures::Run;
    use crate::report::{CheckError, Kind, Report};

    const ALL: [u64; 5] = [1, 2, 3, 4, 5];
    const MAJORITY: [u64; 3] = [1, 2, 3];

    fn judged(run: &Run) -> Report {
        k8(&run.view()).unwrap()
    }

    fn kinds(report: &Report) -> Vec<Kind> {
        report.violations.iter().map(|v| v.kind).collect()
    }

    fn expect(run: &Run, kind: Kind) {
        let report = judged(run);
        assert!(
            kinds(&report).contains(&kind),
            "rule: K-8 flags {kind}; report={report:#?}"
        );
    }

    fn expect_pass(run: &Run, what: &str) {
        let report = judged(run);
        assert!(
            report.passed(),
            "rule: K-8 passes {what}; report={report:#?}"
        );
    }

    /// Node 1 leads term 1 and a majority holds and has accepted entry 1.
    fn committed_once() -> Run {
        let mut run = Run::new();
        run.step().state(1, Role::Leader, 1, 0, 0);
        run.step().replicate(&MAJORITY, 1, 1, "01a1");
        run.step()
            .committed(1, 1, "01a1")
            .state(1, Role::Leader, 1, 1, 1);
        run
    }

    #[test]
    fn passes_an_entry_committed_by_a_majority_certificate() {
        let run = committed_once();
        let report = judged(&run);
        assert!(report.passed(), "rule: K-8 pass; report={report:#?}");
        assert_eq!(report.examined, 5 + 1 + 6 + 2, "records judged");
    }

    #[test]
    fn a_commit_index_recorded_before_its_certificate_within_one_step_passes() {
        let mut run = Run::new();
        run.step()
            .state(1, Role::Leader, 1, 1, 1)
            .replicate(&MAJORITY, 1, 1, "x");
        expect_pass(
            &run,
            "a node_state commit index recorded before its certificate within one step",
        );
    }

    #[test]
    fn passes_the_run_fixture() {
        let text = include_str!("../../../schemas/observation/fixtures/run.v1.json");
        let report = k8(&GodView::parse(text).unwrap()).unwrap();
        assert!(
            report.passed(),
            "rule: K-8 passes run.v1; report={report:#?}"
        );
        assert!(report.examined > 0, "rule: K-8 examined something");
    }

    #[test]
    fn passes_the_drain_fixture_with_an_old_term_entry_committed_by_prefix() {
        let text = include_str!("../../../schemas/observation/fixtures/drain.v1.json");
        let report = k8(&GodView::parse(text).unwrap()).unwrap();
        assert!(
            report.passed(),
            "rule: K-8 passes drain.v1; report={report:#?}"
        );
        assert!(report.examined > 0, "rule: K-8 examined something");
    }

    #[test]
    fn fails_a_fixture_whose_commit_index_runs_past_its_certificates() {
        let text = include_str!("../../../schemas/observation/fixtures/run.v1.json");
        let mut doc: serde_json::Value = serde_json::from_str(text).unwrap();
        let records = doc["records"].as_array_mut().unwrap();
        let node_state = records
            .iter_mut()
            .rfind(|r| r["kind"] == "node_state" && r["process"] == "running")
            .unwrap();
        node_state["commit"] = serde_json::json!(9999);
        let report = k8(&GodView::from_value(&doc).unwrap()).unwrap();
        assert!(
            kinds(&report).contains(&Kind::CommitBeyondCertificate),
            "rule: K-8 flags a commit past the certificates in a mutated run.v1; report={report:#?}"
        );
    }

    #[test]
    fn flags_two_leaders_in_one_term() {
        let mut run = Run::new();
        run.step().state(1, Role::Leader, 3, 0, 0);
        run.step().state(2, Role::Leader, 3, 0, 0);
        expect(&run, Kind::TwoLeadersInTerm);
    }

    #[test]
    fn leaders_of_different_terms_may_overlap_in_time() {
        let mut run = Run::new();
        run.step().state(1, Role::Leader, 2, 0, 0);
        run.step().state(2, Role::Leader, 3, 0, 0);
        expect_pass(&run, "an old leader and a new leader of later terms");
    }

    #[test]
    fn flags_a_vote_for_two_candidates_in_one_term() {
        let mut run = Run::new();
        run.step().voted(3, 2, 1);
        run.step().voted(3, 2, 2);
        expect(&run, Kind::DoubleVote);
    }

    #[test]
    fn flags_the_same_term_with_different_bytes() {
        let mut run = Run::new();
        run.step().append(1, 1, 1, "aa").append(2, 1, 1, "bb");
        expect(&run, Kind::SameTermDifferentBytes);
    }

    #[test]
    fn flags_a_prefix_that_differs_under_a_matching_later_term() {
        let mut run = Run::new();
        // Both hold term 2 at index 2, but their first entries differ in term.
        run.step()
            .append(1, 1, 1, "aa")
            .append(1, 2, 2, "cc")
            .append(2, 1, 2, "bb")
            .append(2, 2, 2, "cc");
        expect(&run, Kind::PrefixDiffersUnderSameTerm);
    }

    #[test]
    fn different_terms_at_one_index_do_not_violate_log_matching() {
        let mut run = Run::new();
        run.step().append(1, 1, 1, "aa").append(2, 1, 2, "bb");
        expect_pass(&run, "uncommitted entries of different terms at one index");
    }

    #[test]
    fn flags_a_leader_that_took_office_without_a_certified_entry() {
        let mut run = committed_once();
        run.step().replicate(&[4, 5], 1, 1, "01a1");
        // Node 4 now holds the entry; node 5 wipes it before leading term 2.
        run.step().truncate(5, 1);
        run.step().state(5, Role::Leader, 2, 0, 0);
        expect(&run, Kind::LeaderMissingCommitted);
    }

    #[test]
    fn a_leader_holding_every_certified_entry_passes() {
        let mut run = committed_once();
        run.step().state(2, Role::Leader, 2, 0, 0);
        expect_pass(&run, "a new leader that holds the certified entry");
    }

    #[test]
    fn flags_a_commit_index_beyond_the_certified_prefix() {
        let mut run = Run::new();
        run.step().append(1, 1, 1, "aa").append(2, 1, 1, "aa");
        run.step().state(1, Role::Leader, 1, 1, 1);
        expect(&run, Kind::CommitBeyondCertificate);
    }

    #[test]
    fn an_old_term_entry_on_a_majority_is_not_committed_by_itself() {
        let mut run = Run::new();
        // Term-1 entry on three disks, accepted in term 2 (re-replication): no certificate.
        run.step();
        for node in MAJORITY {
            run.append(node, 1, 1, "old").accept(node, 1, 1, 2, "old");
        }
        run.step().state(1, Role::Leader, 2, 1, 1);
        expect(&run, Kind::CommitBeyondCertificate);
    }

    #[test]
    fn a_harness_commit_without_a_certificate_is_flagged() {
        let mut run = Run::new();
        run.step().append(1, 1, 1, "aa").accept(1, 1, 1, 1, "aa");
        run.step().committed(1, 1, "aa");
        expect(&run, Kind::CommittedWithoutCertificate);
    }

    #[test]
    fn a_certificate_outlives_the_permanent_loss_of_its_nodes() {
        let mut run = committed_once();
        run.step().lost(1).lost(2).lost(3);
        run.step().state(4, Role::Leader, 2, 1, 1);
        // Node 4 never held the entry, so completeness fails although every holder is lost.
        expect(&run, Kind::LeaderMissingCommitted);
    }

    #[test]
    fn a_node_whose_log_disagrees_with_the_certified_entry_within_its_commit_is_flagged() {
        let mut run = committed_once();
        run.step().append(5, 1, 2, "other");
        run.step().state(5, Role::Follower, 2, 1, 0);
        expect(&run, Kind::NodeDivergesFromCertified);
    }

    #[test]
    fn flags_two_state_digests_at_one_applied_index() {
        let mut run = committed_once();
        let state = crate::godview::NodeState {
            process: Process::Running,
            role: Role::Follower,
            term: 1,
            vote: None,
            commit: 1,
            applied: 1,
            digest: "state-other".to_owned(),
        };
        run.step().state_digest(2, state);
        expect(&run, Kind::StateDigestDiverges);
    }

    #[test]
    fn flags_a_leader_that_removes_log_entries() {
        let mut run = Run::new();
        run.step()
            .append(1, 1, 1, "aa")
            .state(1, Role::Leader, 1, 0, 0);
        run.step().truncate(1, 1);
        expect(&run, Kind::LeaderTruncated);
    }

    #[test]
    fn a_follower_may_truncate_its_log() {
        let mut run = Run::new();
        run.step()
            .append(2, 1, 1, "aa")
            .state(2, Role::Follower, 1, 0, 0);
        run.step().truncate(2, 1);
        expect_pass(&run, "a follower truncating an uncertified suffix");
    }

    #[test]
    fn flags_two_entries_certified_at_one_index() {
        let mut run = Run::new();
        run.step().replicate(&MAJORITY, 1, 1, "aa");
        // Nodes 3, 4 and 5 vouch for different bytes at the same index and term.
        run.step().accept(3, 1, 1, 1, "bb");
        run.step().append(4, 1, 1, "bb").accept(4, 1, 1, 1, "bb");
        run.step().append(5, 1, 1, "bb").accept(5, 1, 1, 1, "bb");
        expect(&run, Kind::ConflictingCertificates);
    }

    #[test]
    fn a_stream_that_skips_an_index_is_malformed_not_a_pass() {
        let mut run = Run::new();
        run.step().append(1, 2, 1, "aa");
        let error = k8(&run.view()).unwrap_err();
        assert!(
            matches!(error, CheckError::Malformed { .. }),
            "rule: a gap in a node's log is a malformed document; error={error:?}"
        );
    }

    #[test]
    fn a_full_replication_to_all_voters_passes() {
        let mut run = Run::new();
        run.step().state(1, Role::Leader, 1, 0, 0);
        run.step().replicate(&ALL, 1, 1, "aa");
        run.step().state(1, Role::Leader, 1, 1, 1);
        expect_pass(&run, "an entry on all five voters");
    }

    #[test]
    fn a_leader_that_took_office_before_the_certificate_formed_is_still_flagged() {
        let mut run = Run::new();
        run.step().state(5, Role::Leader, 2, 0, 0);
        // A term-1 majority certifies entry 1 after node 5 took office without it.
        run.step().replicate(&MAJORITY, 1, 1, "01a1");
        expect(&run, Kind::LeaderMissingCommitted);
    }

    #[test]
    fn a_vote_lost_with_an_unsynced_crash_may_be_cast_again() {
        let mut run = Run::new();
        run.step().voted(3, 2, 1);
        run.step().recover(3, 1, None);
        run.step().voted(3, 2, 2);
        expect_pass(
            &run,
            "a vote that never reached the disk, cast again after a restart",
        );
    }

    #[test]
    fn a_durable_vote_forgotten_across_a_restart_is_a_double_vote() {
        let mut run = Run::new();
        run.step().voted(3, 2, 1);
        run.step().recover(3, 2, Some(1));
        run.step().voted(3, 2, 2);
        expect(&run, Kind::DoubleVote);
    }

    #[test]
    fn votes_in_different_terms_for_different_candidates_pass() {
        let mut run = Run::new();
        run.step().voted(3, 2, 1);
        run.step().voted(3, 3, 2);
        expect_pass(&run, "one vote per term for different candidates");
    }

    #[test]
    fn a_harness_commit_with_the_wrong_payload_is_flagged() {
        let mut run = committed_once();
        run.step().committed(1, 1, "other");
        expect(&run, Kind::CommittedWithoutCertificate);
    }

    #[test]
    fn flags_an_applied_index_beyond_the_certified_prefix() {
        let mut run = Run::new();
        run.step().replicate(&MAJORITY, 1, 1, "aa");
        run.step().state(1, Role::Follower, 1, 1, 2);
        expect(&run, Kind::CommitBeyondCertificate);
    }

    #[test]
    fn flags_a_paused_leader_that_removes_log_entries() {
        let mut run = Run::new();
        run.step().append(1, 1, 1, "aa");
        run.step().state_digest(
            1,
            crate::godview::NodeState {
                process: Process::Paused,
                role: Role::Leader,
                term: 1,
                vote: None,
                commit: 0,
                applied: 0,
                digest: "state-0".to_owned(),
            },
        );
        run.step().truncate(1, 1);
        expect(&run, Kind::LeaderTruncated);
    }

    #[test]
    fn an_old_term_entry_is_committed_by_a_later_term_certificate() {
        let mut run = Run::new();
        run.step();
        for node in MAJORITY {
            run.append(node, 1, 1, "old").accept(node, 1, 1, 2, "old");
        }
        run.step().replicate(&MAJORITY, 2, 2, "new");
        run.step().state(1, Role::Leader, 2, 2, 2);
        expect_pass(
            &run,
            "an old-term entry committed through a current-term certificate",
        );
    }

    #[test]
    fn a_truncation_past_the_end_of_a_log_is_malformed() {
        let mut run = Run::new();
        run.step().append(1, 1, 1, "aa");
        run.step().truncate(1, 2);
        let error = k8(&run.view()).unwrap_err();
        assert!(
            matches!(error, CheckError::Malformed { .. }),
            "rule: truncating entries a log does not hold is malformed; error={error:?}"
        );
    }
}
