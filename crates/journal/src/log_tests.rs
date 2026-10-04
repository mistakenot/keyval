//! The logical log's tests (PLAN.md §7.6, §7.7 step 2, W11.2).
#![cfg(test)]

// check: JOURNAL-LOG

use std::cell::RefCell;
use std::path::Path;

use types::{
    ByteCount, Config, Entry, EntryIndex, EntryPayload, HardState, InstanceId, JournalRead,
    LogIndex, NodeId, ReadLen, StorageError, Term,
};

use crate::{
    FrameKind, Genesis, GenesisMismatch, JournalError, LogicalLog, RaftBatch, decode, encode,
};

/// The repository's local cluster config.
const LOCAL: &str = include_str!("../../../config/local.toml");

/// A journal held in memory: the byte-storage port's read side, recording each read's
/// length.
#[derive(Default)]
struct Memory {
    /// The journal's bytes.
    bytes: Vec<u8>,
    /// Each read's length, in order.
    reads: RefCell<Vec<u32>>,
    /// Whether reads return one byte too few.
    short: bool,
}

impl JournalRead for Memory {
    fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
        self.reads.borrow_mut().push(len.get());
        let start = usize::try_from(offset.get()).unwrap();
        let end = start + len.get() as usize - usize::from(self.short);
        Ok(self.bytes[start..end].to_vec())
    }
}

/// An entry at `index` in `term` carrying `bytes` of command.
fn entry(index: u64, term: u64, bytes: usize) -> Entry {
    Entry {
        index: EntryIndex::new(index).unwrap(),
        term: Term::new(term).unwrap(),
        payload: EntryPayload::Command(vec![u8::try_from(index % 251).unwrap(); bytes]),
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

/// A logical log after a 100-byte genesis, and its journal, with a cache of `cache` bytes.
fn fresh(cache: u64) -> (LogicalLog, Memory) {
    let memory = Memory {
        bytes: vec![0; 100],
        ..Memory::default()
    };
    (LogicalLog::after_genesis(100, cache), memory)
}

/// Appends `batch` to `log` and writes its frame to `memory`.
fn append(log: &mut LogicalLog, memory: &mut Memory, batch: RaftBatch) {
    let frame = log.append(batch).unwrap();
    memory.bytes.extend_from_slice(&frame);
    assert_eq!(
        u64::try_from(memory.bytes.len()).unwrap(),
        log.end(),
        "rule: the log's end is the journal's length"
    );
}

#[test]
fn appends_assign_consecutive_sequences_and_offsets() {
    let (mut log, mut memory) = fresh(1 << 20);
    append(
        &mut log,
        &mut memory,
        batch(vec![entry(1, 1, 3), entry(2, 1, 3)], None),
    );
    append(
        &mut log,
        &mut memory,
        batch(vec![entry(3, 1, 3)], Some(state(1, 2))),
    );
    assert_eq!(log.next_sequence(), 4, "rule: genesis is 1, then 2 and 3");
    assert_eq!(log.last_index().get(), 3);
    assert_eq!(
        log.hard_state(),
        state(1, 2),
        "rule: the persisted hard state is kept"
    );
    let second = decode(&memory.bytes[100..]).unwrap();
    let third_at = 100 + second.header().frame_len();
    let third = decode(&memory.bytes[third_at..]).unwrap();
    assert_eq!(
        (
            second.header().sequence(),
            third.header().sequence(),
            third.header().kind()
        ),
        (2, 3, FrameKind::RaftBatch),
        "rule: frames are written with strictly increasing sequences (§7.6)"
    );
}

#[test]
fn a_batch_at_or_below_the_last_index_replaces_the_suffix() {
    let (mut log, mut memory) = fresh(1 << 20);
    append(
        &mut log,
        &mut memory,
        batch(
            vec![entry(1, 1, 3), entry(2, 1, 3), entry(3, 1, 3)],
            Some(state(1, 1)),
        ),
    );
    append(
        &mut log,
        &mut memory,
        batch(vec![entry(2, 2, 5), entry(3, 2, 5), entry(4, 2, 5)], None),
    );
    assert_eq!(
        log.last_index().get(),
        4,
        "rule: logical truncation then append (§7.6)"
    );
    for (index, term) in [(1, 1), (2, 2), (3, 2), (4, 2)] {
        let at = EntryIndex::new(index).unwrap();
        assert_eq!(
            log.term(at),
            Some(Term::new(term).unwrap()),
            "rule: index {index}"
        );
        assert_eq!(
            log.entry(at, &memory).unwrap(),
            entry(index, term, if index == 1 { 3 } else { 5 }),
            "rule: the replacing entries are the log's from the truncation point"
        );
    }
    append(&mut log, &mut memory, batch(vec![entry(3, 3, 1)], None));
    assert_eq!(
        log.last_index().get(),
        3,
        "rule: a shorter replacement shortens the log"
    );
    assert_eq!(log.term(EntryIndex::new(4).unwrap()), None);
}

#[test]
fn every_refusal_leaves_the_log_unchanged() {
    let (mut log, mut memory) = fresh(1 << 20);
    append(
        &mut log,
        &mut memory,
        batch(vec![entry(1, 1, 1), entry(2, 1, 1)], Some(state(1, 2))),
    );
    let before = log.clone();
    let end = log.end();
    let next = log.next_sequence();
    let cases = [
        (
            log.clone()
                .apply(end, next + 1, 40, batch(vec![entry(3, 1, 1)], None)),
            JournalError::Sequence {
                expected: next,
                found: next + 1,
            },
        ),
        (
            log.clone()
                .apply(end + 1, next, 40, batch(vec![entry(3, 1, 1)], None)),
            JournalError::Offset {
                expected: end,
                found: end + 1,
            },
        ),
        (
            log.clone()
                .apply(end, next, 40, batch(vec![entry(4, 1, 1)], None)),
            JournalError::Gap {
                expected: 3,
                found: 4,
            },
        ),
        (
            log.clone()
                .apply(end, next, 40, batch(vec![entry(2, 2, 1)], None)),
            JournalError::TruncatesCommitted {
                first: 2,
                commit: 2,
            },
        ),
        (
            log.clone().apply(
                end,
                next,
                40,
                batch(vec![entry(3, 1, 1)], Some(state(1, 4))),
            ),
            JournalError::CommitBeyondLog { commit: 4, last: 3 },
        ),
    ];
    for (result, expected) in cases {
        assert_eq!(result, Err(expected.clone()), "rule: {expected} is refused");
    }
    let mut refusing = log.clone();
    assert!(refusing.append(batch(vec![entry(1, 2, 1)], None)).is_err());
    assert_eq!(
        refusing, before,
        "rule: a refused frame changes nothing (§7.6)"
    );
}

#[test]
fn eviction_never_changes_results() {
    // CD-10, §7.6: the same appends against a roomy cache and a tiny one read back the
    // same entries; the tiny one reads through the port and never exceeds its bound.
    let (mut roomy, mut memory) = fresh(1 << 20);
    let (mut tiny, _) = fresh(100);
    for index in 1..=40_u64 {
        let next = batch(
            vec![entry(
                index,
                1 + index / 10,
                usize::try_from(index).unwrap(),
            )],
            None,
        );
        let frame = roomy.append(next.clone()).unwrap();
        memory.bytes.extend_from_slice(&frame);
        tiny.append(next).unwrap();
        assert!(
            tiny.cached_bytes() <= 100,
            "rule: the cache stays within its bound (CD-9)"
        );
    }
    append(&mut roomy, &mut memory, batch(vec![entry(30, 9, 7)], None));
    let truncating = batch(vec![entry(30, 9, 7)], None);
    tiny.append(truncating).unwrap();
    for index in 1..=30_u64 {
        let at = EntryIndex::new(index).unwrap();
        assert_eq!(
            tiny.entry(at, &memory),
            roomy.entry(at, &memory),
            "rule: eviction changes cost, never results (§7.6); index={index}"
        );
    }
    assert!(
        !memory.reads.borrow().is_empty(),
        "rule: the tiny cache read through the port"
    );
}

#[test]
fn a_miss_reads_its_frame_in_chunks_of_at_most_256_kib() {
    let (mut log, mut memory) = fresh(0);
    let large: Vec<Entry> = (1..=15).map(|index| entry(index, 1, 65_000)).collect();
    append(&mut log, &mut memory, batch(large, None));
    let read = log.entry(EntryIndex::new(15).unwrap(), &memory).unwrap();
    assert_eq!(read, entry(15, 1, 65_000));
    let reads = memory.reads.borrow();
    assert!(
        reads.len() >= 5,
        "rule: a ~1 MiB frame takes several reads; reads={reads:?}"
    );
    assert!(
        reads.iter().all(|len| *len <= ReadLen::MAX),
        "rule: every read is at most 256 KiB (§7.11); reads={reads:?}"
    );
}

#[test]
fn reads_report_storage_and_index_failures() {
    let (mut log, mut memory) = fresh(0);
    append(&mut log, &mut memory, batch(vec![entry(1, 1, 4)], None));
    assert_eq!(
        log.entry(EntryIndex::new(2).unwrap(), &memory),
        Err(JournalError::NotInLog { index: 2 }),
        "rule: past the last index is not in the log"
    );
    memory.short = true;
    assert_eq!(
        log.entry(EntryIndex::new(1).unwrap(), &memory),
        Err(JournalError::Storage(StorageError::ShortRead {
            wanted: ReadLen::new(24).unwrap(),
            got: 23
        })),
        "rule: a short read is a storage error, not a guess"
    );
    memory.short = false;
    memory.bytes[100 + 30] ^= 1;
    assert!(
        matches_frame_error(&log.clone().entry(EntryIndex::new(1).unwrap(), &memory)),
        "rule: damaged bytes read back are refused (§7.6)"
    );
}

/// Whether `result` is a voter-set mismatch.
fn is_voters_mismatch(result: &Result<(), GenesisMismatch>) -> bool {
    match result {
        Err(GenesisMismatch::Voters { bound, configured }) => {
            bound.len() == 5 && configured.len() == 3
        }
        Ok(()) | Err(_) => false,
    }
}

/// Whether `result` is a frame error (a damaged read-back).
fn matches_frame_error(result: &Result<Entry, JournalError>) -> bool {
    match result {
        Err(JournalError::Frame(_)) => true,
        Ok(_) | Err(_) => false,
    }
}

#[test]
fn genesis_verification_refuses_a_conflicting_start() {
    let config = Config::parse(LOCAL, Path::new("config")).unwrap();
    let node = NodeId::new(2).unwrap();
    let genesis = Genesis::new(&config, node, InstanceId::from_bytes([1; 16])).unwrap();
    assert_eq!(
        genesis.verify(&config, node),
        Ok(()),
        "rule: the bound config passes (DUR-8)"
    );
    assert_eq!(
        genesis.verify(&config, NodeId::new(3).unwrap()),
        Err(GenesisMismatch::Node {
            bound: node,
            starting: NodeId::new(3).unwrap()
        }),
        "rule: another node's data directory is refused (§7.7 step 2)"
    );
    let three = LOCAL.split("\n[[nodes]]\nid = 4").next().unwrap();
    let three = Config::parse(three, Path::new("config")).unwrap();
    assert!(
        is_voters_mismatch(&genesis.verify(&three, node)),
        "rule: a different voter set is refused (DUR-8)"
    );
    let smaller = LOCAL.replace("max_live_keys = 10000", "max_live_keys = 9999");
    let smaller = Config::parse(&smaller, Path::new("config")).unwrap();
    assert_eq!(
        genesis.verify(&smaller, node),
        Err(GenesisMismatch::Fingerprint {
            bound: config.semantic_fingerprint(),
            configured: smaller.semantic_fingerprint()
        }),
        "rule: a changed semantic configuration is refused (DUR-8)"
    );
    let encoded = encode(FrameKind::Genesis, 1, &genesis.encode()).unwrap();
    assert_eq!(
        Genesis::decode(decode(&encoded).unwrap().payload())
            .unwrap()
            .verify(&config, node),
        Ok(()),
        "rule: a genesis read back verifies like the one written"
    );
}

#[test]
fn a_replaced_entry_is_never_served_from_a_stale_cache() {
    // A small entry is cached; its replacement is too heavy to cache. Truncation must
    // drop the stale copy, so the read goes to the journal (§7.6, CD-10).
    let (mut log, mut memory) = fresh(100);
    append(
        &mut log,
        &mut memory,
        batch(vec![entry(1, 1, 1), entry(2, 1, 1)], None),
    );
    append(&mut log, &mut memory, batch(vec![entry(2, 2, 500)], None));
    assert_eq!(
        log.entry(EntryIndex::new(2).unwrap(), &memory),
        Ok(entry(2, 2, 500)),
        "rule: after logical truncation the replacing entry is read, never the replaced one"
    );
}
