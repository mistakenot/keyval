//! The golden vectors (PLAN.md §7.6, DUR-5) and the record tests. Each `golden/*.bin`
//! file was built field by field from §7.6 and `golden/README.md`, with checksums from an
//! independent bitwise CRC-32C, not by this crate's encoder. Goldens are oracles (P9): a
//! format change adds new files and bumps the version, never regenerates these.
#![cfg(test)]

use std::path::Path;

use types::{
    ClusterId, ClusterIdError, Config, ENTRY_ENVELOPE_BOUND, Entry, EntryIndex, EntryPayload,
    HardState, InstanceId, JOURNAL_PROTOCOL_ALLOWANCE, LogIndex, NodeId, OutOfRange, Quantity,
    SemanticFingerprint, Term,
};

use crate::{FrameKind, Genesis, RaftBatch, RecordError, decode, encode};

/// The repository's local cluster config.
const LOCAL: &str = include_str!("../../../config/local.toml");

/// An entry at `index` in `term`, empty or carrying `command`.
fn entry(index: u64, term: u64, command: Option<&[u8]>) -> Entry {
    Entry {
        index: EntryIndex::new(index).unwrap(),
        term: Term::new(term).unwrap(),
        payload: command.map_or(EntryPayload::Empty, |bytes| {
            EntryPayload::Command(bytes.to_vec())
        }),
    }
}

/// A hard state.
fn hard_state(term: u64, vote: u64, commit: u64) -> HardState {
    HardState {
        term: Term::new(term).unwrap(),
        vote: NodeId::new(vote).ok(),
        commit: LogIndex::new(commit).unwrap(),
    }
}

/// The fixed 32-byte fingerprint in the genesis golden: `config/local.toml`'s under
/// fingerprint encoding v1. The golden pins the frame format, not the fingerprint
/// algorithm, so it is a literal here.
const GOLDEN_FINGERPRINT: &str = "3b2e3ed34b0f5f49312b1d00bcebdc9f5100d408ae2209daf7a4c1695ec3c26c";

/// The golden genesis, from literal parts: instance 00112233…eeff, cluster
/// `keyval-local-v1`, node 1, voters 1–5, [`GOLDEN_FINGERPRINT`].
fn golden_genesis() -> Genesis {
    let fingerprint: Vec<u8> = (0..32)
        .map(|at| u8::from_str_radix(&GOLDEN_FINGERPRINT[2 * at..2 * at + 2], 16).unwrap())
        .collect();
    Genesis::from_parts(
        InstanceId::parse("00112233445566778899aabbccddeeff").unwrap(),
        ClusterId::parse("keyval-local-v1".to_owned()).unwrap(),
        NodeId::new(1).unwrap(),
        (1..=5).map(|id| NodeId::new(id).unwrap()).collect(),
        SemanticFingerprint::from_bytes(fingerprint.try_into().unwrap()),
    )
    .unwrap()
}

/// Asserts that `golden` is the frame of `kind` and `sequence` carrying `payload`, both
/// ways: decoding gives the payload, and encoding gives the identical bytes.
fn assert_golden(name: &str, golden: &[u8], kind: FrameKind, sequence: u64, payload: &[u8]) {
    let frame = decode(golden).unwrap_or_else(|error| panic!("rule: {name} decodes; {error}"));
    assert_eq!(
        (
            frame.header().kind(),
            frame.header().sequence(),
            frame.payload()
        ),
        (kind, sequence, payload),
        "rule: the golden decodes to the expected record (§7.6, DUR-5); golden={name}"
    );
    assert_eq!(
        frame.header().frame_len(),
        golden.len(),
        "rule: the golden is exactly one frame; golden={name}"
    );
    assert_eq!(
        encode(kind, sequence, payload).unwrap(),
        golden,
        "rule: encoding the expected record gives identical bytes (§7.6, DUR-5); golden={name}"
    );
}

#[test]
fn the_genesis_golden_round_trips() {
    let genesis = golden_genesis();
    let golden = include_bytes!("../golden/genesis.bin");
    assert_golden(
        "genesis.bin",
        golden,
        FrameKind::Genesis,
        1,
        &genesis.encode(),
    );
    let decoded = Genesis::decode(decode(golden).unwrap().payload()).unwrap();
    assert_eq!(decoded, genesis, "rule: decode(encode(genesis)) == genesis");
    assert_eq!(
        decoded.fingerprint().to_string(),
        GOLDEN_FINGERPRINT,
        "rule: genesis carries the fingerprint's 32 bytes unchanged (DUR-8)"
    );
    let voters: Vec<u64> = decoded.voters().iter().map(|id| id.get()).collect();
    assert_eq!(
        voters,
        [1, 2, 3, 4, 5],
        "rule: the canonical voter set (§7.10)"
    );
}

#[test]
fn the_raft_batch_goldens_round_trip() {
    let cases: [(&str, &[u8], u64, RaftBatch); 3] = [
        (
            "raft-batch.bin",
            include_bytes!("../golden/raft-batch.bin"),
            2,
            RaftBatch::new(
                vec![
                    entry(1, 1, None),
                    entry(2, 1, Some(b"golden command bytes")),
                ],
                Some(hard_state(1, 1, 1)),
            )
            .unwrap(),
        ),
        (
            "raft-batch-entry-only.bin",
            include_bytes!("../golden/raft-batch-entry-only.bin"),
            3,
            RaftBatch::new(vec![entry(3, 2, Some(b"x"))], None).unwrap(),
        ),
        (
            "raft-batch-vote-only.bin",
            include_bytes!("../golden/raft-batch-vote-only.bin"),
            4,
            RaftBatch::new(Vec::new(), Some(hard_state(3, 0, 2))).unwrap(),
        ),
    ];
    for (name, golden, sequence, batch) in cases {
        assert_golden(
            name,
            golden,
            FrameKind::RaftBatch,
            sequence,
            &batch.encode(),
        );
        assert_eq!(
            RaftBatch::decode(decode(golden).unwrap().payload()),
            Ok(batch),
            "rule: decode(encode(batch)) == batch; golden={name}"
        );
    }
}

#[test]
fn a_genesis_from_a_config_binds_its_voters_and_current_fingerprint() {
    let config = Config::parse(LOCAL, Path::new("config")).unwrap();
    let instance = InstanceId::from_bytes([7; 16]);
    assert!(Genesis::new(&config, NodeId::new(6).unwrap(), instance).is_none());
    let genesis = Genesis::new(&config, NodeId::new(5).unwrap(), instance).unwrap();
    assert_eq!(
        genesis.fingerprint(),
        config.semantic_fingerprint(),
        "rule: init binds the config's semantic fingerprint (DUR-8)"
    );
    assert_eq!(
        genesis.voters().len(),
        5,
        "rule: the canonical voter set (§7.10)"
    );
    assert_eq!(
        Genesis::decode(&genesis.encode()),
        Ok(genesis),
        "rule: it round-trips"
    );
}

#[test]
fn every_malformed_genesis_is_refused_with_its_own_error() {
    let good = golden_genesis().encode();
    let voters_at = 2 + 16 + 1 + 15 + 8; // version, instance, cluster length and bytes, node
    let with = |at: usize, bytes: &[u8]| {
        let mut payload = good.clone();
        payload[at..at + bytes.len()].copy_from_slice(bytes);
        payload
    };
    let mut trailing = good.clone();
    trailing.push(0);
    let cases = [
        (
            good[..10].to_vec(),
            RecordError::Truncated {
                field: "instance ID",
            },
        ),
        (trailing, RecordError::TrailingBytes { extra: 1 }),
        (
            with(0, &2_u16.to_le_bytes()),
            RecordError::UnsupportedFormat { version: 2 },
        ),
        (
            with(18, &[0]),
            RecordError::ClusterId(ClusterIdError::Empty),
        ),
        (with(19, &[0xff]), RecordError::ClusterIdNotUtf8),
        (
            with(34, &0_u64.to_le_bytes()),
            RecordError::NodeId(OutOfRange {
                quantity: Quantity::NodeId,
                value: 0,
            }),
        ),
        (with(voters_at, &[4]), RecordError::VoterCount { count: 4 }),
        (
            with(voters_at + 1, &9_u64.to_le_bytes()),
            RecordError::VotersNotCanonical,
        ),
        (
            with(34, &6_u64.to_le_bytes()),
            RecordError::NodeNotAVoter {
                node: NodeId::new(6).unwrap(),
            },
        ),
    ];
    for (payload, expected) in cases {
        assert_eq!(
            Genesis::decode(&payload),
            Err(expected.clone()),
            "rule: a malformed genesis is refused with a specific error (§7.6); expected={expected}"
        );
    }
}

#[test]
fn every_malformed_raft_batch_is_refused_with_its_own_error() {
    let good = RaftBatch::new(vec![entry(5, 1, Some(b"ab"))], Some(hard_state(1, 2, 4)))
        .unwrap()
        .encode();
    let with = |at: usize, bytes: &[u8]| {
        let mut payload = good.clone();
        payload[at..at + bytes.len()].copy_from_slice(bytes);
        payload
    };
    let empty_entry = |index: u64| {
        [
            index.to_le_bytes().as_slice(),
            &1_u64.to_le_bytes(),
            &[0],
            &0_u32.to_le_bytes(),
        ]
        .concat()
    };
    let gap = [
        2_u32.to_le_bytes().as_slice(),
        &empty_entry(5),
        &empty_entry(7),
        &[0],
    ]
    .concat();
    let flag_at = 4 + 21 + 2;
    let cases = [
        (
            good[..3].to_vec(),
            RecordError::Truncated {
                field: "entry count",
            },
        ),
        (
            with(4, &0_u64.to_le_bytes()),
            RecordError::EntryIndex(OutOfRange {
                quantity: Quantity::EntryIndex,
                value: 0,
            }),
        ),
        (with(20, &[9]), RecordError::UnknownPayloadKind { code: 9 }),
        (with(20, &[0]), RecordError::EmptyEntryWithData { len: 2 }),
        (
            with(21, &(70_000_u32).to_le_bytes()),
            RecordError::CommandTooLarge { len: 70_000 },
        ),
        (with(flag_at, &[2]), RecordError::HardStateFlag { value: 2 }),
        (
            good[..flag_at + 5].to_vec(),
            RecordError::Truncated {
                field: "hard-state term",
            },
        ),
        (
            gap,
            RecordError::EntryGap {
                expected: 6,
                found: 7,
            },
        ),
        (vec![0, 0, 0, 0, 0], RecordError::EmptyBatch),
    ];
    for (payload, expected) in cases {
        assert_eq!(
            RaftBatch::decode(&payload),
            Err(expected.clone()),
            "rule: a malformed batch is refused with a specific error (§7.6); expected={expected}"
        );
    }
}

#[test]
fn an_entry_s_journal_overhead_fits_the_margin_formula() {
    // §7.10, D-68: 64 B per entry = a frame's 32 B + an entry envelope of at most 32 B.
    let command = vec![0_u8; 100];
    let batch = RaftBatch::new(vec![entry(1, 1, Some(&command))], None).unwrap();
    let envelope = batch.encode().len() - command.len();
    assert_eq!(envelope, 26, "rule: count 4 + entry 21 + flag 1");
    assert!(
        u64::try_from(envelope).unwrap() <= ENTRY_ENVELOPE_BOUND,
        "rule: a one-entry batch's envelope fits ENTRY_ENVELOPE_BOUND (W08)"
    );
    let with_state = RaftBatch::new(vec![entry(1, 1, Some(&command))], Some(hard_state(1, 1, 1)))
        .unwrap()
        .encode()
        .len()
        - command.len();
    let state_bytes = u64::try_from(with_state - envelope).unwrap();
    assert_eq!(
        state_bytes, 24,
        "rule: a hard state adds term, vote and commit"
    );
    assert!(
        state_bytes * types::ConfigParam::MaxLogEntries.maximum() <= JOURNAL_PROTOCOL_ALLOWANCE,
        "rule: a hard state on every frame, at the most entries a config allows, stays \
         inside the protocol allowance (§7.10)"
    );
}

#[test]
fn a_batch_must_carry_something_and_be_consecutive() {
    assert_eq!(
        RaftBatch::new(Vec::new(), None),
        Err(RecordError::EmptyBatch)
    );
    assert_eq!(
        RaftBatch::new(vec![entry(1, 1, None), entry(3, 1, None)], None),
        Err(RecordError::EntryGap {
            expected: 2,
            found: 3
        }),
        "rule: a batch's entries are consecutive"
    );
    let too_large = vec![0_u8; 66 * 1024 + 1];
    assert_eq!(
        RaftBatch::new(vec![entry(1, 1, Some(&too_large))], None),
        Err(RecordError::CommandTooLarge { len: 66 * 1024 + 1 }),
        "rule: a command is at most 66 KiB (§7.11)"
    );
}
