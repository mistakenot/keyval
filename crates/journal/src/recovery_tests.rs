//! The recovery scan's tests (PLAN.md §7.6 "Fail closed", §7.7 steps 0–4, W11.3).
#![cfg(test)]

// check: JOURNAL-RECOVERY

use std::cell::RefCell;
use std::path::Path;

use types::{
    ByteCount, Config, Entry, EntryIndex, EntryPayload, HardState, InstanceId, JournalRead,
    LogIndex, NodeId, ReadLen, StorageError, Term,
};

use crate::{
    FrameError, FrameKind, Genesis, GenesisMismatch, HEADER_LEN, JournalError, LogBounds,
    LogicalLog, MAX_FRAME_LEN, RaftBatch, Recovered, RecoveryError, crc32c, encode, recover,
};

/// The repository's local cluster config.
const LOCAL: &str = include_str!("../../../config/local.toml");

/// The bounds in these tests: a 1 MiB entry cache and a hard stop far away.
const BOUNDS: LogBounds = LogBounds::new(ByteCount::new(1 << 20), ByteCount::new(1 << 40));

/// A journal held in memory: the byte-storage port's read side, recording each read's
/// length.
#[derive(Default)]
struct Memory {
    /// The journal's bytes.
    bytes: Vec<u8>,
    /// Each read's length, in order.
    reads: RefCell<Vec<u32>>,
    /// Whether reads fail.
    fail: bool,
    /// Whether reads return one byte too few.
    short: bool,
}

impl JournalRead for Memory {
    fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
        self.reads.borrow_mut().push(len.get());
        if self.fail {
            return Err(StorageError::Failed);
        }
        let start = usize::try_from(offset.get()).unwrap();
        let end = start + len.get() as usize - usize::from(self.short);
        Ok(self.bytes[start..end].to_vec())
    }
}

/// A length as a journal offset.
fn len(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap()
}

/// An entry at `index` in `term` carrying `bytes` of command.
fn entry(index: u64, term: u64, bytes: usize) -> Entry {
    command(index, term, vec![u8::try_from(index % 251).unwrap(); bytes])
}

/// An entry at `index` in `term` carrying `command`.
fn command(index: u64, term: u64, command: Vec<u8>) -> Entry {
    Entry {
        index: EntryIndex::new(index).unwrap(),
        term: Term::new(term).unwrap(),
        payload: EntryPayload::Command(command),
    }
}

/// A hard state in `term` committing `commit`.
fn state(term: u64, commit: u64) -> HardState {
    HardState {
        term: Term::new(term).unwrap(),
        vote: None,
        commit: LogIndex::new(commit).unwrap(),
    }
}

/// A batch of `entries` with `hard_state`.
fn batch(entries: Vec<Entry>, hard_state: Option<HardState>) -> RaftBatch {
    RaftBatch::new(entries, hard_state).unwrap()
}

/// Three batches: entries 1–2, entry 3 with commit 2, then entries 3–4 replaced in term
/// 2 with commit 3.
fn standard() -> Vec<RaftBatch> {
    vec![
        batch(vec![entry(1, 1, 10), entry(2, 1, 20)], Some(state(1, 0))),
        batch(vec![entry(3, 1, 30)], Some(state(1, 2))),
        batch(vec![entry(3, 2, 40), entry(4, 2, 50)], Some(state(2, 3))),
    ]
}

/// A journal written by the live path, and what recovery must rebuild from it.
struct Journal {
    /// The configuration it was initialised under.
    config: Config,
    /// The node it belongs to.
    node: NodeId,
    /// Its bytes: the genesis frame, then one frame per batch.
    bytes: Vec<u8>,
    /// Each batch frame's offset.
    starts: Vec<u64>,
    /// The live logical log after the last append.
    live: LogicalLog,
}

impl Journal {
    /// The journal of node 2 that holds `batches` after its genesis.
    fn new(batches: Vec<RaftBatch>) -> Self {
        let config = Config::parse(LOCAL, Path::new("config")).unwrap();
        let node = NodeId::new(2).unwrap();
        let genesis = Genesis::new(&config, node, InstanceId::from_bytes([7; 16])).unwrap();
        let mut bytes = encode(FrameKind::Genesis, 1, &genesis.encode()).unwrap();
        let mut live = LogicalLog::after_genesis(len(&bytes), genesis.voters(), BOUNDS);
        let mut starts = Vec::new();
        for batch in batches {
            starts.push(live.end());
            bytes.extend_from_slice(&live.append(batch).unwrap());
        }
        Self {
            config,
            node,
            bytes,
            starts,
            live,
        }
    }

    /// The genesis frame's length.
    fn genesis_len(&self) -> u64 {
        self.starts
            .first()
            .copied()
            .unwrap_or_else(|| len(&self.bytes))
    }

    /// Recovers `bytes` as this journal's node, under its configuration.
    fn recover(&self, bytes: &[u8]) -> Result<Recovered, RecoveryError> {
        let memory = Memory {
            bytes: bytes.to_vec(),
            ..Memory::default()
        };
        recover(
            &memory,
            ByteCount::new(len(bytes)),
            &self.config,
            self.node,
            BOUNDS,
        )
    }
}

/// `bytes` with the byte at `at` flipped.
fn flipped(bytes: &[u8], at: u64) -> Vec<u8> {
    let mut bytes = bytes.to_vec();
    bytes[usize::try_from(at).unwrap()] ^= 0x40;
    bytes
}

/// The first `cut` bytes of `bytes`.
fn prefix(bytes: &[u8], cut: u64) -> &[u8] {
    &bytes[..usize::try_from(cut).unwrap()]
}

/// The bytes of a frame's header that its header checksum covers (§7.6).
const HEADER_CHECKED: usize = 20;

/// `bytes` with the frame starting at `start` (of `frame_len` bytes) given `field` at
/// header offset `at`, and both its checksums made valid again, so that only the field is
/// wrong: an intact, deliberately written header, not crash damage.
fn reforged(bytes: &[u8], start: u64, frame_len: u64, at: usize, field: &[u8]) -> Vec<u8> {
    let mut bytes = bytes.to_vec();
    let start = usize::try_from(start).unwrap();
    let end = start + usize::try_from(frame_len).unwrap();
    bytes[start + at..start + at + field.len()].copy_from_slice(field);
    let header = crc32c(&bytes[start..start + HEADER_CHECKED]);
    bytes[start + HEADER_CHECKED..start + HEADER_LEN].copy_from_slice(&header.to_le_bytes());
    let frame = crc32c(&bytes[start..end - 8]);
    bytes[end - 8..end - 4].copy_from_slice(&frame.to_le_bytes());
    bytes
}

#[test]
fn a_clean_journal_rebuilds_the_live_log() {
    let journal = Journal::new(standard());
    let recovered = journal.recover(&journal.bytes).unwrap();
    assert_eq!(
        recovered.log(),
        &journal.live,
        "rule: replay rebuilds exactly the live log: offset index, hard state, cache \
         (§7.7 step 4, CD-10)"
    );
    assert_eq!(
        recovered.log().hard_state(),
        state(2, 3),
        "rule: the latest hard state"
    );
    assert_eq!(
        recovered.log().last_index(),
        LogIndex::new(4).unwrap(),
        "rule: logical truncations are applied in replay"
    );
    assert_eq!(
        recovered.truncate_to(),
        None,
        "rule: a clean journal has no torn tail"
    );
    assert_eq!(
        recovered.genesis().instance_id(),
        InstanceId::from_bytes([7; 16]),
        "rule: the instance ID comes from genesis (§7.7 step 2)"
    );
    let just_genesis = Journal::new(Vec::new());
    let recovered = just_genesis.recover(&just_genesis.bytes).unwrap();
    assert_eq!(
        (recovered.log().last_index(), recovered.truncate_to()),
        (LogIndex::ZERO, None),
        "rule: a complete genesis and nothing else is an initialised, empty node"
    );
}

#[test]
fn only_a_blank_or_partial_genesis_permits_init() {
    let journal = Journal::new(standard());
    let genesis_len = journal.genesis_len();
    let empty = journal.recover(&[]);
    assert_eq!(
        empty,
        Err(RecoveryError::Empty),
        "rule: a blank start is refused (D-52)"
    );
    assert!(
        empty.unwrap_err().permits_init(),
        "rule: init may run on an empty journal (§7.7 step 0)"
    );
    for cut in 1..genesis_len {
        let result = journal.recover(prefix(&journal.bytes, cut));
        assert_eq!(
            result,
            Err(RecoveryError::IncompleteGenesis),
            "rule: a genesis cut at {cut} of {genesis_len} bytes is refused, and init may \
             re-run (§7.7 step 0)"
        );
        assert!(result.unwrap_err().permits_init(), "cut={cut}");
    }
    let damaged = flipped(prefix(&journal.bytes, genesis_len), genesis_len - 12);
    assert_eq!(
        journal.recover(&damaged),
        Err(RecoveryError::IncompleteGenesis),
        "rule: a full-length genesis failing its checksum is not complete (D-50)"
    );
    let torn_batch = journal
        .recover(prefix(&journal.bytes, genesis_len + 5))
        .unwrap();
    assert_eq!(
        torn_batch.truncate_to(),
        Some(ByteCount::new(genesis_len)),
        "rule: content after a complete genesis is never re-initialised; a torn first \
         batch is truncated"
    );
    let mut followed = flipped(&journal.bytes, genesis_len - 12);
    followed.truncate(usize::try_from(journal.starts[1]).unwrap());
    let refused = journal.recover(&followed).unwrap_err();
    assert_eq!(
        refused,
        RecoveryError::Corruption {
            damaged_at: 0,
            valid_at: genesis_len,
            sequence: 2
        },
        "rule: a damaged genesis with valid frames after it is corruption, never init"
    );
    assert!(
        !refused.permits_init(),
        "rule: init never runs over content (§7.7 step 0)"
    );
    let mut second_genesis = flipped(prefix(&journal.bytes, genesis_len), 3);
    second_genesis.extend_from_slice(prefix(&journal.bytes, genesis_len));
    assert_eq!(
        journal.recover(&second_genesis),
        Err(RecoveryError::Corruption {
            damaged_at: 0,
            valid_at: genesis_len,
            sequence: 1
        }),
        "rule: before any valid frame N is 0, so even a valid genesis after a damaged one          is corruption"
    );
    let mut long = flipped(prefix(&journal.bytes, genesis_len), 3);
    long.resize(usize::try_from(MAX_FRAME_LEN + 1).unwrap(), 0);
    let refused = journal.recover(&long).unwrap_err();
    assert_eq!(
        refused,
        RecoveryError::TailTooLong {
            start: 0,
            len: MAX_FRAME_LEN + 1
        },
        "rule: more than one frame of unreadable bytes is not an interrupted init"
    );
    assert!(
        !refused.permits_init(),
        "rule: TailTooLong never permits init"
    );
}

#[test]
fn an_intact_header_with_an_impossible_field_refuses_and_is_never_truncated() {
    let journal = Journal::new(standard());
    let last = journal.starts[2];
    let last_len = len(&journal.bytes) - last;
    let too_long = u32::try_from(types::MAX_FRAME_PAYLOAD_BYTES + 1).unwrap();
    let cases = [
        (
            "version 2",
            4,
            vec![2, 0],
            FrameError::UnsupportedVersion { version: 2 },
        ),
        ("kind 3", 6, vec![3], FrameError::UnknownKind { code: 3 }),
        (
            "reserved 1",
            7,
            vec![1],
            FrameError::NonZeroReserved { value: 1 },
        ),
        (
            "an over-length payload",
            8,
            too_long.to_le_bytes().to_vec(),
            FrameError::PayloadTooLong {
                len: u64::from(too_long),
            },
        ),
        ("sequence 0", 12, vec![0; 8], FrameError::ZeroSequence),
    ];
    for (what, at, field, error) in cases {
        let bytes = reforged(&journal.bytes, last, last_len, at, &field);
        let refused = journal.recover(&bytes).unwrap_err();
        assert_eq!(
            refused,
            RecoveryError::ImpossibleHeader { at: last, error },
            "rule: a synced final frame whose intact header holds {what} is damage outside \
             the crash model, refused, never cut away as a torn tail with its hard state \
             (DUR-9, DUR-6, br-o69w)"
        );
        assert!(!refused.permits_init(), "{what}");
        let genesis_len = journal.genesis_len();
        let first = reforged(&journal.bytes, 0, genesis_len, at, &field);
        let refused = journal.recover(&first).unwrap_err();
        assert_eq!(
            refused,
            RecoveryError::ImpossibleHeader { at: 0, error },
            "rule: a genesis whose intact header holds {what} (say, a journal of another \
             format version) is never offered to init (§7.7 step 0)"
        );
        assert!(!refused.permits_init(), "{what} at the genesis");
    }
}

#[test]
fn more_bytes_than_one_genesis_frame_without_a_valid_genesis_is_not_an_interrupted_init() {
    let journal = Journal::new(Vec::new());
    let genesis_len = journal.genesis_len();
    let damaged = flipped(&journal.bytes, genesis_len - 12);
    assert_eq!(
        journal.recover(&damaged),
        Err(RecoveryError::IncompleteGenesis),
        "control: a full-length damaged genesis is an interrupted init"
    );
    let mut longer = damaged;
    longer.push(0);
    let refused = journal.recover(&longer).unwrap_err();
    assert_eq!(
        refused,
        RecoveryError::NotAnInterruptedInit {
            len: genesis_len + 1,
            genesis_len
        },
        "rule: an interrupted init leaves at most one genesis frame of bytes; more is \
         content, never re-initialised (§7.7 step 0)"
    );
    assert!(!refused.permits_init());
}

#[test]
fn every_torn_tail_is_truncated_to_the_last_valid_frame() {
    let journal = Journal::new(standard());
    let durable = Journal::new(standard().into_iter().take(2).collect());
    let last = journal.starts[2];
    for cut in last + 1..len(&journal.bytes) {
        let recovered = journal.recover(prefix(&journal.bytes, cut)).unwrap();
        assert_eq!(
            (recovered.log(), recovered.truncate_to()),
            (&durable.live, Some(ByteCount::new(last))),
            "rule: a final frame cut at {cut} is a torn tail, truncated to {last} (DUR-6)"
        );
    }
    for at in [
        last + 4,
        last + 30,
        len(&journal.bytes) - 8,
        len(&journal.bytes) - 1,
    ] {
        let recovered = journal.recover(&flipped(&journal.bytes, at)).unwrap();
        assert_eq!(
            (recovered.log(), recovered.truncate_to()),
            (&durable.live, Some(ByteCount::new(last))),
            "rule: a full-length final frame damaged at {at} is a torn tail (D-50)"
        );
    }
    let mut zeroed = journal.bytes.clone();
    zeroed.resize(journal.bytes.len() + 4096, 0);
    let recovered = journal.recover(&zeroed).unwrap();
    assert_eq!(
        (recovered.log(), recovered.truncate_to()),
        (&journal.live, Some(ByteCount::new(len(&journal.bytes)))),
        "rule: zeroed bytes after the last frame are a torn tail"
    );
}

#[test]
fn damage_inside_the_durable_prefix_is_refused() {
    let journal = Journal::new(standard());
    let [first, second, third] = [journal.starts[0], journal.starts[1], journal.starts[2]];
    assert_eq!(
        journal.recover(&flipped(&journal.bytes, first + 30)),
        Err(RecoveryError::Corruption {
            damaged_at: first,
            valid_at: second,
            sequence: 3
        }),
        "rule: a damaged payload with a valid later frame is corruption (D-60)"
    );
    assert_eq!(
        journal.recover(&flipped(&journal.bytes, second + 12)),
        Err(RecoveryError::Corruption {
            damaged_at: second,
            valid_at: third,
            sequence: 4
        }),
        "rule: a damaged header with a valid later frame is corruption (D-60)"
    );
    let mut gap = prefix(&journal.bytes, second).to_vec();
    gap.extend_from_slice(&journal.bytes[usize::try_from(third).unwrap()..]);
    assert_eq!(
        journal.recover(&gap),
        Err(RecoveryError::Corruption {
            damaged_at: second,
            valid_at: second,
            sequence: 4
        }),
        "rule: a sequence gap with a valid continuation is corruption (D-60)"
    );
    let genesis = Genesis::new(
        &journal.config,
        journal.node,
        InstanceId::from_bytes([7; 16]),
    )
    .unwrap()
    .encode();
    let mut wrong_kind = journal.bytes.clone();
    wrong_kind.extend_from_slice(&encode(FrameKind::Genesis, 5, &genesis).unwrap());
    assert_eq!(
        journal.recover(&wrong_kind),
        Err(RecoveryError::Corruption {
            damaged_at: len(&journal.bytes),
            valid_at: len(&journal.bytes),
            sequence: 5
        }),
        "rule: a valid frame of the wrong kind with a later sequence is corruption"
    );
    let end = len(&journal.bytes);
    let first_frame =
        &journal.bytes[usize::try_from(first).unwrap()..usize::try_from(second).unwrap()];
    let last_frame = &journal.bytes[usize::try_from(third).unwrap()..];
    for (copy, garbage) in [(first_frame, 0), (last_frame, 0), (last_frame, 1)] {
        let mut stale = journal.bytes.clone();
        stale.resize(stale.len() + garbage, 0xAA);
        stale.extend_from_slice(copy);
        let recovered = journal.recover(&stale).unwrap();
        assert_eq!(
            (recovered.log(), recovered.truncate_to()),
            (&journal.live, Some(ByteCount::new(end))),
            "rule: only a sequence above N proves corruption; a stale copy of a frame at              or below N, after {garbage} damaged bytes, is a torn tail (D-60)"
        );
    }
}

#[test]
fn the_resync_scan_finds_a_frame_across_every_chunk_boundary() {
    let journal = Journal::new(standard().into_iter().take(1).collect());
    let later = encode(
        FrameKind::RaftBatch,
        3,
        &batch(vec![entry(3, 1, 8)], None).encode(),
    )
    .unwrap();
    let damaged_at = len(&journal.bytes);
    let chunk = u64::from(ReadLen::MAX);
    for shift in chunk - 5..chunk + 2 {
        let valid_at = damaged_at + 1 + shift;
        let mut bytes = journal.bytes.clone();
        bytes.resize(usize::try_from(valid_at).unwrap(), 0xAA);
        bytes.extend_from_slice(&later);
        assert_eq!(
            journal.recover(&bytes),
            Err(RecoveryError::Corruption {
                damaged_at,
                valid_at,
                sequence: 3
            }),
            "rule: the resync scan checks every offset, including a frame whose magic \
             straddles a {chunk}-byte read (D-60); shift={shift}"
        );
    }
}

#[test]
fn an_intact_torn_header_skips_its_payload() {
    let inner = encode(
        FrameKind::RaftBatch,
        9,
        &batch(vec![entry(3, 1, 8)], None).encode(),
    )
    .unwrap();
    let journal = Journal::new(vec![
        batch(vec![entry(1, 1, 10)], Some(state(1, 1))),
        batch(vec![command(2, 1, inner.clone())], None),
    ]);
    let outer = journal.starts[1];
    let inner_at = outer
        + u64::try_from(
            journal.bytes[usize::try_from(outer).unwrap()..]
                .windows(inner.len())
                .position(|window| window == inner.as_slice())
                .unwrap(),
        )
        .unwrap();
    let torn = prefix(&journal.bytes, len(&journal.bytes) - 1);
    let recovered = journal.recover(torn).unwrap();
    assert_eq!(
        recovered.truncate_to(),
        Some(ByteCount::new(outer)),
        "rule: a torn frame with an intact header is skipped whole, so a valid frame \
         inside its payload is not mistaken for a later frame (D-69)"
    );
    let checksum_failing = flipped(&journal.bytes, outer + 24);
    assert_eq!(
        journal.recover(&checksum_failing).unwrap().truncate_to(),
        Some(ByteCount::new(outer)),
        "rule: a full-length frame failing its checksum keeps its intact header's length, \
         so its payload is skipped too (D-50, D-69)"
    );
    let damaged_header = flipped(torn, outer + 12);
    assert_eq!(
        journal.recover(&damaged_header),
        Err(RecoveryError::Corruption {
            damaged_at: outer,
            valid_at: inner_at,
            sequence: 9
        }),
        "rule: with a damaged header the length is untrusted and the payload is scanned: \
         the Assumption-class residual case (§7.6, §10.15)"
    );
    let mut stale = journal.bytes.clone();
    stale.extend_from_slice(&journal.bytes[usize::try_from(outer).unwrap()..]);
    let recovered = journal.recover(&stale).unwrap();
    assert_eq!(
        recovered.truncate_to(),
        Some(ByteCount::new(len(&journal.bytes))),
        "rule: a stale valid frame's intact header is trusted too, so its payload is          skipped (D-69)"
    );
}

#[test]
fn a_tail_longer_than_one_maximum_frame_is_refused() {
    let journal = Journal::new(standard());
    let end = len(&journal.bytes);
    let mut bytes = journal.bytes.clone();
    bytes.resize(usize::try_from(end + MAX_FRAME_LEN).unwrap(), 0);
    let recovered = journal.recover(&bytes).unwrap();
    assert_eq!(
        recovered.truncate_to(),
        Some(ByteCount::new(end)),
        "rule: a torn tail of exactly one maximum frame ({MAX_FRAME_LEN} bytes) is \
         discarded"
    );
    bytes.push(0);
    assert_eq!(
        journal.recover(&bytes),
        Err(RecoveryError::TailTooLong {
            start: end,
            len: MAX_FRAME_LEN + 1
        }),
        "rule: a trailing region longer than one maximum frame is outside the crash \
         model: refuse (§7.6)"
    );
}

#[test]
fn genesis_and_index_relationship_errors_are_refused() {
    let journal = Journal::new(Vec::new());
    let genesis_len = journal.genesis_len();
    let one = batch(vec![entry(1, 1, 10)], None).encode();
    let mut errors = Vec::new();
    let mut expect = |bytes: Vec<u8>, expected: RecoveryError, rule: &str| {
        let result = journal.recover(&bytes);
        assert_eq!(result, Err(expected), "rule: {rule}");
        errors.push(result.unwrap_err());
    };
    expect(
        encode(FrameKind::RaftBatch, 1, &one).unwrap(),
        RecoveryError::NotGenesis {
            kind: FrameKind::RaftBatch,
            sequence: 1,
        },
        "the first frame must be the genesis",
    );
    let genesis = Genesis::new(
        &journal.config,
        journal.node,
        InstanceId::from_bytes([7; 16]),
    )
    .unwrap()
    .encode();
    expect(
        encode(FrameKind::Genesis, 2, &genesis).unwrap(),
        RecoveryError::NotGenesis {
            kind: FrameKind::Genesis,
            sequence: 2,
        },
        "the genesis has sequence 1",
    );
    expect(
        encode(FrameKind::Genesis, 1, b"not a genesis").unwrap(),
        RecoveryError::GenesisRecord(Genesis::decode(b"not a genesis").unwrap_err()),
        "an invalid genesis record is refused",
    );
    let other = Genesis::new(
        &journal.config,
        NodeId::new(3).unwrap(),
        InstanceId::from_bytes([7; 16]),
    )
    .unwrap();
    expect(
        encode(FrameKind::Genesis, 1, &other.encode()).unwrap(),
        RecoveryError::Mismatch(GenesisMismatch::Node {
            bound: NodeId::new(3).unwrap(),
            starting: journal.node,
        }),
        "another node's journal is refused (DUR-8)",
    );
    let after = |frames: &[(u64, Vec<u8>)]| {
        let mut bytes = journal.bytes.clone();
        for (sequence, payload) in frames {
            bytes.extend_from_slice(&encode(FrameKind::RaftBatch, *sequence, payload).unwrap());
        }
        bytes
    };
    expect(
        after(&[(2, b"not a batch".to_vec())]),
        RecoveryError::Record {
            at: genesis_len,
            error: RaftBatch::decode(b"not a batch").unwrap_err(),
        },
        "a checksum-valid frame with an invalid batch is refused",
    );
    let committed = batch(vec![entry(1, 1, 10), entry(2, 1, 10)], Some(state(1, 2)));
    let second = genesis_len + len(&encode(FrameKind::RaftBatch, 2, &committed.encode()).unwrap());
    expect(
        after(&[
            (2, committed.encode()),
            (3, batch(vec![entry(2, 2, 10)], None).encode()),
        ]),
        RecoveryError::Apply {
            at: second,
            error: JournalError::TruncatesCommitted {
                first: 2,
                commit: 2,
            },
        },
        "replacing a committed entry is an impossible index relationship (DUR-6)",
    );
    expect(
        after(&[(2, batch(vec![entry(3, 1, 10)], None).encode())]),
        RecoveryError::Apply {
            at: genesis_len,
            error: JournalError::Gap {
                expected: 1,
                found: 3,
            },
        },
        "an index gap is an impossible index relationship (DUR-6)",
    );
    assert!(
        errors.iter().all(|error| !error.permits_init()),
        "rule: none of these permits init: {errors:?}"
    );
}

#[test]
fn reads_are_bounded_and_storage_failures_refuse() {
    let journal = Journal::new(vec![
        batch(vec![entry(1, 1, 10)], Some(state(1, 1))),
        batch(
            (2..=11).map(|index| entry(index, 1, 60_000)).collect(),
            None,
        ),
    ]);
    let big = journal.starts[1];
    let mut bytes = flipped(&journal.bytes, big + 12);
    bytes.resize(bytes.len() + 300 * 1024, 0);
    let memory = Memory {
        bytes,
        ..Memory::default()
    };
    let journal_len = ByteCount::new(len(&memory.bytes));
    let recovered = recover(&memory, journal_len, &journal.config, journal.node, BOUNDS).unwrap();
    assert_eq!(
        recovered.truncate_to(),
        Some(ByteCount::new(big)),
        "rule: about 900 KiB after a damaged header are scanned and found torn"
    );
    let reads = memory.reads.borrow();
    assert!(
        reads.iter().all(|&read| read <= ReadLen::MAX),
        "rule: every read is at most {} bytes (§7.11): {reads:?}",
        ReadLen::MAX
    );
    assert!(
        reads.iter().filter(|&&read| read == ReadLen::MAX).count() >= 3,
        "rule: the resync scan reads the whole remainder in bounded chunks: {reads:?}"
    );
    drop(reads);
    let failing = Memory {
        bytes: journal.bytes.clone(),
        fail: true,
        ..Memory::default()
    };
    let journal_len = ByteCount::new(len(&journal.bytes));
    assert_eq!(
        recover(&failing, journal_len, &journal.config, journal.node, BOUNDS),
        Err(RecoveryError::Storage(StorageError::Failed)),
        "rule: a failed read refuses the start"
    );
    let short = Memory {
        bytes: journal.bytes.clone(),
        short: true,
        ..Memory::default()
    };
    let result = recover(&short, journal_len, &journal.config, journal.node, BOUNDS);
    assert!(
        is_short_read(&result),
        "rule: a short read is a storage error, never a torn tail: {result:?}"
    );
}

/// Whether `result` is a short-read refusal.
fn is_short_read(result: &Result<Recovered, RecoveryError>) -> bool {
    match result {
        Err(RecoveryError::Storage(StorageError::ShortRead { .. })) => true,
        Ok(_) | Err(_) => false,
    }
}

#[test]
fn recovery_converges_and_appends_continue_the_live_log() {
    let journal = Journal::new(standard());
    let last = journal.starts[2];
    let torn = prefix(&journal.bytes, len(&journal.bytes) - 5);
    let recovered = journal.recover(torn).unwrap();
    assert_eq!(recovered.truncate_to(), Some(ByteCount::new(last)));
    let half_truncated = journal.recover(prefix(&journal.bytes, last + 3)).unwrap();
    assert_eq!(
        (half_truncated.log(), half_truncated.truncate_to()),
        (recovered.log(), recovered.truncate_to()),
        "rule: a crash part-way through the truncation recovers to the same answer"
    );
    let truncated = prefix(&journal.bytes, last);
    let again = journal.recover(truncated).unwrap();
    assert_eq!(
        (again.log(), again.truncate_to()),
        (recovered.log(), None),
        "rule: after the truncation the journal is clean"
    );
    let (_, mut log) = again.into_parts();
    let mut bytes = truncated.to_vec();
    bytes.extend_from_slice(&log.append(standard().remove(2)).unwrap());
    assert_eq!(
        bytes, journal.bytes,
        "rule: appends after recovery write exactly the frames the live path would"
    );
    let resumed = journal.recover(&bytes).unwrap();
    assert_eq!(
        (resumed.log(), resumed.truncate_to()),
        (&journal.live, None),
        "rule: the continued journal recovers to the live log"
    );
}

#[test]
fn an_impossible_history_in_the_journal_is_refused() {
    let journal = Journal::new(vec![batch(
        vec![entry(1, 1, 10), entry(2, 1, 10), entry(3, 1, 10)],
        Some(state(1, 3)),
    )]);
    let at = len(&journal.bytes);
    let with = |hard_state: HardState| {
        let mut bytes = journal.bytes.clone();
        let frame = batch(Vec::new(), Some(hard_state)).encode();
        bytes.extend_from_slice(&encode(FrameKind::RaftBatch, 3, &frame).unwrap());
        bytes
    };
    let refused = journal.recover(&with(state(1, 1))).unwrap_err();
    assert_eq!(
        refused,
        RecoveryError::Apply {
            at,
            error: JournalError::CommitRegression {
                persisted: 3,
                found: 1
            }
        },
        "rule: a commit regression in the journal is an impossible relationship: refuse \
         (br-n60a counterexample 1, §7.6)"
    );
    assert!(!refused.permits_init(), "rule: never re-initialised");
    let vote = |id: u64| HardState {
        term: Term::new(1).unwrap(),
        vote: NodeId::new(id).ok(),
        commit: LogIndex::new(3).unwrap(),
    };
    assert_eq!(
        journal.recover(&with(vote(9))),
        Err(RecoveryError::Apply {
            at,
            error: JournalError::VoteForNonVoter {
                vote: NodeId::new(9).unwrap()
            }
        }),
        "rule: a vote for a node outside the genesis voters is refused, by the same log rule \
         the live path applies (CD-10)"
    );
    assert_eq!(
        journal.recover(&with(vote(3))).unwrap().log().hard_state(),
        vote(3),
        "rule: a vote for a voter recovers"
    );
}

#[test]
fn the_recovered_log_keeps_the_hard_stop() {
    let journal = Journal::new(standard());
    let end = len(&journal.bytes);
    let next = batch(vec![entry(5, 2, 10)], None);
    let next_len = len(&encode(FrameKind::RaftBatch, 5, &next.encode()).unwrap());
    for stop in [end - 1, end + next_len - 1, end + next_len] {
        let memory = Memory {
            bytes: journal.bytes.clone(),
            ..Memory::default()
        };
        let bounds = LogBounds::new(ByteCount::new(1 << 20), ByteCount::new(stop));
        let recovered = recover(
            &memory,
            ByteCount::new(end),
            &journal.config,
            journal.node,
            bounds,
        )
        .unwrap();
        let (_, mut log) = recovered.into_parts();
        let result = log.append(next.clone());
        if stop < end + next_len {
            assert_eq!(
                result,
                Err(JournalError::HardStop {
                    end,
                    frame_len: next_len,
                    limit: stop
                }),
                "rule: a journal recovered under a {stop}-byte stop refuses the first append \
                 that would pass it, so the node re-enters failed (DM-7)"
            );
        } else {
            assert!(result.is_ok(), "rule: an append ending at the stop fits");
        }
    }
}
