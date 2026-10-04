//! Tests of [`RaftNode`] through the Consensus port (PLAN.md §7.3, §7.4, §7.7, D-78): W13.1's,
//! and br-kky5's recovery, happy-path step and invariant fixtures.
#![cfg(test)]

// check: RAFT-ADAPTER

use journal::{JournalError, LogBounds, LogicalLog};
use raft::eraftpb::{self, MessageType};
use types::{
    ByteCount, Config, Consensus, Entry, EntryPayload, LightReady, LogIndex, NodeId, ProposeError,
    Ready, Role, StepError,
};

use crate::convert::decode_message;
use crate::node::{ConstructError, RaftNode};
use crate::ready_loop::{RaftInput, ReadyLoop};
use crate::ready_loop_tests::{
    Cluster, FIRST_SYNC, INCARNATION, Member, assert_synced_obligations, durable_state,
    member_over, start_campaign,
};
use crate::test_support::{
    BOUNDS, CONFIGS, Journal, Seeded, config, empty_log, journal_with_genesis,
};

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

/// One turn of a node's loop: take a Ready, persist its frame into `journal` and report it
/// synced, advance, and report everything committed as applied. `None` when there is no
/// Ready.
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
        node.synced();
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
                if node.persist().unwrap().is_some() {
                    node.synced();
                }
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
    let frame = node.persist().unwrap();
    assert_eq!(frame.is_some(), ready.must_sync(), "rule: frame iff sync");
    if frame.is_some() {
        node.synced();
    }
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

#[test]
#[should_panic(
    expected = "invariant RA-ADVANCE-UNSYNCED violated: node 1: advance before the Ready's \
                frame was synced"
)]
fn advancing_before_the_frame_is_synced_breaks_its_invariant() {
    let config = config("one-voter");
    let mut node = node(&config, 1, &Journal::default(), 7);
    let ready = (0..1_000)
        .find_map(|_| {
            node.tick();
            node.ready()
        })
        .unwrap();
    assert!(
        node.persist().unwrap().is_some() && ready.must_sync(),
        "fixture: the first Ready (a new term) writes a frame"
    );
    assert!(
        node.storage().is_sealed(),
        "rule: a written frame seals the storage view until its sync (§7.4 step 4)"
    );
    node.advance();
}

#[test]
#[should_panic(
    expected = "invariant RA-SYNCED-WITHOUT-FRAME violated: node 1: a sync was reported with \
                no written frame waiting for it"
)]
fn a_sync_with_no_frame_waiting_breaks_its_invariant() {
    let config = config("one-voter");
    let mut node = node(&config, 1, &Journal::default(), 7);
    node.synced();
}

/// A lone voter holding its first Ready, not yet advanced.
fn holding_a_ready() -> RaftNode<Journal> {
    let mut node = node(&config("one-voter"), 1, &Journal::default(), 7);
    let held = (0..1_000).find_map(|_| {
        node.tick();
        node.ready()
    });
    assert!(held.is_some(), "fixture: a Ready appears");
    node
}

#[test]
#[should_panic(
    expected = "invariant RA-INPUT-DURING-READY violated: node 1: a tick fed while a Ready is \
                outstanding"
)]
fn a_tick_while_a_ready_is_outstanding_breaks_its_invariant() {
    holding_a_ready().tick();
}

#[test]
#[should_panic(
    expected = "invariant RA-INPUT-DURING-READY violated: node 1: a peer message fed while a \
                Ready is outstanding"
)]
fn a_peer_message_while_a_ready_is_outstanding_breaks_its_invariant() {
    let _ = holding_a_ready().step(vec![0xff]);
}

#[test]
#[should_panic(
    expected = "invariant RA-INPUT-DURING-READY violated: node 1: a proposal fed while a Ready \
                is outstanding"
)]
fn a_proposal_while_a_ready_is_outstanding_breaks_its_invariant() {
    let _ = holding_a_ready().propose(b"x".to_vec());
}

#[test]
fn a_restart_through_recovery_resumes_the_persisted_term_vote_commit_and_log() {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    let settings = config.consensus(id).unwrap();
    let (journal, log) = journal_with_genesis(&config, id);
    let mut first = RaftNode::new(
        &settings,
        config.voters(),
        log,
        journal.clone(),
        Seeded::new(7),
        LogIndex::ZERO,
    )
    .unwrap();
    elect(&mut first, &journal);
    for command in [b"one".as_slice(), b"two"] {
        first.propose(command.to_vec()).unwrap();
        while turn(&mut first, &journal).is_some() {}
    }
    let live = first.log().hard_state();
    let live_last = first.log().last_index();
    let bytes = journal.bytes();
    let recovered = journal::recover(
        &journal,
        ByteCount::new(u64::try_from(bytes.len()).unwrap()),
        &config,
        id,
        BOUNDS,
    )
    .unwrap();
    assert_eq!(
        recovered.truncate_to(),
        None,
        "fixture: every frame synced, so there is no torn tail"
    );
    let (_, log) = recovered.into_parts();
    assert_eq!(
        (log.hard_state(), log.last_index()),
        (live, live_last),
        "rule: recovery rebuilds the persisted hard state and log from the journal bytes \
         (§7.7 step 4)"
    );
    let restarted = RaftNode::new(
        &settings,
        config.voters(),
        log,
        journal.clone(),
        Seeded::new(8),
        live.commit,
    )
    .unwrap();
    let status = restarted.status();
    assert_eq!(
        (
            status.term,
            status.commit,
            status.last,
            restarted.log().hard_state().vote
        ),
        (live.term, live.commit, live_last, Some(id)),
        "rule: a node constructed from the recovered log resumes its persisted term, vote, \
         commit index and last index (§7.7 step 6)"
    );
    assert!(
        live.term.get() >= 1 && live_last.get() >= 3,
        "fixture: an election and two commands were persisted; term {} last {live_last}",
        live.term.get()
    );
}

#[test]
fn a_valid_peer_message_steps_to_its_response() {
    let config = config("three-voters");
    let mut voters = config.voters();
    let (asker, granter) = (voters.next().unwrap(), voters.next().unwrap());
    let journal = Journal::default();
    let mut candidate = node(&config, asker.get(), &journal, 3);
    let request = (0..1_000)
        .find_map(|_| {
            candidate.tick();
            let ready = candidate.ready()?;
            let request = ready.messages().iter().find(|m| m.to == granter).cloned();
            if candidate.persist().unwrap().is_some() {
                candidate.synced();
            }
            candidate.advance();
            request
        })
        .unwrap();
    assert_eq!(
        decode_message(&request.bytes).unwrap().get_msg_type(),
        MessageType::MsgRequestPreVote,
        "fixture: the candidate asks for a pre-vote first"
    );
    let mut voter = node(&config, granter.get(), &Journal::default(), 4);
    assert_eq!(
        voter.step(request.bytes),
        Ok(()),
        "rule: a valid peer message is stepped into the library"
    );
    let ready = voter.ready().unwrap();
    let replies: Vec<(NodeId, MessageType, bool)> = ready
        .messages()
        .iter()
        .map(|m| {
            let decoded = decode_message(&m.bytes).unwrap();
            (m.to, decoded.get_msg_type(), decoded.get_reject())
        })
        .collect();
    assert_eq!(
        replies,
        vec![(asker, MessageType::MsgRequestPreVoteResponse, false)],
        "rule: the stepped request produces its granted response in the next Ready"
    );
}

#[test]
#[should_panic(
    expected = "invariant RA-PERSIST-WITHOUT-READY violated: node 1: persist with no \
                outstanding Ready"
)]
fn persisting_with_no_ready_breaks_its_invariant() {
    let _ = node(&config("one-voter"), 1, &Journal::default(), 7).persist();
}

#[test]
#[should_panic(
    expected = "invariant RA-ADVANCE-WITHOUT-READY violated: node 1: advance with no \
                outstanding Ready"
)]
fn advancing_with_no_ready_breaks_its_invariant() {
    let _ = node(&config("one-voter"), 1, &Journal::default(), 7).advance();
}

#[test]
fn a_frame_refused_at_the_hard_stop_stays_owed() {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    let voters: Vec<NodeId> = config.voters().collect();
    let tiny = LogicalLog::after_genesis(
        0,
        &voters,
        LogBounds::new(ByteCount::new(16), ByteCount::new(1)),
    );
    let mut node = RaftNode::new(
        &config.consensus(id).unwrap(),
        config.voters(),
        tiny,
        Journal::default(),
        Seeded::new(7),
        LogIndex::ZERO,
    )
    .unwrap();
    let ready = (0..1_000)
        .find_map(|_| {
            node.tick();
            node.ready()
        })
        .unwrap();
    assert!(ready.must_sync(), "fixture: the first Ready must sync");
    for attempt in 1..=2 {
        let refused = node.persist();
        assert!(
            is_hard_stop(&refused),
            "rule: a frame past max_journal_bytes is refused (DM-7), and the refusal leaves it \
             owed rather than dropped; attempt {attempt}: {refused:?}"
        );
    }
    assert!(
        !node.storage().is_sealed() && node.log().last_index() == LogIndex::ZERO,
        "rule: a refused frame changes nothing: the log is untouched and the view unsealed"
    );
}

/// Every public read on `member`'s loop and node, taken while its frame without entries is
/// syncing: none may touch the sealed storage view (br-g4hr).
fn assert_reads_answer_during_sync(member: &Member, who: &str) {
    let lp = &member.lp;
    assert!(
        lp.outstanding_sync().is_some(),
        "fixture: the {who} waits for its sync"
    );
    let node = lp.node();
    let (written, last) = (node.log().hard_state(), node.log().last_index());
    assert!(
        node.storage().is_sealed(),
        "fixture: the {who}'s view is sealed until SyncDone"
    );
    let status = node.status();
    assert_eq!(
        &status,
        lp.status(),
        "rule: the node's status during the sync is the loop's cached one (br-g4hr); {who}"
    );
    assert_eq!(
        (status.term, status.last),
        (written.term, last),
        "rule: during a sync the status shows the written, not yet durable, term and the log's \
         last index; {who}"
    );
    assert_eq!(
        (
            node.applied_index(),
            lp.applied(),
            node.storage_failure(),
            node.voters().len()
        ),
        (LogIndex::ZERO, LogIndex::ZERO, None, 3),
        "rule: the other reads answer during the sync too; {who}"
    );
}

#[test]
fn every_public_read_answers_while_a_frame_without_entries_syncs() {
    let mut cluster = Cluster::new(&config("three-voters"), 1);
    let candidate = start_campaign(&mut cluster);
    assert_eq!(
        cluster.get(candidate).lp.node().log().last_index(),
        LogIndex::ZERO,
        "fixture: the candidate's frame holds only its new term and self-vote, no entries"
    );
    assert_reads_answer_during_sync(cluster.get(candidate), "candidate");
    assert_eq!(
        cluster.get(candidate).lp.status().role,
        Role::Candidate,
        "rule: the status shows the candidacy whose term and vote are still syncing"
    );
    cluster.complete_sync(candidate);
    let at = cluster
        .network
        .iter()
        .position(|(_, _, bytes)| {
            decode_message(bytes).unwrap().get_msg_type() == MessageType::MsgRequestVote
        })
        .unwrap();
    let voter = cluster.network[at].1;
    cluster.deliver(at);
    assert_eq!(
        cluster.get(voter).lp.node().log().hard_state().vote,
        Some(candidate),
        "fixture: the voter's frame holds only its vote for the candidate, no entries"
    );
    assert_reads_answer_during_sync(cluster.get(voter), "voter");
    cluster.settle();
    assert_synced_obligations(&cluster);
}

/// A node of `config` restarted over a fresh copy of `bytes` and `log`, its journal and
/// logical log, built at `replayed`: its state machine replayed the entries through it first
/// (§7.7 steps 5 and 6).
fn restarted_at(
    config: &Config,
    id: NodeId,
    bytes: &[u8],
    log: &LogicalLog,
    replayed: LogIndex,
) -> Member {
    let journal = Journal::default();
    journal.push(bytes);
    let mut member = member_over(config, id, 8, log.clone(), journal.clone());
    let node = RaftNode::new(
        &config.consensus(id).unwrap(),
        config.voters(),
        log.clone(),
        journal,
        Seeded::new(8),
        replayed,
    )
    .unwrap();
    member.lp = ReadyLoop::new(node, INCARNATION, FIRST_SYNC);
    member
}

/// The indices of the entries handed to `member`'s applier, in order.
fn applied_indices(member: &Member) -> Vec<u64> {
    member
        .recorder
        .applied
        .iter()
        .map(|(entry, _)| entry.index.get())
        .collect()
}

/// Settles `cluster` like [`Cluster::settle`], but drops every append that carries no
/// entries. A follower then learns each commit index together with the next entries, in a
/// frame that persists it; learnt alone, the commit index is never worth a frame (§7.4 step 3).
fn settle_persisting_commits(cluster: &mut Cluster) {
    for _ in 0..100_000 {
        let deliverable = (0..cluster.network.len())
            .find(|&at| !cluster.syncs.contains_key(&cluster.network[at].1));
        if let Some(at) = deliverable {
            let message = decode_message(&cluster.network[at].2).unwrap();
            if message.get_msg_type() == MessageType::MsgAppend && message.entries.is_empty() {
                cluster.network.remove(at);
            } else {
                cluster.deliver(at);
            }
        } else if let Some(&node) = cluster.syncs.keys().next() {
            cluster.complete_sync(node);
        } else {
            return;
        }
    }
    panic!("the cluster did not settle");
}

/// The cluster's leader, if it has one.
fn leader_of(cluster: &Cluster) -> Option<NodeId> {
    cluster
        .ids()
        .into_iter()
        .find(|&id| cluster.get(id).lp.status().role == Role::Leader)
}

#[test]
fn a_cluster_restarted_at_its_replayed_indices_hands_over_only_the_entries_after_them() {
    let config = config("three-voters");
    let mut first = Cluster::new(&config, 4);
    start_campaign(&mut first);
    first.settle();
    let leader = leader_of(&first).unwrap();
    for n in 0..5 {
        first.feed(leader, RaftInput::Propose(format!("c{n}").into_bytes()));
        settle_persisting_commits(&mut first);
    }
    assert!(
        first.syncs.is_empty(),
        "fixture: every frame synced, so each logical log is what recovery reads back (§7.7 \
         step 4; a_restart_through_recovery_resumes_the_persisted_term_vote_commit_and_log)"
    );
    // Each node restarts at its own replayed index: the first that persisted a commit index
    // replays all of it (as §7.7 step 5 does), the next only half of it, and a node that
    // persisted none replays nothing.
    let mut second = Cluster::new(&config, 5);
    let mut starts = Vec::new();
    for id in first.ids() {
        let bytes = first.get(id).journal.bytes();
        let log = first.get(id).lp.node().log().clone();
        let persisted = log.hard_state().commit.get();
        let partial = starts.iter().any(|&(_, _, persisted)| persisted > 0);
        let replayed = LogIndex::new(if partial { persisted / 2 } else { persisted }).unwrap();
        second
            .members
            .insert(id, restarted_at(&config, id, &bytes, &log, replayed));
        second.durable.insert(id, durable_state(&bytes));
        starts.push((id, replayed, persisted));
    }
    assert!(
        starts
            .iter()
            .any(|&(_, replayed, persisted)| persisted >= 3 && replayed.get() == persisted)
            && starts.iter().any(|&(_, replayed, persisted)| {
                replayed.get() >= 1 && replayed.get() < persisted
            }),
        "fixture: one node restarts having replayed all of several persisted committed entries, \
         and one part of them; {starts:?}"
    );
    for &(id, replayed, persisted) in &starts {
        let member = second.members.get_mut(&id).unwrap();
        assert_eq!(
            (member.lp.applied(), member.lp.node().applied_index()),
            (replayed, replayed),
            "rule: the loop and the library start from the replayed index (§7.7 step 6); \
             node={id}"
        );
        member.poll().unwrap();
        assert_eq!(
            applied_indices(member),
            (replayed.get() + 1..=persisted).collect::<Vec<_>>(),
            "rule: at start-up the loop hands over exactly the persisted committed entries past \
             the replayed index (§7.7 step 7); node={id} replayed={replayed}"
        );
    }
    start_campaign(&mut second);
    second.settle();
    let leader = leader_of(&second).unwrap();
    second.feed(leader, RaftInput::Propose(b"after the restart".to_vec()));
    second.settle();
    for &(id, replayed, persisted) in &starts {
        let member = second.get(id);
        let commit = member.lp.status().commit;
        assert!(
            commit.get() > persisted,
            "fixture: the new term commits past the restart; node={id} commit={commit}"
        );
        assert_eq!(
            applied_indices(member),
            (replayed.get() + 1..=commit.get()).collect::<Vec<_>>(),
            "rule: nothing at or below the replayed index is handed over again, and every entry \
             after it once, in index order (§7.4 step 5, C-4); node={id} replayed={replayed}"
        );
        assert_eq!(
            (member.lp.applied(), member.lp.node().applied_index()),
            (commit, commit),
            "rule: applied progress ends at the commit, and the library is told (§7.4 step 7); \
             node={id}"
        );
    }
    assert_synced_obligations(&second);
}

/// Whether `result` is the journal's hard-stop refusal.
const fn is_hard_stop(result: &Result<Option<Vec<u8>>, JournalError>) -> bool {
    match result {
        Err(JournalError::HardStop { .. }) => true,
        Ok(_)
        | Err(
            JournalError::Sequence { .. }
            | JournalError::Offset { .. }
            | JournalError::Gap { .. }
            | JournalError::TruncatesCommitted { .. }
            | JournalError::CommitBeyondLog { .. }
            | JournalError::ZeroTerm { .. }
            | JournalError::TermDecreases { .. }
            | JournalError::TermAhead { .. }
            | JournalError::TermRegression { .. }
            | JournalError::CommitRegression { .. }
            | JournalError::VoteChanged { .. }
            | JournalError::VoteForNonVoter { .. }
            | JournalError::NotInLog { .. }
            | JournalError::NotRaftBatch { .. }
            | JournalError::EntryMismatch { .. }
            | JournalError::Frame(_)
            | JournalError::Record(_)
            | JournalError::Storage(_),
        ) => false,
    }
}
