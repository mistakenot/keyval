//! Raw-byte tests of peer-message decoding (PLAN.md §7.11, CD-3, CD-9; W02 audit H-3):
//! the bounded wire scan (br-umjh) and the canonical-bytes rule (br-808c), through
//! `decode_message`, admission and an envelope.
#![cfg(test)]

use protobuf::Message as _;
use raft::eraftpb::{self, MessageType};
use types::{ConfigParam, InstanceId, NodeId, OutboundMessage, SemanticFingerprint};

use crate::admit::{Inadmissible, admit};
use crate::convert::decode_message;
use crate::envelope::{EnvelopeError, PeerCodec};
use crate::wire::{MAX_CONF_STATE_IDS, MAX_ENTRIES, Malformed, scan};

/// Field 1000 (unknown to this build) as a group: start, a varint field 1 inside, end.
const GROUP: [u8; 6] = [0xc3, 0x3e, 0x08, 0x01, 0xc4, 0x3e];

fn id(raw: u64) -> NodeId {
    NodeId::new(raw).unwrap()
}

/// A message of `kind` from node 2 to node 1 in term 3.
fn message(kind: MessageType) -> eraftpb::Message {
    let mut message = eraftpb::Message::default();
    message.set_msg_type(kind);
    message.to = 1;
    message.from = 2;
    message.term = 3;
    message
}

/// An entry at `index` in term 3 carrying `data`.
fn entry(index: u64, data: &[u8]) -> eraftpb::Entry {
    eraftpb::Entry {
        index,
        term: 3,
        data: data.to_vec().into(),
        ..eraftpb::Entry::default()
    }
}

fn encode(message: &eraftpb::Message) -> Vec<u8> {
    message.write_to_bytes().unwrap()
}

/// An append whose entry field (7) holds `entry_bytes` as written, after the message's
/// own fields up to its index: where the encoder writes entries.
fn append_with_raw_entry(entry_bytes: &[u8]) -> Vec<u8> {
    let mut bytes = encode(&message(MessageType::MsgAppend));
    bytes.push(0x3a);
    bytes.push(u8::try_from(entry_bytes.len()).unwrap());
    bytes.extend_from_slice(entry_bytes);
    bytes
}

/// Node 1's envelope codec among voters 1 to 3, and a sealed envelope from node 2 holding
/// `payload`.
fn sealed_to_node_1(payload: Vec<u8>) -> (PeerCodec, Vec<u8>) {
    let codec = |node| {
        PeerCodec::new(
            InstanceId::from_bytes([7; 16]),
            SemanticFingerprint::from_bytes([9; 32]),
            id(node),
            [1, 2, 3].map(id),
        )
        .unwrap()
    };
    let envelope = codec(2)
        .seal(&OutboundMessage {
            to: id(1),
            bytes: payload,
        })
        .unwrap();
    (codec(1), envelope)
}

#[test]
fn every_message_v1_sends_decodes_unchanged() {
    let mut append = message(MessageType::MsgAppend);
    append.log_term = 2;
    append.index = 4;
    append.commit = 4;
    append.entries = vec![entry(5, b""), entry(6, b"put k v")].into();
    let mut rejection = message(MessageType::MsgAppendResponse);
    rejection.reject = true;
    rejection.reject_hint = 4;
    rejection.log_term = 2;
    let cases = [
        message(MessageType::MsgHeartbeat),
        message(MessageType::MsgHeartbeatResponse),
        message(MessageType::MsgRequestVote),
        message(MessageType::MsgRequestVoteResponse),
        message(MessageType::MsgRequestPreVote),
        message(MessageType::MsgRequestPreVoteResponse),
        message(MessageType::MsgAppendResponse),
        rejection,
        append,
    ];
    for case in cases {
        assert_eq!(
            decode_message(&encode(&case)),
            Some(case.clone()),
            "rule: the generated encoder's bytes are canonical and decode unchanged; {:?}",
            case.get_msg_type()
        );
    }
}

#[test]
fn groups_are_refused_on_the_message_and_in_an_entry() {
    let mut top = encode(&message(MessageType::MsgHeartbeat));
    top.extend_from_slice(&GROUP);
    let mut inner = vec![0x10, 0x03, 0x18, 0x05];
    inner.extend_from_slice(&GROUP);
    let in_entry = append_with_raw_entry(&inner);
    for (case, bytes) in [("on the message", top), ("in an entry", in_entry)] {
        assert!(
            eraftpb::Message::parse_from_bytes(&bytes).is_ok(),
            "fixture: the runtime skips a group without a trace; case={case}"
        );
        assert_eq!(
            (scan(&bytes), decode_message(&bytes)),
            (Err(Malformed), None),
            "rule: a group is refused before parsing, at every depth (H-3); case={case}"
        );
        assert_eq!(
            admit(&bytes, id(1), &[1, 2, 3].map(id)),
            Err(Inadmissible::Undecodable),
            "rule: admission refuses it as undecodable; case={case}"
        );
    }
}

#[test]
fn unknown_fields_of_every_wire_type_and_mistyped_known_fields_are_refused() {
    let unknown: [(&str, &[u8]); 4] = [
        ("varint", &[0xc0, 0x3e, 0x01]),
        ("fixed64", &[0xc1, 0x3e, 1, 2, 3, 4, 5, 6, 7, 8]),
        ("length-delimited", &[0xc2, 0x3e, 0x01, 0x00]),
        ("fixed32", &[0xc5, 0x3e, 1, 2, 3, 4]),
    ];
    for (wire, field) in unknown {
        let mut top = encode(&message(MessageType::MsgHeartbeat));
        top.extend_from_slice(field);
        let mut inner = vec![0x10, 0x03, 0x18, 0x05];
        inner.extend_from_slice(field);
        for (depth, bytes) in [("message", top), ("entry", append_with_raw_entry(&inner))] {
            assert_eq!(
                scan(&bytes),
                Err(Malformed),
                "rule: an unknown {wire} field is refused before parsing (H-3); depth={depth}"
            );
        }
    }
    // Field 4 (term) written as fixed64, not varint.
    let mut mistyped = encode(&message(MessageType::MsgHeartbeat));
    mistyped.extend_from_slice(&[0x21, 3, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        scan(&mistyped),
        Err(Malformed),
        "rule: a known field must have its own wire type"
    );
}

#[test]
fn lengths_past_the_end_and_bad_varints_are_refused_before_anything_is_reserved() {
    let mut heartbeat = encode(&message(MessageType::MsgHeartbeat));
    let cases: [(&str, Vec<u8>); 5] = [
        // Field 12 (context) declaring 9,999,999 bytes in five: the runtime would reserve
        // the whole buffer before looking for the bytes (br-umjh).
        ("a context's length", vec![0x62, 0xff, 0xac, 0xe2, 0x04]),
        // An unknown length-delimited field declaring the same.
        (
            "an unknown field's length",
            vec![0xc2, 0x3e, 0xff, 0xac, 0xe2, 0x04],
        ),
        // An entry declaring 16,383 bytes, with four present.
        (
            "an entry's length",
            vec![0x3a, 0xff, 0x7f, 0x10, 0x03, 0x18, 0x05],
        ),
        // An eleven-byte varint.
        (
            "an over-long varint",
            [vec![0x20], vec![0xff; 10], vec![0x01]].concat(),
        ),
        // A ten-byte varint past 64 bits.
        (
            "an overflowing varint",
            [vec![0x20], vec![0xff; 9], vec![0x02]].concat(),
        ),
    ];
    for (case, tail) in cases {
        heartbeat.truncate(encode(&message(MessageType::MsgHeartbeat)).len());
        heartbeat.extend_from_slice(&tail);
        assert_eq!(
            (scan(&heartbeat), decode_message(&heartbeat)),
            (Err(Malformed), None),
            "rule: the scan refuses {case} before the runtime reserves anything (CD-9)"
        );
    }
    // An entry whose declared length runs past its own fields into the message's: the
    // scan reads the swallowed commit (field 8) as an entry field, which an entry lacks.
    let over_long = [
        encode(&message(MessageType::MsgAppend)),
        vec![0x3a, 0x06, 0x10, 0x03, 0x18, 0x05, 0x40, 0x01],
    ]
    .concat();
    assert_eq!(
        decode_message(&over_long),
        None,
        "rule: an over-long nested length is refused"
    );
}

#[test]
fn a_flood_of_empty_entries_is_refused_before_it_is_parsed() {
    assert_eq!(
        (
            MAX_ENTRIES,
            ConfigParam::ReplicationBatchBytes.maximum() / 4 + 1
        ),
        (65_537, 65_537),
        "rule: the bound is the largest replication batch over the smallest admissible entry, \
         plus the one entry raft-rs always sends (§7.11)"
    );
    let smallest = eraftpb::Entry {
        term: 1,
        index: 1,
        ..eraftpb::Entry::default()
    };
    assert_eq!(
        smallest.compute_size(),
        4,
        "rule: the smallest admissible entry (term and index at least 1) computes to 4 bytes"
    );
    let empty_entry = [0x3a_u8, 0x00];
    let flood = empty_entry.repeat(524_288);
    assert_eq!(
        (scan(&flood), decode_message(&flood)),
        (Err(Malformed), None),
        "rule: 1 MiB of empty entries is refused before the runtime builds 524,288 of them \
         (br-umjh, CD-9)"
    );
    let cap = usize::try_from(MAX_ENTRIES).unwrap();
    assert_eq!(
        (
            scan(&empty_entry.repeat(cap)),
            scan(&empty_entry.repeat(cap + 1))
        ),
        (Ok(()), Err(Malformed)),
        "rule: the scan admits exactly MAX_ENTRIES entries (N, N+1)"
    );
}

#[test]
fn the_most_entries_a_v1_append_carries_still_decode() {
    let mut append = message(MessageType::MsgAppend);
    append.log_term = 3;
    append.entries = (1..=MAX_ENTRIES)
        .map(|index| entry(index, b""))
        .collect::<Vec<_>>()
        .into();
    let bytes = encode(&append);
    assert!(
        u64::try_from(bytes.len()).unwrap() <= types::MAX_FRAME_PAYLOAD_BYTES,
        "fixture: the append fits an envelope; {} bytes",
        bytes.len()
    );
    assert_eq!(
        decode_message(&bytes).map(|decoded| decoded.entries.len()),
        Some(usize::try_from(MAX_ENTRIES).unwrap()),
        "rule: an append at the bound still decodes"
    );
}

#[test]
fn encodings_the_generated_encoder_never_writes_are_refused() {
    let heartbeat = encode(&message(MessageType::MsgHeartbeat));
    let cases: [(&str, Vec<u8>); 4] = [
        // Field 4 (term) twice: the runtime keeps the last.
        (
            "a repeated field",
            [heartbeat.clone(), vec![0x20, 0x04]].concat(),
        ),
        // Term 3 as a two-byte varint.
        (
            "a non-minimal varint",
            vec![0x08, 0x08, 0x10, 0x01, 0x18, 0x02, 0x20, 0x83, 0x00],
        ),
        // Field 10 (reject) written out as false, its default.
        (
            "a default written out",
            [heartbeat, vec![0x50, 0x00]].concat(),
        ),
        // `to` before `msg_type`.
        (
            "fields out of order",
            vec![0x10, 0x01, 0x08, 0x08, 0x18, 0x02, 0x20, 0x03],
        ),
    ];
    for (case, bytes) in cases {
        assert!(
            eraftpb::Message::parse_from_bytes(&bytes).is_ok() && scan(&bytes).is_ok(),
            "fixture: the runtime and the scan both accept {case}"
        );
        assert_eq!(
            decode_message(&bytes),
            None,
            "rule: only the canonical bytes v1's encoder writes are admitted (br-808c); {case}"
        );
    }
}

#[test]
fn a_snapshot_is_scanned_at_every_depth_and_refused_by_admission() {
    let snapshot_with_ids = |ids: u64| {
        let mut message = message(MessageType::MsgAppend);
        message.mut_snapshot().mut_metadata().index = 9;
        message
            .mut_snapshot()
            .mut_metadata()
            .mut_conf_state()
            .voters = (1..=ids).collect();
        encode(&message)
    };
    let voters = [1, 2, 3].map(id);
    let well_formed = snapshot_with_ids(MAX_CONF_STATE_IDS);
    assert!(
        scan(&well_formed).is_ok() && decode_message(&well_formed).is_some(),
        "rule: a well-formed snapshot decodes, so admission names it in its own order"
    );
    assert_eq!(
        admit(&well_formed, id(1), &voters),
        Err(Inadmissible::Snapshot),
        "rule: admission refuses the snapshot (§7.3)"
    );
    // A snapshot whose ConfState packs `ids` node IDs into field 1, as raw bytes: the
    // generated encoder writes them unpacked, but the runtime parses either.
    let packed_ids = |ids: u8| {
        let conf_state = [&[0x0a, ids][..], &(1..=ids).collect::<Vec<u8>>()].concat();
        let metadata = [
            &[0x0a, u8::try_from(conf_state.len()).unwrap()][..],
            &conf_state,
        ]
        .concat();
        let snapshot = [
            &[0x12, u8::try_from(metadata.len()).unwrap()][..],
            &metadata,
        ]
        .concat();
        [
            encode(&message(MessageType::MsgHeartbeat)),
            vec![0x4a, u8::try_from(snapshot.len()).unwrap()],
            snapshot,
        ]
        .concat()
    };
    let cap = u8::try_from(MAX_CONF_STATE_IDS).unwrap();
    assert_eq!(
        (scan(&packed_ids(cap)), scan(&packed_ids(cap + 1))),
        (Ok(()), Err(Malformed)),
        "rule: packed node IDs count against the bound too (N, N+1)"
    );
    // A ConfState: field 1 packed with two IDs, then field 1000 as a varint.
    let unknown_in_conf_state = [0x0a, 0x02, 0x01, 0x02, 0xc0, 0x3e, 0x01];
    // A SnapshotMetadata holding that ConfState.
    let metadata = [&[0x0a, 0x07][..], &unknown_in_conf_state].concat();
    // A Snapshot holding that SnapshotMetadata.
    let snapshot = [&[0x12, 0x09][..], &metadata].concat();
    let deep_unknown = [
        encode(&message(MessageType::MsgHeartbeat)),
        vec![0x4a, 0x0b],
        snapshot,
    ]
    .concat();
    let mut grouped = encode(&message(MessageType::MsgHeartbeat));
    grouped.extend_from_slice(&[0x4a, 0x06]);
    grouped.extend_from_slice(&GROUP);
    let garbage = [
        encode(&message(MessageType::MsgHeartbeat)),
        vec![0x4a, 0x03, 0xff, 0xff, 0xff],
    ]
    .concat();
    for (case, bytes) in [
        ("an unknown field three levels down", deep_unknown),
        ("a group in the snapshot", grouped),
        ("bytes that are no snapshot", garbage),
        (
            "more node IDs than a v1 configuration has",
            snapshot_with_ids(MAX_CONF_STATE_IDS + 1),
        ),
    ] {
        assert_eq!(
            (scan(&bytes), decode_message(&bytes)),
            (Err(Malformed), None),
            "rule: the scan reaches into the snapshot too (H-3, CD-9); case={case}"
        );
        assert_eq!(
            admit(&bytes, id(1), &voters),
            Err(Inadmissible::Undecodable),
            "rule: admission refuses it as undecodable; case={case}"
        );
    }
}

#[test]
fn an_envelope_holding_a_group_is_dropped() {
    let mut payload = encode(&message(MessageType::MsgHeartbeat));
    payload.extend_from_slice(&GROUP);
    let (mut node_1, envelope) = sealed_to_node_1(payload);
    assert_eq!(
        node_1.open(id(2), &envelope),
        Err(EnvelopeError::Message(Inadmissible::Undecodable)),
        "rule: a group hidden in an otherwise valid envelope never reaches the library (H-3)"
    );
    let (mut node_1, envelope) = sealed_to_node_1(encode(&message(MessageType::MsgHeartbeat)));
    assert!(
        node_1.open(id(2), &envelope).is_ok(),
        "fixture: the same heartbeat without the group opens"
    );
}
