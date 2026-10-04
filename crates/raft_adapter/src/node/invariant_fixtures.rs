//! Fixtures that fire each of `RaftNode`'s conversion invariants (CD-7, br-kky5). A child
//! module, so it reaches the node's private conversions and the library behind them; the
//! other `RA-*` invariants fire through the public API in `node_tests`.
#![cfg(test)]

use raft::eraftpb::{self, ConfState, EntryType, MessageType};
use types::{Consensus, LogIndex, NodeId};

use super::RaftNode;
use crate::test_support::{Journal, Seeded, config, empty_log};

/// Node `id` of fixture `name`, over an empty log.
fn node(name: &str, id: u64) -> RaftNode<Journal> {
    let config = config(name);
    let id = NodeId::new(id).unwrap();
    RaftNode::new(
        &config.consensus(id).unwrap(),
        config.voters(),
        empty_log(),
        Journal::default(),
        Seeded::new(7),
        LogIndex::ZERO,
    )
    .unwrap()
}

#[test]
#[should_panic(
    expected = "invariant RA-NO-SNAPSHOT violated: node 1: the library handed over a snapshot at \
                index 10"
)]
fn a_ready_with_a_snapshot_breaks_its_invariant() {
    let config = config("three-voters");
    let voters: Vec<u64> = config.voters().map(NodeId::get).collect();
    let mut node = node("three-voters", 1);
    let mut snapshot = eraftpb::Snapshot::default();
    snapshot.mut_metadata().index = 10;
    snapshot.mut_metadata().term = 1;
    snapshot
        .mut_metadata()
        .set_conf_state(ConfState::from((voters.clone(), Vec::new())));
    let mut message = eraftpb::Message::default();
    message.set_msg_type(MessageType::MsgSnapshot);
    message.from = voters[1];
    message.to = 1;
    message.term = 1;
    message.set_snapshot(snapshot);
    // Straight into the library: admission (W13.3) refuses snapshots before they get here.
    node.raw.step(message).unwrap();
    let _ = node.ready();
}

#[test]
#[should_panic(
    expected = "invariant RA-ENTRY violated: node 1: the library produced a new entry v1 cannot \
                hold"
)]
fn a_configuration_change_entry_breaks_its_invariant() {
    let node = node("one-voter", 1);
    let mut change = eraftpb::Entry::default();
    change.set_entry_type(EntryType::EntryConfChange);
    change.index = 1;
    change.term = 1;
    let _ = node.core_entries(&[change], "a new entry");
}

#[test]
#[should_panic(
    expected = "invariant RA-MESSAGE violated: node 1: the library produced a message with no \
                valid destination or encoding"
)]
fn a_message_to_node_zero_breaks_its_invariant() {
    let node = node("one-voter", 1);
    let mut message = eraftpb::Message::default();
    message.set_msg_type(MessageType::MsgHeartbeat);
    message.to = 0;
    let _ = node.core_messages(&[message]);
}

#[test]
#[should_panic(
    expected = "invariant RA-HARD-STATE violated: node 1: the library produced an out-of-range \
                hard state: term 9007199254740992"
)]
fn a_term_past_two_to_the_53_breaks_its_invariant() {
    let node = node("one-voter", 1);
    let state = eraftpb::HardState {
        term: 1 << 53,
        ..eraftpb::HardState::default()
    };
    let _ = node.core_hard_state(&state);
}

#[test]
#[should_panic(
    expected = "invariant RA-BATCH violated: node 1: a Ready that must sync is no \
                           journal batch"
)]
fn an_oversized_command_breaks_its_invariant() {
    let mut node = node("one-voter", 1);
    for _ in 0..1_000 {
        node.tick();
        if let Some(ready) = node.ready() {
            if node.persist().unwrap().is_some() {
                node.synced();
            }
            node.advance();
            let _ = ready;
        }
        if node.status().role == types::Role::Leader {
            break;
        }
    }
    // Over the 66 KiB command bound (§7.11): the controller never proposes one (W14).
    node.propose(vec![0; 70_000]).unwrap();
    let _ = node.ready();
}

#[test]
#[should_panic(
    expected = "invariant RA-APPLIED-RANGE violated: node 1: applied index 5 reported outside \
                [applied 0, commit 0]"
)]
fn an_applied_index_past_the_commit_breaks_its_invariant() {
    let mut node = node("one-voter", 1);
    node.applied(LogIndex::new(5).unwrap());
}

#[test]
#[should_panic(
    expected = "invariant RA-INDEX-RANGE violated: node 1: the library reported commit index \
                9007199254740992, past 2^53 - 1"
)]
fn an_index_past_two_to_the_53_breaks_its_invariant() {
    let _ = super::log_index(NodeId::new(1).unwrap(), 1 << 53, "commit");
}

#[test]
#[should_panic(
    expected = "invariant RA-TERM-RANGE violated: node 1: the library reported term \
                9007199254740992, past 2^53 - 1"
)]
fn a_status_term_past_two_to_the_53_breaks_its_invariant() {
    let _ = super::term(NodeId::new(1).unwrap(), 1 << 53);
}
