//! Tests of peer-message admission (br-xx08; PLAN.md §7.3, §7.8, §8.2; W02 audit S-1, H-3):
//! each rule refuses what it names and nothing else, and refused messages never reach the
//! library through [`RaftNode`]'s `step`.
#![cfg(test)]

// check: PEER-ADMISSION

use protobuf::Message as _;
use raft::eraftpb::{self, EntryType, MessageType};
use types::{
    Config, Consensus, LogIndex, MAX_COMMAND_BYTES, MAX_SAFE_INTEGER, NodeId, ProposeError,
    StepError, Term,
};

use crate::admit::{Inadmissible, MAX_PEER_TERM, admit, fits_log};
use crate::node::RaftNode;
use crate::test_support::{Journal, Seeded, config, empty_log};

/// Node 1, among the three-voter fixture's voters: 1, 7 and 2^53 - 1.
const ME: u64 = 1;

/// Another voter.
const PEER: u64 = 7;

fn voters() -> Vec<NodeId> {
    [ME, PEER, MAX_SAFE_INTEGER]
        .map(|id| NodeId::new(id).unwrap())
        .to_vec()
}

fn me() -> NodeId {
    NodeId::new(ME).unwrap()
}

/// A message of `kind` from node 7 to node 1 in term 3, as a peer would send it.
fn message(kind: MessageType) -> eraftpb::Message {
    let mut message = eraftpb::Message::default();
    message.set_msg_type(kind);
    message.from = PEER;
    message.to = ME;
    message.term = 3;
    message
}

/// A `MsgAppend` after index 4 (term 2) carrying entries with `terms`, in term 3.
fn append(terms: &[u64]) -> eraftpb::Message {
    let mut append = message(MessageType::MsgAppend);
    append.index = 4;
    append.log_term = 2;
    append.commit = 4;
    for (offset, &term) in (1..).zip(terms) {
        let mut entry = eraftpb::Entry::default();
        entry.set_entry_type(EntryType::EntryNormal);
        entry.index = 4 + offset;
        entry.term = term;
        entry.data = b"command".to_vec().into();
        append.entries.push(entry);
    }
    append
}

fn encode(message: &eraftpb::Message) -> Vec<u8> {
    message.write_to_bytes().unwrap()
}

/// What node 1 makes of `message`.
fn judge(message: &eraftpb::Message) -> Result<eraftpb::Message, Inadmissible> {
    admit(&encode(message), me(), &voters())
}

/// The eight message types v1 peers exchange, with whether each may carry entries.
const EXCHANGED: [(MessageType, bool); 8] = [
    (MessageType::MsgAppend, true),
    (MessageType::MsgAppendResponse, false),
    (MessageType::MsgHeartbeat, false),
    (MessageType::MsgHeartbeatResponse, false),
    (MessageType::MsgRequestVote, false),
    (MessageType::MsgRequestVoteResponse, false),
    (MessageType::MsgRequestPreVote, false),
    (MessageType::MsgRequestPreVoteResponse, false),
];

/// The eleven message types v1 peers never send.
const NEVER_EXCHANGED: [MessageType; 11] = [
    MessageType::MsgHup,
    MessageType::MsgBeat,
    MessageType::MsgPropose,
    MessageType::MsgSnapshot,
    MessageType::MsgUnreachable,
    MessageType::MsgSnapStatus,
    MessageType::MsgCheckQuorum,
    MessageType::MsgTransferLeader,
    MessageType::MsgTimeoutNow,
    MessageType::MsgReadIndex,
    MessageType::MsgReadIndexResp,
];

#[test]
fn every_message_type_v1_exchanges_is_admitted_unchanged() {
    for (kind, carries_entries) in EXCHANGED {
        let sent = if carries_entries {
            append(&[2, 3])
        } else {
            message(kind)
        };
        assert_eq!(
            judge(&sent),
            Ok(sent.clone()),
            "rule: v1 peers exchange {kind:?}, so it is admitted as sent (br-xx08)"
        );
    }
    let mut response = message(MessageType::MsgAppendResponse);
    response.reject = true;
    response.reject_hint = MAX_SAFE_INTEGER;
    response.index = MAX_SAFE_INTEGER;
    response.term = MAX_PEER_TERM;
    assert_eq!(
        judge(&response),
        Ok(response.clone()),
        "rule: indices up to 2^53 - 1 are in range (R-22), and terms up to the 2^52 ceiling \
         (br-fdal); message={response:?}"
    );
}

#[test]
fn message_types_v1_never_exchanges_are_refused() {
    assert_eq!(
        EXCHANGED.len() + NEVER_EXCHANGED.len(),
        19,
        "rule: the two lists cover every raft-rs 0.7 message type"
    );
    for kind in NEVER_EXCHANGED {
        let mut sent = message(kind);
        if kind == MessageType::MsgPropose {
            sent.entries = append(&[3]).entries;
        }
        assert_eq!(
            judge(&sent),
            Err(Inadmissible::MessageType {
                code: protobuf::ProtobufEnum::value(&kind)
            }),
            "rule: v1 peers never send {kind:?}, so it never reaches the library (br-xx08)"
        );
    }
}

#[test]
fn messages_for_another_node_are_refused() {
    for to in [0, 2, PEER, MAX_SAFE_INTEGER, 99] {
        let mut sent = message(MessageType::MsgHeartbeat);
        sent.to = to;
        assert_eq!(
            judge(&sent),
            Err(Inadmissible::Destination { to }),
            "rule: node 1 steps only messages addressed to it; to={to}"
        );
    }
}

#[test]
fn messages_not_from_another_voter_are_refused() {
    for from in [0, ME, 2, 99, MAX_SAFE_INTEGER + 1] {
        for kind in [
            MessageType::MsgRequestVote,
            MessageType::MsgRequestPreVote,
            MessageType::MsgHeartbeatResponse,
        ] {
            let mut sent = message(kind);
            sent.from = from;
            assert_eq!(
                judge(&sent),
                Err(Inadmissible::Sender { from }),
                "rule: only another voter may send {kind:?}; from={from}"
            );
        }
    }
}

#[test]
fn snapshots_and_snapshot_requests_are_refused() {
    let mut snapshot = message(MessageType::MsgAppend);
    snapshot.mut_snapshot().mut_metadata().index = 4;
    assert_eq!(
        judge(&snapshot),
        Err(Inadmissible::Snapshot),
        "rule: v1 never compacts, so no message carries a snapshot (§7.3)"
    );
    let mut empty = message(MessageType::MsgHeartbeat);
    empty.set_snapshot(eraftpb::Snapshot::default());
    assert_eq!(
        judge(&empty),
        Err(Inadmissible::Snapshot),
        "rule: even an empty snapshot field is refused"
    );
    let mut request = message(MessageType::MsgAppendResponse);
    request.request_snapshot = 5;
    assert_eq!(
        judge(&request),
        Err(Inadmissible::Snapshot),
        "rule: nothing may ask for a snapshot"
    );
}

#[test]
fn contexts_and_priorities_are_refused() {
    let mut transfer = message(MessageType::MsgRequestVote);
    transfer.context = b"CampaignTransfer".to_vec().into();
    let mut read = message(MessageType::MsgHeartbeat);
    read.context = b"read-index".to_vec().into();
    let mut priority = message(MessageType::MsgRequestPreVote);
    priority.priority = 1;
    let mut negative = message(MessageType::MsgRequestVote);
    negative.priority = -1;
    let mut deprecated = message(MessageType::MsgRequestVote);
    deprecated.deprecated_priority = 1;
    for (case, sent) in [
        ("a leader-transfer campaign", transfer),
        ("a read-index context", read),
        ("a priority", priority),
        ("a negative priority", negative),
        ("a deprecated priority", deprecated),
    ] {
        assert_eq!(
            judge(&sent),
            Err(Inadmissible::UnusedField),
            "rule: v1 never sets {case}"
        );
    }
}

#[test]
fn terms_and_indices_above_2_53_are_refused() {
    type Field = fn(&mut eraftpb::Message) -> &mut u64;
    let fields: [(&str, Field); 6] = [
        ("term", |m| &mut m.term),
        ("log_term", |m| &mut m.log_term),
        ("index", |m| &mut m.index),
        ("commit", |m| &mut m.commit),
        ("commit_term", |m| &mut m.commit_term),
        ("reject_hint", |m| &mut m.reject_hint),
    ];
    for (name, field) in fields {
        for value in [MAX_SAFE_INTEGER + 1, u64::MAX] {
            let mut sent = message(MessageType::MsgAppendResponse);
            *field(&mut sent) = value;
            assert_eq!(
                judge(&sent),
                Err(Inadmissible::OutOfRange),
                "rule: {name} above 2^53 - 1 is refused (R-22); {name}={value}"
            );
        }
    }
    let mut entry_index = append(&[3]);
    entry_index.index = MAX_SAFE_INTEGER;
    entry_index.entries[0].index = MAX_SAFE_INTEGER + 1;
    let mut entry_term = append(&[3]);
    entry_term.entries[0].term = MAX_SAFE_INTEGER + 1;
    for (case, sent) in [
        ("an entry index", entry_index),
        ("an entry term", entry_term),
    ] {
        assert_eq!(
            judge(&sent),
            Err(Inadmissible::OutOfRange),
            "rule: {case} above 2^53 - 1 is refused"
        );
    }
}

#[test]
fn term_zero_is_refused_on_every_admitted_type() {
    for (kind, carries_entries) in EXCHANGED {
        let mut sent = if carries_entries {
            append(&[])
        } else {
            message(kind)
        };
        sent.term = 0;
        assert_eq!(
            judge(&sent),
            Err(Inadmissible::ZeroTerm),
            "rule: a v1 peer never sends term 0, which raft-rs would take for a local message \
             (br-e1sd); kind={kind:?}"
        );
    }
}

#[test]
fn a_forged_term_zero_vote_never_reaches_the_library() {
    // Without admission, raft-rs grants this vote with term 0 and then fatal!s on sending
    // the grant ("term should be set when sending MsgRequestVoteResponse"), so one forged
    // message from a voter would crash the node (br-e1sd).
    let raw: [&[u8]; 2] = [
        // MsgRequestVote (5), to 1, from 7: no term field, so term 0.
        &[0x08, 0x05, 0x10, 0x01, 0x18, 0x07],
        // MsgRequestPreVote (17), to 1, from 7.
        &[0x08, 0x11, 0x10, 0x01, 0x18, 0x07],
    ];
    for bytes in raw {
        let journal = Journal::default();
        let mut node = follower(&journal);
        let before = node.status();
        assert_eq!(
            node.step(bytes.to_vec()),
            Err(StepError::Rejected),
            "rule: a term-0 vote request is refused before the library sees it; bytes={bytes:02x?}"
        );
        assert_eq!(
            (node.ready(), node.status()),
            (None, before),
            "rule: and leaves no Ready and no change; bytes={bytes:02x?}"
        );
    }
    for kind in [MessageType::MsgAppend, MessageType::MsgHeartbeat] {
        let journal = Journal::default();
        let mut node = follower(&journal);
        let mut sent = message(kind);
        sent.term = 0;
        sent.commit = 1;
        assert_eq!(
            node.step(encode(&sent)),
            Err(StepError::Rejected),
            "rule: a term-0 {kind:?} is refused; it would skip every term rule and could move \
             commit or regress the term"
        );
    }
}

#[test]
fn entries_v1_never_replicates_are_refused() {
    let mut cases: Vec<(&str, eraftpb::Message, usize)> = Vec::new();
    for kind in [EntryType::EntryConfChange, EntryType::EntryConfChangeV2] {
        let mut change = append(&[2, 3]);
        change.entries[1].set_entry_type(kind);
        cases.push(("a configuration change", change, 1));
    }
    let mut context = append(&[3]);
    context.entries[0].context = b"x".to_vec().into();
    cases.push(("an entry context", context, 0));
    let mut sync_log = append(&[3]);
    sync_log.entries[0].sync_log = true;
    cases.push(("sync_log", sync_log, 0));
    let mut oversized = append(&[2, 3]);
    oversized.entries[1].data = vec![0; usize::try_from(MAX_COMMAND_BYTES).unwrap() + 1].into();
    cases.push(("more than one command's bytes", oversized, 1));
    let mut heartbeat = message(MessageType::MsgHeartbeat);
    heartbeat.entries = append(&[3]).entries;
    cases.push(("entries on a heartbeat", heartbeat, 0));
    let mut response = message(MessageType::MsgAppendResponse);
    response.entries = append(&[3]).entries;
    cases.push(("entries on an append response", response, 0));
    for (case, sent, position) in cases {
        assert_eq!(
            judge(&sent),
            Err(Inadmissible::Entry { position }),
            "rule: v1 never replicates {case} (br-xx08)"
        );
    }
    let mut largest = append(&[3]);
    largest.entries[0].data = vec![0; usize::try_from(MAX_COMMAND_BYTES).unwrap()].into();
    assert_eq!(
        judge(&largest),
        Ok(largest.clone()),
        "rule: an entry of exactly one command's bytes is admitted"
    );
}

#[test]
fn entries_that_do_not_follow_their_predecessor_are_refused() {
    let mut gap = append(&[2, 3]);
    gap.entries[1].index += 1;
    let mut first_index = append(&[3]);
    first_index.entries[0].index = 4;
    let mut zero = append(&[3]);
    zero.index = 0;
    zero.log_term = 0;
    zero.entries[0].index = 1;
    zero.entries[0].term = 0;
    let mut decreasing = append(&[3, 2]);
    decreasing.term = 3;
    let below_log_term = append(&[1]);
    let past_message = append(&[3, 4]);
    for (case, sent, position) in [
        ("a gap in the indices", gap, 1),
        (
            "a first index that does not follow the message's",
            first_index,
            0,
        ),
        ("a term of 0", zero, 0),
        ("a decreasing term", decreasing, 1),
        ("a term below the message's log_term", below_log_term, 0),
        ("a term past the message's", past_message, 1),
    ] {
        assert_eq!(
            judge(&sent),
            Err(Inadmissible::EntryOrder { position }),
            "rule: entries follow their predecessor (br-xx08); case={case}"
        );
    }
}

#[test]
fn unknown_fields_are_refused_at_every_depth() {
    let mut top = encode(&message(MessageType::MsgHeartbeat));
    // Field 1000, varint 1: a field this build does not know.
    top.extend_from_slice(&[0xc0, 0x3e, 0x01]);
    let mut in_entry = append(&[3]);
    in_entry.entries[0].mut_unknown_fields().add_varint(1000, 1);
    // Field 1 (msg_type), varint 99: a message type this build does not know, which the
    // runtime refuses to decode (`read_enum`).
    let unknown_type = [0x08, 0x63, 0x10, 0x01, 0x18, 0x02];
    for (case, bytes) in [
        ("on the message", top),
        ("on an entry", encode(&in_entry)),
        ("as an unknown message type", unknown_type.to_vec()),
    ] {
        assert_eq!(
            admit(&bytes, me(), &voters()),
            Err(Inadmissible::Undecodable),
            "rule: unknown fields are refused, never stored (W02 audit H-3); case={case}"
        );
    }
    let mut in_snapshot = message(MessageType::MsgAppend);
    in_snapshot
        .mut_snapshot()
        .mut_metadata()
        .mut_conf_state()
        .mut_unknown_fields()
        .add_varint(1000, 1);
    assert_eq!(
        judge(&in_snapshot),
        Err(Inadmissible::Undecodable),
        "rule: an unknown field inside a snapshot is refused before decoding, like at every \
         other depth (H-3, br-808c)"
    );
    assert_eq!(
        admit(&[0xff, 0xff, 0xff], me(), &voters()),
        Err(Inadmissible::Undecodable),
        "rule: bytes that are not a message are refused"
    );
}

/// Node 1 of the three-voter fixture, fresh.
fn follower(journal: &Journal) -> RaftNode<Journal> {
    let config: Config = config("three-voters");
    RaftNode::new(
        &config.consensus(me()).unwrap(),
        config.voters(),
        empty_log(),
        journal.clone(),
        Seeded::new(11),
        LogIndex::ZERO,
    )
    .unwrap()
}

/// Runs every Ready `node` has, persisting each frame into `journal`, and reports what it
/// committed as applied.
fn settle_node(node: &mut RaftNode<Journal>, journal: &Journal) {
    while let Some(ready) = node.ready() {
        if let Some(frame) = node.persist().unwrap() {
            journal.push(&frame);
            node.synced();
        }
        let light = node.advance();
        if let Some(last) = ready.committed().iter().chain(&light.committed).last() {
            node.applied(last.index.index());
        }
    }
}

/// Node 1 following node 7 in term 2, with entries 1 to 3 of term 2 committed.
fn follower_with_committed_log(journal: &Journal) -> RaftNode<Journal> {
    let mut node = follower(journal);
    let mut sent = message(MessageType::MsgAppend);
    sent.term = 2;
    sent.commit = 3;
    for index in 1..=3 {
        sent.entries.push(eraftpb::Entry {
            index,
            term: 2,
            data: b"committed".to_vec().into(),
            ..eraftpb::Entry::default()
        });
    }
    node.step(encode(&sent)).unwrap();
    settle_node(&mut node, journal);
    assert_eq!(
        (node.status().commit.get(), node.status().last.get()),
        (3, 3),
        "fixture: entries 1 to 3 are committed"
    );
    node
}

#[test]
fn a_heartbeat_commit_past_the_log_is_refused() {
    let mut heartbeat = message(MessageType::MsgHeartbeat);
    heartbeat.commit = 5;
    assert_eq!(
        fits_log(&heartbeat, 4),
        Err(Inadmissible::CommitPastLog { commit: 5, last: 4 }),
        "rule: raft-rs's commit_to would fatal! past the last index (br-1jtp)"
    );
    heartbeat.commit = 4;
    assert_eq!(
        fits_log(&heartbeat, 4),
        Ok(()),
        "rule: up to the last index is fine"
    );
    let mut append = append(&[2, 3]);
    append.commit = 99;
    assert_eq!(
        fits_log(&append, 4),
        Ok(()),
        "rule: an append's commit is not judged here: once its previous entry matches, which \
         the previous-entry rule makes this node's own entry (br-0mio), maybe_append bounds \
         the commit by entries this node holds"
    );
    // Without this refusal, raft-rs fatal!s: "to_commit 1 is out of range [last_index 0]".
    let journal = Journal::default();
    let mut node = follower(&journal);
    let before = node.status();
    let mut forged = message(MessageType::MsgHeartbeat);
    forged.commit = 1;
    assert_eq!(
        node.step(encode(&forged)),
        Err(StepError::Rejected),
        "rule: a heartbeat committing past this node's log never reaches the library"
    );
    assert_eq!(
        (node.ready(), node.status()),
        (None, before),
        "rule: and changes nothing"
    );
}

#[test]
fn an_appends_previous_entry_has_index_and_term_both_zero_or_both_nonzero() {
    let mut past_the_log = append(&[]);
    past_the_log.log_term = 0;
    let mut zero_index = append(&[]);
    zero_index.index = 0;
    let mut first = append(&[]);
    first.index = 0;
    first.log_term = 0;
    first.entries = append(&[3]).entries;
    first.entries[0].index = 1;
    for (case, sent, expected) in [
        (
            "a nonzero index with term 0",
            past_the_log,
            Err(Inadmissible::PrevEntry {
                index: 4,
                log_term: 0,
            }),
        ),
        (
            "index 0 with a nonzero term",
            zero_index,
            Err(Inadmissible::PrevEntry {
                index: 0,
                log_term: 2,
            }),
        ),
    ] {
        assert_eq!(
            judge(&sent),
            expected,
            "rule: a v1 leader sends log_term = term(index), 0 only at index 0 (br-0mio); \
             case={case}"
        );
    }
    assert_eq!(
        judge(&first),
        Ok(first.clone()),
        "rule: the first append, after index 0 with term 0, is admitted"
    );
}

#[test]
fn a_forged_append_past_the_log_never_reaches_the_library() {
    // Without the previous-entry rule each of these panics a fresh follower: raft-rs's
    // term() answers 0 past the log, so log_term 0 "matches" and maybe_append runs past its
    // bounds (br-0mio).
    let mut with_entry = message(MessageType::MsgAppend);
    with_entry.term = 1;
    with_entry.index = 1;
    with_entry.log_term = 0;
    with_entry.entries.push(eraftpb::Entry {
        index: 2,
        term: 1,
        data: b"forged".to_vec().into(),
        ..eraftpb::Entry::default()
    });
    let mut at_the_ceiling = message(MessageType::MsgAppend);
    at_the_ceiling.term = MAX_SAFE_INTEGER;
    at_the_ceiling.index = MAX_SAFE_INTEGER;
    at_the_ceiling.commit = MAX_SAFE_INTEGER;
    at_the_ceiling.log_term = 0;
    let payloads: [(&str, Vec<u8>); 3] = [
        (
            "\"to_commit 1 is out of range [last_index 0]\"",
            // MsgAppend: to 1, from 7, term 1, index 1, log_term 0, commit 1.
            vec![
                0x08, 0x03, 0x10, 0x01, 0x18, 0x07, 0x20, 0x01, 0x30, 0x01, 0x40, 0x01,
            ],
        ),
        (
            "\"unstable.slice[1, 2] out of bound[1, 1]\"",
            encode(&with_entry),
        ),
        (
            "every node's crash at term 2^53 - 1",
            encode(&at_the_ceiling),
        ),
    ];
    for (case, bytes) in payloads {
        let journal = Journal::default();
        let mut node = follower(&journal);
        let before = node.status();
        assert_eq!(
            node.step(bytes),
            Err(StepError::Rejected),
            "rule: a forged append past the log is refused before the library sees it; \
             case={case}"
        );
        assert_eq!(
            (node.ready(), node.status()),
            (None, before),
            "rule: and changes nothing; case={case}"
        );
    }
}

#[test]
fn peer_terms_above_the_ceiling_are_refused() {
    for (term, expected) in [
        (MAX_PEER_TERM, None),
        (
            MAX_PEER_TERM + 1,
            Some(Inadmissible::TermCeiling {
                term: MAX_PEER_TERM + 1,
            }),
        ),
        (
            MAX_SAFE_INTEGER,
            Some(Inadmissible::TermCeiling {
                term: MAX_SAFE_INTEGER,
            }),
        ),
    ] {
        let mut sent = message(MessageType::MsgHeartbeat);
        sent.term = term;
        assert_eq!(
            judge(&sent).err(),
            expected,
            "rule: peer terms are admitted up to 2^52, so a node can always add elections \
             (br-fdal); term={term}"
        );
    }
}

#[test]
fn a_forged_term_at_the_top_can_neither_stall_nor_crash_a_node() {
    // One forged heartbeat at 2^53 - 1 would make a node follow at a term whose next
    // election no peer admits; a second message, a granted pre-vote, would push it to 2^53,
    // where RA-HARD-STATE fires (br-fdal). Neither reaches the library now.
    let mut heartbeat = message(MessageType::MsgHeartbeat);
    heartbeat.term = MAX_SAFE_INTEGER;
    let mut granted = message(MessageType::MsgRequestPreVoteResponse);
    granted.term = MAX_SAFE_INTEGER;
    granted.reject = false;
    let journal = Journal::default();
    let mut node = follower(&journal);
    let before = node.status();
    for (case, sent) in [
        ("the stalling heartbeat", heartbeat),
        ("the crashing pre-vote grant", granted),
    ] {
        assert_eq!(
            node.step(encode(&sent)),
            Err(StepError::Rejected),
            "rule: {case} at term 2^53 - 1 is refused (br-fdal)"
        );
        assert_eq!(
            (node.ready(), node.status()),
            (None, before.clone()),
            "rule: {case} changes nothing"
        );
    }
}

#[test]
fn an_append_conflicting_with_committed_entries_is_answered_not_fatal() {
    // raft-rs fatal!s if an appended entry conflicts with a committed one
    // (maybe_append). Admission makes that unreachable: below the commit index,
    // handle_append_entries answers at once; at or above it, contiguity puts every entry
    // past the commit index (br-1jtp).
    let journal = Journal::default();
    let mut node = follower_with_committed_log(&journal);
    let mut forged = message(MessageType::MsgAppend);
    forged.term = 3;
    forged.index = 1;
    forged.log_term = 2;
    forged.entries.push(eraftpb::Entry {
        index: 2,
        term: 3,
        data: b"conflicting".to_vec().into(),
        ..eraftpb::Entry::default()
    });
    assert_eq!(
        node.step(encode(&forged)),
        Ok(()),
        "rule: the admitted append reaches the library, which answers it without panicking"
    );
    settle_node(&mut node, &journal);
    let term_at = |index| {
        node.log()
            .term(types::EntryIndex::new(index).unwrap())
            .map(Term::get)
    };
    assert_eq!(
        (term_at(2), node.status().commit.get()),
        (Some(2), 3),
        "rule: the committed entry is kept, and the commit index with it"
    );
}

#[test]
fn a_vote_request_from_a_non_voter_is_refused_and_leaves_no_vote() {
    let journal = Journal::default();
    let mut node = follower(&journal);
    let before = node.status();
    for kind in [MessageType::MsgRequestVote, MessageType::MsgRequestPreVote] {
        let mut request = message(kind);
        request.from = 99;
        request.term = 5;
        request.index = 100;
        request.log_term = 4;
        assert_eq!(
            node.step(encode(&request)),
            Err(StepError::Rejected),
            "rule: a vote request from a non-voter is refused (br-xx08); kind={kind:?}"
        );
        assert_eq!(
            node.ready(),
            None,
            "rule: a refused vote request leaves no Ready, so no vote or term is persisted; \
             kind={kind:?}"
        );
        assert_eq!(
            node.status(),
            before,
            "rule: the library never saw it; kind={kind:?}"
        );
    }
    assert!(journal.is_empty(), "rule: nothing was written");
}

#[test]
fn proposals_snapshots_and_configuration_changes_from_the_wire_never_reach_the_library() {
    let journal = Journal::default();
    let mut node = follower(&journal);
    let before = node.status();
    let mut propose = message(MessageType::MsgPropose);
    propose.term = 0;
    propose.entries = append(&[3]).entries;
    let mut empty_propose = message(MessageType::MsgPropose);
    empty_propose.term = 0;
    let mut snapshot = message(MessageType::MsgSnapshot);
    snapshot.mut_snapshot().mut_metadata().index = 9;
    snapshot.mut_snapshot().mut_metadata().term = 3;
    let mut change = append(&[3]);
    change.entries[0].set_entry_type(EntryType::EntryConfChange);
    let mut ahead = append(&[3, 4]);
    ahead.term = 3;
    let mut huge_term = message(MessageType::MsgHeartbeat);
    huge_term.term = MAX_SAFE_INTEGER + 1;
    for (case, sent, refusal) in [
        ("a proposal", propose, Inadmissible::MessageType { code: 2 }),
        (
            "an empty proposal, which the library would fatal! on",
            empty_propose,
            Inadmissible::MessageType { code: 2 },
        ),
        (
            "a snapshot",
            snapshot,
            Inadmissible::MessageType { code: 7 },
        ),
        (
            "a configuration change",
            change,
            Inadmissible::Entry { position: 0 },
        ),
        (
            "an entry with a term past the message's",
            ahead,
            Inadmissible::EntryOrder { position: 1 },
        ),
        ("a term above 2^53 - 1", huge_term, Inadmissible::OutOfRange),
    ] {
        assert_eq!(
            judge(&sent),
            Err(refusal),
            "rule: {case} is refused for its own rule, from another voter (br-xx08)"
        );
        assert_eq!(
            node.step(encode(&sent)),
            Err(StepError::Rejected),
            "rule: {case} from the wire is refused (br-xx08)"
        );
        assert_eq!(node.ready(), None, "rule: {case} leaves no Ready");
        assert_eq!(node.status(), before, "rule: the library never saw {case}");
    }
}

#[test]
fn messages_v1_exchanges_still_reach_the_library() {
    let journal = Journal::default();
    let mut node = follower(&journal);
    let heartbeat = message(MessageType::MsgHeartbeat);
    assert_eq!(
        node.step(encode(&heartbeat)),
        Ok(()),
        "rule: a heartbeat from another voter is stepped"
    );
    let status = node.status();
    assert_eq!(
        (status.term, status.leader),
        (Term::new(3).unwrap(), NodeId::new(PEER).ok()),
        "rule: the library saw the heartbeat and follows its sender"
    );
}

#[test]
fn a_follower_that_knows_its_leader_never_forwards_a_proposal() {
    // raft-rs 0.7.0 has no switch to stop a follower forwarding `MsgPropose` to its leader,
    // so the adapter keeps proposals away from a follower's library: `propose` answers
    // `NotLeader` first (§7.8), and `step` refuses a peer's `MsgPropose` (br-xx08).
    let journal = Journal::default();
    let mut node = follower(&journal);
    node.step(encode(&message(MessageType::MsgHeartbeat)))
        .unwrap();
    let ready = node.ready().unwrap();
    if node.persist().unwrap().is_some() {
        node.synced();
    }
    let light = node.advance();
    assert!(
        ready.messages().len() + light.messages.len() == 1,
        "rule: the heartbeat is answered once; ready={ready:?} light={light:?}"
    );
    assert_eq!(
        node.status().leader,
        NodeId::new(PEER).ok(),
        "rule: the follower knows its leader, so the library would forward a proposal"
    );
    assert_eq!(
        node.propose(b"put".to_vec()),
        Err(ProposeError::NotLeader),
        "rule: a follower refuses a proposal before the library sees it (§7.8)"
    );
    assert_eq!(
        node.ready(),
        None,
        "rule: nothing is forwarded to the leader"
    );
    let mut forwarded = message(MessageType::MsgPropose);
    forwarded.term = 0;
    forwarded.entries = append(&[3]).entries;
    assert_eq!(
        node.step(encode(&forwarded)),
        Err(StepError::Rejected),
        "rule: a proposal another follower forwarded is refused (br-xx08)"
    );
    assert_eq!(node.ready(), None, "rule: and is not forwarded on either");
}
