//! Tests of the peer-envelope codec (W13.3; PLAN.md §8.2, D-63, D-71, §10.19): messages
//! round-trip, every identity mismatch drops and is counted, arbitrary bytes never panic, and
//! a three-voter cluster whose every message travels sealed elects a leader and commits.
#![cfg(test)]

// check: PEER-ENVELOPE

use protobuf::Message as _;
use raft::eraftpb::{self, MessageType};
use types::{
    Config, Consensus, InstanceId, LogIndex, MAX_FRAME_PAYLOAD_BYTES, NodeId, OutboundMessage,
    PROTOCOL_VERSION, Role, SemanticFingerprint,
};

use crate::admit::Inadmissible;
use crate::envelope::{
    DropCounts, EnvelopeError, HEADER_BYTES, IdentityError, MAX_ENVELOPE_BYTES, PeerCodec,
    SealError,
};
use crate::node::RaftNode;
use crate::test_support::{Journal, Seeded, config, empty_log};

const INSTANCE: [u8; 16] = [7; 16];
const FINGERPRINT: [u8; 32] = [9; 32];

fn id(raw: u64) -> NodeId {
    NodeId::new(raw).unwrap()
}

/// Node `node`'s codec among voters 1, 2 and 3 of the test instance.
fn codec(node: u64) -> PeerCodec {
    PeerCodec::new(
        InstanceId::from_bytes(INSTANCE),
        SemanticFingerprint::from_bytes(FINGERPRINT),
        id(node),
        [1, 2, 3].map(id),
    )
    .unwrap()
}

/// A heartbeat from node 2 to node 1 in term 3, as the library encodes it.
fn heartbeat() -> OutboundMessage {
    let mut message = eraftpb::Message::default();
    message.set_msg_type(MessageType::MsgHeartbeat);
    message.from = 2;
    message.to = 1;
    message.term = 3;
    message.commit = 1;
    crate::convert::outbound(&message).unwrap()
}

/// The heartbeat, sealed by node 2 for node 1.
fn sealed() -> Vec<u8> {
    codec(2).seal(&heartbeat()).unwrap()
}

#[test]
fn a_sealed_message_opens_at_its_destination_unchanged() {
    let message = heartbeat();
    let envelope = codec(2).seal(&message).unwrap();
    assert_eq!(
        envelope.len(),
        HEADER_BYTES + message.bytes.len(),
        "rule: an envelope is its 80-byte header plus the message (§8.2)"
    );
    let payload_len = u32::try_from(message.bytes.len()).unwrap();
    let mut expected = b"KVPE".to_vec();
    expected.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    expected.extend_from_slice(&INSTANCE);
    expected.extend_from_slice(&FINGERPRINT);
    expected.extend_from_slice(&2_u64.to_le_bytes());
    expected.extend_from_slice(&1_u64.to_le_bytes());
    expected.extend_from_slice(&payload_len.to_le_bytes());
    expected.extend_from_slice(&message.bytes);
    assert_eq!(
        envelope, expected,
        "rule: the wire format is the documented layout, little-endian (§8.2)"
    );
    let mut receiver = codec(1);
    assert_eq!(
        receiver.open(id(2), &envelope),
        Ok(message.bytes),
        "rule: the destination opens exactly the message that was sealed"
    );
    assert_eq!(
        receiver.dropped(),
        DropCounts::default(),
        "rule: nothing is counted for an envelope that opens"
    );
}

/// `envelope` with `bytes` written at `offset`.
fn patched(envelope: &[u8], offset: usize, bytes: &[u8]) -> Vec<u8> {
    let mut patched = envelope.to_vec();
    patched[offset..offset + bytes.len()].copy_from_slice(bytes);
    patched
}

#[test]
fn every_identity_mismatch_drops_and_is_counted() {
    let envelope = sealed();
    let version = PROTOCOL_VERSION + 1;
    let cases: [(&str, u64, Vec<u8>, EnvelopeError); 10] = [
        (
            "another protocol version",
            2,
            patched(&envelope, 4, &version.to_le_bytes()),
            EnvelopeError::ProtocolVersion { version },
        ),
        (
            "protocol version 0",
            2,
            patched(&envelope, 4, &0_u64.to_le_bytes()),
            EnvelopeError::ProtocolVersion { version: 0 },
        ),
        (
            "another cluster instance with the same configuration",
            2,
            patched(&envelope, 12, &[8; 16]),
            EnvelopeError::Instance,
        ),
        (
            "another semantic configuration",
            2,
            patched(&envelope, 28, &[1; 32]),
            EnvelopeError::Fingerprint,
        ),
        (
            "a sender outside the voters",
            99,
            patched(&envelope, 60, &99_u64.to_le_bytes()),
            EnvelopeError::Sender { sender: 99 },
        ),
        (
            "this node as the sender",
            1,
            patched(&envelope, 60, &1_u64.to_le_bytes()),
            EnvelopeError::Sender { sender: 1 },
        ),
        (
            "sender 0",
            2,
            patched(&envelope, 60, &0_u64.to_le_bytes()),
            EnvelopeError::Sender { sender: 0 },
        ),
        (
            "a sender other than the node the transport received it from",
            3,
            envelope.clone(),
            EnvelopeError::Sender { sender: 2 },
        ),
        (
            "another voter as the destination",
            2,
            patched(&envelope, 68, &3_u64.to_le_bytes()),
            EnvelopeError::Destination { destination: 3 },
        ),
        (
            "a destination outside the voters",
            2,
            patched(&envelope, 68, &99_u64.to_le_bytes()),
            EnvelopeError::Destination { destination: 99 },
        ),
    ];
    let mut receiver = codec(1);
    for (count, (case, from, bytes, expected)) in (1..).zip(cases) {
        let before = receiver.dropped();
        assert_eq!(
            receiver.open(id(from), &bytes),
            Err(expected),
            "rule: {case} is dropped before Raft sees the message (§8.2)"
        );
        assert_eq!(
            receiver.dropped().total(),
            count,
            "rule: every dropped envelope is counted (§8.2); case={case}"
        );
        assert_ne!(
            receiver.dropped(),
            before,
            "rule: the drop is counted under its reason; case={case}"
        );
    }
    assert_eq!(
        receiver.dropped(),
        DropCounts {
            protocol_version: 2,
            instance: 1,
            fingerprint: 1,
            sender: 4,
            destination: 2,
            ..DropCounts::default()
        },
        "rule: each drop is counted under its own field"
    );
}

#[test]
fn the_message_inside_must_be_admissible_and_bound_to_the_envelope() {
    let mut receiver = codec(1);
    let mut propose = eraftpb::Message::default();
    propose.set_msg_type(MessageType::MsgPropose);
    propose.from = 2;
    propose.to = 1;
    let propose = crate::convert::outbound(&propose).unwrap();
    assert_eq!(
        receiver.open(id(2), &codec(2).seal(&propose).unwrap()),
        Err(EnvelopeError::Message(Inadmissible::MessageType {
            code: 2
        })),
        "rule: a message v1 peers never exchange is dropped even in a valid envelope \
         (br-xx08)"
    );
    // Node 3 seals node 2's heartbeat: the envelope is from 3, the message from 2.
    assert_eq!(
        receiver.open(id(3), &codec(3).seal(&heartbeat()).unwrap()),
        Err(EnvelopeError::Binding { from: 2 }),
        "rule: the message's own sender must be the envelope's (§8.2)"
    );
    let mut garbage = heartbeat();
    garbage.bytes = vec![0xff; 5];
    assert_eq!(
        receiver.open(id(2), &codec(2).seal(&garbage).unwrap()),
        Err(EnvelopeError::Message(Inadmissible::Undecodable)),
        "rule: a payload that is not a Raft message is dropped"
    );
    assert_eq!(
        receiver.dropped(),
        DropCounts {
            message: 2,
            binding: 1,
            ..DropCounts::default()
        },
        "rule: each drop is counted under its own reason"
    );
}

#[test]
fn malformed_and_oversized_envelopes_drop() {
    let envelope = sealed();
    let mut trailing = envelope.clone();
    trailing.push(0);
    let mut short_payload = envelope.clone();
    short_payload.pop();
    let longer = u32::try_from(envelope.len() - HEADER_BYTES + 1).unwrap();
    let cases: [(&str, Vec<u8>); 8] = [
        ("no bytes", Vec::new()),
        ("three bytes", vec![b'K', b'V', b'P']),
        ("another magic", patched(&envelope, 0, b"KVPF")),
        ("a header cut short", envelope[..HEADER_BYTES - 1].to_vec()),
        ("a header and no payload length", envelope[..76].to_vec()),
        ("a trailing byte", trailing),
        ("a payload shorter than its length", short_payload),
        (
            "a payload length past the bytes received",
            patched(&envelope, 76, &longer.to_le_bytes()),
        ),
    ];
    let mut receiver = codec(1);
    for (case, bytes) in cases {
        assert_eq!(
            receiver.open(id(2), &bytes),
            Err(EnvelopeError::Malformed),
            "rule: {case} is not an envelope"
        );
    }
    let too_long = vec![0; usize::try_from(MAX_ENVELOPE_BYTES).unwrap() + 1];
    assert_eq!(
        receiver.open(id(2), &too_long),
        Err(EnvelopeError::TooLarge {
            len: MAX_ENVELOPE_BYTES + 1
        }),
        "rule: an envelope past the bound is dropped before it is read (§7.11)"
    );
    assert_eq!(
        receiver.dropped(),
        DropCounts {
            malformed: 8,
            too_large: 1,
            ..DropCounts::default()
        },
        "rule: malformed and oversized envelopes are counted"
    );
}

#[test]
fn sealing_refuses_non_peers_and_messages_past_the_bound() {
    let sender = codec(1);
    for to in [1, 4, 99] {
        let message = OutboundMessage {
            to: id(to),
            bytes: vec![1],
        };
        assert_eq!(
            sender.seal(&message),
            Err(SealError::Destination { to: id(to) }),
            "rule: an envelope goes only to another voter; to={to}"
        );
    }
    let largest = usize::try_from(MAX_FRAME_PAYLOAD_BYTES).unwrap();
    let at_bound = OutboundMessage {
        to: id(2),
        bytes: vec![0xff; largest],
    };
    let envelope = sender.seal(&at_bound).unwrap();
    assert_eq!(
        u64::try_from(envelope.len()).unwrap(),
        MAX_ENVELOPE_BYTES,
        "rule: a message at the bound seals into the longest envelope"
    );
    assert_eq!(
        codec(2).open(id(1), &envelope),
        Err(EnvelopeError::Message(Inadmissible::Undecodable)),
        "rule: the longest envelope is read, not refused for its length"
    );
    let past_bound = OutboundMessage {
        to: id(2),
        bytes: vec![0; largest + 1],
    };
    assert_eq!(
        sender.seal(&past_bound),
        Err(SealError::TooLarge { len: largest + 1 }),
        "rule: a message past the bound is never sealed (§7.11)"
    );
}

#[test]
fn a_codec_belongs_to_one_of_its_voters() {
    assert_eq!(
        PeerCodec::new(
            InstanceId::from_bytes(INSTANCE),
            SemanticFingerprint::from_bytes(FINGERPRINT),
            id(4),
            [1, 2, 3].map(id),
        ),
        Err(IdentityError::NotAVoter { node: id(4) }),
        "rule: a node outside the voter set has no peer identity"
    );
    let unsorted = PeerCodec::new(
        InstanceId::from_bytes(INSTANCE),
        SemanticFingerprint::from_bytes(FINGERPRINT),
        id(1),
        [3, 1, 2, 3].map(id),
    )
    .unwrap();
    assert_eq!(
        unsorted,
        codec(1),
        "rule: the voter set is a set: order and repeats do not matter"
    );
    assert_eq!(unsorted.node(), id(1), "rule: the codec knows its node");
}

/// A `SplitMix64` stream: these tests' only randomness, seeded (§7.14).
struct Bytes(u64);

impl Bytes {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
    }

    fn fill(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next().to_le_bytes()[0]).collect()
    }
}

#[test]
fn arbitrary_bytes_never_panic_and_mismatched_identity_always_drops() {
    let mut stream = Bytes(0x5eed);
    let mut receiver = codec(1);
    let mut drops = 0;
    for _ in 0..5_000 {
        let len = stream.below(200);
        let bytes = stream.fill(len);
        assert!(
            receiver.open(id(2), &bytes).is_err(),
            "rule: random bytes never open (§10.19); bytes={bytes:02x?}"
        );
        drops += 1;
    }
    let envelope = sealed();
    // Every header byte is checked: changing any one of them drops the envelope.
    for offset in 0..HEADER_BYTES {
        for flip in [0x01, 0x80, 0xff] {
            let mut mutated = envelope.clone();
            mutated[offset] ^= flip;
            assert!(
                receiver.open(id(2), &mutated).is_err(),
                "rule: a changed header byte always drops (§10.19); offset={offset} \
                 flip={flip:#04x}"
            );
            drops += 1;
        }
    }
    // A changed payload byte may still be a valid heartbeat; it must never panic.
    for offset in HEADER_BYTES..envelope.len() {
        for _ in 0..8 {
            let mut mutated = envelope.clone();
            mutated[offset] = stream.fill(1)[0];
            if receiver.open(id(2), &mutated).is_err() {
                drops += 1;
            }
        }
    }
    assert_eq!(
        receiver.dropped().total(),
        drops,
        "rule: every drop is counted, and nothing else is"
    );
}

/// One member of an in-memory cluster.
struct Member {
    node: RaftNode<Journal>,
    journal: Journal,
    codec: PeerCodec,
}

/// Runs every Ready member `at` has, persisting each frame; returns the envelopes it sends,
/// sealed, with their destinations.
fn drain(member: &mut Member) -> Vec<(NodeId, Vec<u8>)> {
    let mut sent = Vec::new();
    while let Some(ready) = member.node.ready() {
        if let Some(frame) = member.node.persist().unwrap() {
            member.journal.push(&frame);
            member.node.synced();
        }
        let light = member.node.advance();
        let messages = ready.messages().iter().chain(&light.messages);
        sent.extend(messages.map(|message| (message.to, member.codec.seal(message).unwrap())));
        if let Some(last) = ready.committed().iter().chain(&light.committed).last() {
            member.node.applied(last.index.index());
        }
    }
    sent
}

/// Delivers every envelope, opening each at its destination, until the cluster is quiet.
fn deliver(members: &mut [Member], mut in_flight: Vec<(NodeId, NodeId, Vec<u8>)>) {
    while !in_flight.is_empty() {
        for (from, to, envelope) in std::mem::take(&mut in_flight) {
            let member = members
                .iter_mut()
                .find(|member| member.codec.node() == to)
                .unwrap();
            let message = member.codec.open(from, &envelope).unwrap_or_else(|error| {
                panic!("rule: a message the library sends always opens; {from}->{to}: {error:?}")
            });
            member.node.step(message).unwrap();
            for (next, sealed) in drain(member) {
                in_flight.push((to, next, sealed));
            }
        }
    }
}

#[test]
fn a_three_voter_cluster_elects_and_commits_with_every_message_sealed() {
    let config: Config = config("three-voters");
    let mut members: Vec<Member> = config
        .voters()
        .map(|node| {
            let journal = Journal::default();
            Member {
                node: RaftNode::new(
                    &config.consensus(node).unwrap(),
                    config.voters(),
                    empty_log(),
                    journal.clone(),
                    Seeded::new(node.get()),
                    LogIndex::ZERO,
                )
                .unwrap(),
                journal,
                codec: PeerCodec::new(
                    InstanceId::from_bytes(INSTANCE),
                    config.semantic_fingerprint(),
                    node,
                    config.voters(),
                )
                .unwrap(),
            }
        })
        .collect();
    let mut leader = None;
    for _ in 0..1_000 {
        for at in 0..members.len() {
            members[at].node.tick();
            let from = members[at].codec.node();
            let sent = drain(&mut members[at]);
            deliver(
                &mut members,
                sent.into_iter()
                    .map(|(to, sealed)| (from, to, sealed))
                    .collect(),
            );
        }
        leader = members
            .iter()
            .position(|member| member.node.status().role == Role::Leader);
        if leader.is_some() {
            break;
        }
    }
    let leader = leader.expect("rule: a three-voter cluster elects a leader through envelopes");
    members[leader].node.propose(b"put".to_vec()).unwrap();
    let from = members[leader].codec.node();
    let sent = drain(&mut members[leader]);
    deliver(
        &mut members,
        sent.into_iter()
            .map(|(to, sealed)| (from, to, sealed))
            .collect(),
    );
    for _ in 0..10 {
        for at in 0..members.len() {
            members[at].node.tick();
            let from = members[at].codec.node();
            let sent = drain(&mut members[at]);
            deliver(
                &mut members,
                sent.into_iter()
                    .map(|(to, sealed)| (from, to, sealed))
                    .collect(),
            );
        }
    }
    let commit = members[leader].node.status().commit;
    assert!(
        commit >= LogIndex::new(2).unwrap(),
        "rule: the leader commits its no-op and the proposal; commit={commit:?}"
    );
    for member in &members {
        let node = member.codec.node();
        assert_eq!(
            member.node.applied_index(),
            commit,
            "rule: every voter applies the committed prefix; node={node}"
        );
        assert_eq!(
            member.codec.dropped(),
            DropCounts::default(),
            "rule: no message the library sends is ever dropped; node={node}"
        );
    }
}

#[test]
fn a_raft_message_decodes_from_the_payload_it_was_sealed_with() {
    let message = heartbeat();
    let opened = codec(1)
        .open(id(2), &codec(2).seal(&message).unwrap())
        .unwrap();
    let decoded = eraftpb::Message::parse_from_bytes(&opened).unwrap();
    assert_eq!(
        (
            decoded.get_msg_type(),
            decoded.from,
            decoded.to,
            decoded.term
        ),
        (MessageType::MsgHeartbeat, 2, 1, 3),
        "rule: the payload is the library's own message"
    );
}
