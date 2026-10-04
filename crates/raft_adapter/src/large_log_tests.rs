//! W13.5's large-log bounds (PLAN.md §7.3; W02 audit U-1, U-2): however far commit runs ahead
//! of applied, the election and vote-commit paths read nothing from the journal, and a Ready
//! hands over committed entries only within the per-Ready bound.
#![cfg(test)]

// check: RAFT-BOUNDS

use raft::eraftpb::{self, MessageType};
use types::{Consensus, Entry, EntryPayload, HardState, LogIndex, NodeId, Role, Term};

use crate::convert::outbound;
use crate::node::RaftNode;
use crate::test_support::{Journal, Seeded, config, empty_log, entry, persist_batch};

/// Entries in the large log.
const ENTRIES: u64 = 2_000;
/// Bytes per command: the log holds about 2 MiB of commands.
const COMMAND: usize = 1_024;

/// Node 1 of `three-voters` over a journal of `ENTRIES` commands in term 1, with commit index
/// `commit` persisted and nothing applied: a node that restarts far behind.
fn far_behind(commit: u64) -> (RaftNode<Journal>, Journal) {
    let config = config("three-voters");
    let id = config.voters().next().unwrap();
    let journal = Journal::default();
    let mut log = empty_log();
    for start in (1..=ENTRIES).step_by(500) {
        let end = (start + 499).min(ENTRIES);
        let entries: Vec<Entry> = (start..=end)
            .map(|i| entry(i, 1, &[b'x'; COMMAND]))
            .collect();
        let state = HardState {
            term: Term::new(1).unwrap(),
            vote: None,
            commit: LogIndex::new(if end == ENTRIES { commit } else { 0 }).unwrap(),
        };
        persist_batch(&mut log, &journal, entries, Some(state));
    }
    let node = RaftNode::new(
        &config.consensus(id).unwrap(),
        config.voters(),
        log,
        journal.clone(),
        Seeded::new(7),
        LogIndex::ZERO,
    )
    .unwrap();
    (node, journal)
}

/// Ticks `node` until it asks for pre-votes; returns the ticks it took.
fn until_pre_candidate(node: &mut RaftNode<Journal>) -> u32 {
    (1..=1_000)
        .find(|_| {
            node.tick();
            node.status().role == Role::PreCandidate
        })
        .unwrap()
}

#[test]
fn an_election_attempt_reads_nothing_however_far_commit_runs_ahead() {
    let (mut node, journal) = far_behind(ENTRIES);
    assert_eq!(
        (node.status().commit.get(), node.applied_index().get()),
        (ENTRIES, 0),
        "fixture: {ENTRIES} committed entries, none applied"
    );
    let before = journal.bytes_read();
    let ticks = until_pre_candidate(&mut node);
    let read = journal.bytes_read() - before;
    assert_eq!(
        read,
        0,
        "rule: the election attempt's pending-configuration scan over the {ENTRIES} committed but \
         unapplied entries (about {} KiB) reads nothing from the journal (W02 audit U-1, §7.3); \
         read {read} bytes over {ticks} ticks",
        ENTRIES * COMMAND as u64 / 1_024
    );
}

#[test]
fn a_vote_that_fast_forwards_commit_reads_nothing() {
    let (mut node, journal) = far_behind(0);
    until_pre_candidate(&mut node);
    let config = config("three-voters");
    let mut voters = config.voters();
    let (me, peer) = (voters.next().unwrap(), voters.next().unwrap());
    let mut grant = eraftpb::Message::default();
    grant.set_msg_type(MessageType::MsgRequestPreVoteResponse);
    grant.from = peer.get();
    grant.to = me.get();
    grant.term = node.status().term.get() + 1;
    grant.commit = ENTRIES;
    grant.commit_term = 1;
    let bytes = outbound(&grant).unwrap().bytes;
    let before = journal.bytes_read();
    assert_eq!(
        node.step(bytes),
        Ok(()),
        "fixture: the pre-vote grant is admitted"
    );
    let read = journal.bytes_read() - before;
    assert_eq!(
        node.status().commit.get(),
        ENTRIES,
        "fixture: the grant fast-forwarded commit from 0 to {ENTRIES}, so U-2's scan ran"
    );
    assert_eq!(
        read, 0,
        "rule: the vote's commit fast-forward scans {ENTRIES} entries for pending configuration \
         changes and reads nothing from the journal (W02 audit U-2, §7.3); read {read} bytes"
    );
}

#[test]
fn a_ready_hands_over_committed_entries_only_within_the_per_ready_bound() {
    let (mut node, _journal) = far_behind(ENTRIES);
    let config = config("three-voters");
    let me: NodeId = config.voters().next().unwrap();
    let bound = config
        .consensus(me)
        .unwrap()
        .max_committed_size_per_ready()
        .get();
    let ready = node.ready().unwrap();
    let committed = ready.committed();
    let bytes: u64 = committed
        .iter()
        .map(|entry| match &entry.payload {
            EntryPayload::Command(command) => u64::try_from(command.len()).unwrap(),
            EntryPayload::Empty => 0,
        })
        .sum();
    assert!(
        !committed.is_empty() && u64::try_from(committed.len()).unwrap() < ENTRIES,
        "rule: the first Ready hands over some, not all, of the {ENTRIES} committed entries; got \
         {}",
        committed.len()
    );
    assert!(
        bytes <= bound,
        "rule: a Ready's committed entries stay within max_committed_size_per_ready ({bound} \
         bytes, §7.11; W02 audit U-3); got {bytes} bytes in {} entries",
        committed.len()
    );
}
