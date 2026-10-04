//! W13.1's tests of [`RaftNode`] through the Consensus port (PLAN.md §7.3, §7.4, D-78).
#![cfg(test)]

// check: RAFT-ADAPTER

use raft::eraftpb::{self, MessageType};
use types::{
    Config, Consensus, Entry, EntryPayload, LightReady, LogIndex, NodeId, ProposeError, Ready,
    Role, StepError,
};

use crate::convert::decode_message;
use crate::node::{ConstructError, RaftNode};
use crate::test_support::{CONFIGS, Journal, Seeded, config, empty_log};

fn node(config: &Config, id: u64, journal: &Journal, seed: u64) -> RaftNode<Journal> {
    let id = NodeId::new(id).unwrap();
    RaftNode::new(
        &config.consensus(id).unwrap(),
        config.voters(),
        empty_log(),
        journal.clone(),
        Seeded::new(seed),
        LogIndex::ZERO,
    )
    .unwrap()
}

/// One turn of a node's loop: take a Ready, persist its frame into `journal`, advance, and
/// report everything committed as applied. `None` when there is no Ready.
fn turn(node: &mut RaftNode<Journal>, journal: &Journal) -> Option<(Ready, LightReady)> {
    let ready = node.ready()?;
    let frame = node.persist().unwrap();
    assert_eq!(
        frame.is_some(),
        ready.must_sync(),
        "rule: a Ready writes one frame exactly when it must sync (§7.4 step 3)"
    );
    if let Some(frame) = frame {
        journal.push(&frame);
    }
    let light = node.advance();
    if let Some(last) = ready.committed().iter().chain(&light.committed).last() {
        node.applied(last.index.index());
    }
    Some((ready, light))
}

/// Ticks a lone voter until it leads, running every Ready; returns the ticks it took.
fn elect(node: &mut RaftNode<Journal>, journal: &Journal) -> u32 {
    for tick in 1..=1_000 {
        node.tick();
        while turn(node, journal).is_some() {}
        if node.status().role == Role::Leader {
            return tick;
        }
    }
    panic!(
        "a lone voter never became leader; status={:?}",
        node.status()
    );
}

#[test]
fn every_committed_config_constructs_a_raw_node_for_each_voter() {
    for (name, _) in CONFIGS {
        let config = config(name);
        for voter in config.voters() {
            let built = RaftNode::new(
                &config.consensus(voter).unwrap(),
                config.voters(),
                empty_log(),
                Journal::default(),
                Seeded::new(1),
                LogIndex::ZERO,
            );
            assert!(
                built.is_ok(),
                "rule: every config the parser accepts constructs a RawNode (D-78); \
                 config={name} node={voter} error={:?}",
                built.err()
            );
        }
    }
}

#[test]
fn a_lone_voter_elects_itself_and_commits_a_proposal() {
    let config = config("one-voter");
    let journal = Journal::default();
    let mut node = node(&config, 1, &journal, 7);
    assert_eq!(
        node.status().role,
        Role::Follower,
        "rule: a node starts as follower"
    );
    elect(&mut node, &journal);
    let command = b"a canonical command".to_vec();
    assert_eq!(
        node.propose(command.clone()),
        Ok(()),
        "rule: the leader accepts a proposal"
    );
    let mut committed: Vec<Entry> = Vec::new();
    while let Some((ready, light)) = turn(&mut node, &journal) {
        committed.extend(ready.committed().iter().cloned());
        committed.extend(light.committed);
    }
    let applied = committed
        .iter()
        .find(|entry| entry.payload == EntryPayload::Command(command.clone()))
        .unwrap_or_else(|| panic!("rule: the proposal commits; committed={committed:?}"));
    let status = node.status();
    assert_eq!(
        (status.role, status.leader, status.commit),
        (Role::Leader, NodeId::new(1).ok(), applied.index.index()),
        "rule: status reports the leader and the commit index (O-1)"
    );
    assert_eq!(
        node.storage().log().last_index(),
        applied.index.index(),
        "rule: the committed entry is in the journal's logical log"
    );
    assert!(
        !journal.is_empty(),
        "rule: persisted frames reached the journal"
    );
    assert_eq!(
        status
            .progress
            .iter()
            .map(|p| (p.node, p.matched))
            .collect::<Vec<_>>(),
        vec![(NodeId::new(1).unwrap(), applied.index.index())],
        "rule: the leader reports each voter's progress"
    );
}

#[test]
fn the_same_seed_gives_the_same_election_and_frames() {
    let run = |seed| {
        let journal = Journal::default();
        let mut node = node(&config("one-voter"), 1, &journal, seed);
        let ticks = elect(&mut node, &journal);
        (ticks, journal.bytes())
    };
    assert_eq!(
        run(11),
        run(11),
        "rule: same seed, same timeouts, same frames (P4, T-3)"
    );
    let ticks: Vec<u32> = (0..8).map(|seed| run(seed).0).collect();
    assert!(
        ticks.iter().any(|&t| t != ticks[0]),
        "rule: the timeout really comes from the entropy port; ticks={ticks:?}"
    );
}

#[test]
fn a_follower_refuses_proposals_and_bad_messages() {
    let config = config("three-voters");
    let mut node = node(&config, 1, &Journal::default(), 3);
    assert_eq!(
        node.propose(b"x".to_vec()),
        Err(ProposeError::NotLeader),
        "rule: only the leader takes proposals (§7.8)"
    );
    assert_eq!(
        node.step(vec![0xff, 0xff, 0xff]),
        Err(StepError::Rejected),
        "rule: bytes that are not a message are rejected and counted (§8.2)"
    );
    let mut stranger = eraftpb::Message::default();
    stranger.set_msg_type(MessageType::MsgHeartbeatResponse);
    stranger.from = 99;
    stranger.to = 1;
    let bytes = crate::convert::outbound(&stranger).unwrap().bytes;
    assert_eq!(
        node.step(bytes),
        Err(StepError::Rejected),
        "rule: a response from an unknown peer is rejected"
    );
}

#[test]
fn a_candidate_holds_its_vote_requests_in_the_ready() {
    let config = config("three-voters");
    let journal = Journal::default();
    let mut node = node(&config, 1, &journal, 5);
    let ready = (0..1_000)
        .find_map(|_| {
            node.tick();
            let ready = node.ready()?;
            if ready.messages().is_empty() {
                node.persist().unwrap();
                node.advance();
                return None;
            }
            Some(ready)
        })
        .unwrap();
    let mut targets: Vec<(NodeId, MessageType)> = ready
        .messages()
        .iter()
        .map(|message| {
            let decoded = decode_message(&message.bytes).unwrap();
            (message.to, decoded.get_msg_type())
        })
        .collect();
    targets.sort_by_key(|(to, _)| *to);
    let me = NodeId::new(1).unwrap();
    assert_eq!(
        targets,
        config
            .voters()
            .filter(|&peer| peer != me)
            .map(|peer| (peer, MessageType::MsgRequestPreVote))
            .collect::<Vec<_>>(),
        "rule: pre-vote is on, and the requests leave only through the Ready (§7.3, §7.4)"
    );
    assert_eq!(
        node.persist().unwrap().is_some(),
        ready.must_sync(),
        "rule: frame iff sync"
    );
    node.advance();
}

#[test]
fn a_restarted_node_keeps_its_term_and_refuses_an_applied_index_past_commit() {
    let config = config("one-voter");
    let journal = Journal::default();
    let mut first = node(&config, 1, &journal, 7);
    elect(&mut first, &journal);
    let term = first.status().term;
    let recovered = first.storage().log().clone();
    let commit = recovered.hard_state().commit;
    let settings = config.consensus(NodeId::new(1).unwrap()).unwrap();
    let restarted = RaftNode::new(
        &settings,
        config.voters(),
        recovered.clone(),
        journal.clone(),
        Seeded::new(8),
        commit,
    )
    .unwrap();
    assert_eq!(
        restarted.status().term,
        term,
        "rule: a restart resumes from the recovered hard state (§7.7 step 6)"
    );
    let past = LogIndex::new(commit.get() + 1).unwrap();
    assert_eq!(
        RaftNode::new(
            &settings,
            config.voters(),
            recovered,
            journal,
            Seeded::new(8),
            past
        )
        .err(),
        Some(ConstructError::AppliedPastCommit {
            applied: past,
            commit
        }),
        "rule: entries past the commit index were never applied (§7.7 step 5)"
    );
}

#[test]
#[should_panic(expected = "invariant RA-ONE-READY violated: node 1: a second Ready was asked for")]
fn a_second_ready_before_advance_breaks_its_invariant() {
    let config = config("one-voter");
    let mut node = node(&config, 1, &Journal::default(), 7);
    let first = (0..1_000).find_map(|_| {
        node.tick();
        node.ready()
    });
    assert!(first.is_some(), "fixture: a Ready appears");
    node.ready();
}

#[test]
#[should_panic(
    expected = "invariant RA-ADVANCE-UNPERSISTED violated: node 1: advance before the \
                           Ready's frame was persisted"
)]
fn advancing_before_the_frame_is_persisted_breaks_its_invariant() {
    let config = config("one-voter");
    let mut node = node(&config, 1, &Journal::default(), 7);
    let ready = (0..1_000)
        .find_map(|_| {
            node.tick();
            node.ready()
        })
        .unwrap();
    assert!(
        ready.must_sync(),
        "fixture: the first Ready (a new term) must sync"
    );
    node.advance();
}
