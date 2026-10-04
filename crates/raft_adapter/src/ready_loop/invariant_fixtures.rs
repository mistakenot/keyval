//! Fixtures that fire each of the Ready loop's own invariants (CD-7, br-kky5). A child module,
//! so it reaches the loop's private steps; every other invariant fires from the library's own
//! output in `ready_loop_tests`.
#![cfg(test)]

use journal::{FrameError, JournalError};
use types::{Effect, Entry, Incarnation, LogIndex, SyncSeq};

use super::{Apply, RaftInput, ReadyLoop};
use crate::node::RaftNode;
use crate::test_support::{Journal, Seeded, config, empty_log, entry};

/// Applies nothing; these fixtures fail before any apply matters.
struct Ignore;

impl Apply for Ignore {
    fn apply(&mut self, _entry: &Entry, _commit: LogIndex, _effects: &mut Vec<Effect>) {}
}

/// A lone voter's loop whose first sync is `first_sync`.
fn lone(first_sync: SyncSeq) -> ReadyLoop<Journal> {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    let node = RaftNode::new(
        &config.consensus(id).unwrap(),
        config.voters(),
        empty_log(),
        Journal::default(),
        Seeded::new(7),
        LogIndex::ZERO,
    )
    .unwrap();
    ReadyLoop::new(node, Incarnation::new(1), first_sync)
}

#[test]
#[should_panic(
    expected = "invariant RA-SYNC-SEQ violated: node 1: sync sequence 18446744073709551615 has \
                no successor"
)]
fn a_sync_number_with_no_successor_breaks_its_invariant() {
    let mut lp = lone(SyncSeq::new(u64::MAX));
    for _ in 0..1_000 {
        let _ = lp.feed(RaftInput::Tick, &mut Ignore, &mut Vec::new());
    }
}

#[test]
#[should_panic(
    expected = "invariant RA-APPLY-ORDER violated: node 1: entry 5 handed over for apply after \
                applied index 0"
)]
fn an_entry_out_of_order_breaks_its_invariant() {
    let mut lp = lone(SyncSeq::new(1));
    lp.apply_all(&[entry(5, 1, b"x")], &mut Ignore, &mut Vec::new());
}

#[test]
#[should_panic(
    expected = "invariant RA-APPLY-COMMITTED violated: node 1: entry 1 handed over for apply \
                beyond commit index 0"
)]
fn an_entry_beyond_the_commit_index_breaks_its_invariant() {
    let mut lp = lone(SyncSeq::new(1));
    lp.apply_all(&[entry(1, 1, b"x")], &mut Ignore, &mut Vec::new());
}

#[test]
#[should_panic(
    expected = "invariant RA-FRAME-BOUND violated: node 1: a Ready's batch overflows one frame"
)]
fn a_ready_overflowing_one_frame_breaks_its_invariant() {
    let mut lp = lone(SyncSeq::new(1));
    let _ = lp.stop(&JournalError::Frame(FrameError::PayloadTooLong {
        len: types::MAX_FRAME_PAYLOAD_BYTES + 1,
    }));
}

#[test]
#[should_panic(
    expected = "invariant RA-HISTORY violated: node 1: the library wrote a history the journal \
                refuses"
)]
fn an_impossible_history_breaks_its_invariant() {
    let mut lp = lone(SyncSeq::new(1));
    let _ = lp.stop(&JournalError::Gap {
        expected: 1,
        found: 5,
    });
}
