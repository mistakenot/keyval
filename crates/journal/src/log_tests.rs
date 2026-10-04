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
    FrameKind, Genesis, GenesisMismatch, JournalError, LogBounds, LogicalLog, RaftBatch, decode,
    decode_header, encode,
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

/// The genesis voters in these tests: nodes 1 to 5.
fn voters() -> Vec<NodeId> {
    (1..=5).map(|id| NodeId::new(id).unwrap()).collect()
}

/// Bounds of a `cache`-byte entry cache and a `stop`-byte journal hard stop.
fn bounds(cache: u64, stop: u64) -> LogBounds {
    LogBounds::new(ByteCount::new(cache), ByteCount::new(stop))
}

/// A logical log after a 100-byte genesis, and its journal, with a cache of `cache` bytes.
fn fresh(cache: u64) -> (LogicalLog, Memory) {
    let memory = Memory {
        bytes: vec![0; 100],
        ..Memory::default()
    };
    (
        LogicalLog::after_genesis(100, &voters(), bounds(cache, 1 << 40)),
        memory,
    )
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
        batch(vec![entry(1, 1, 3), entry(2, 1, 3)], Some(state(1, 0))),
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
        batch(
            vec![entry(2, 2, 5), entry(3, 2, 5), entry(4, 2, 5)],
            Some(state(2, 1)),
        ),
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
    append(
        &mut log,
        &mut memory,
        batch(vec![entry(3, 3, 1)], Some(state(3, 1))),
    );
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
    // Each refusal runs on its own copy, which is returned so the test can show the
    // refusal left it exactly as it was (§7.6, br-biok).
    let apply = |offset: u64, kind: FrameKind, sequence: u64, batch: RaftBatch| {
        let frame = encode(kind, sequence, &batch.encode()).unwrap();
        let mut copy = log.clone();
        let result = copy.apply(offset, decode_header(&frame).unwrap(), batch);
        (result, copy)
    };
    let raft = FrameKind::RaftBatch;
    let cases = [
        (
            apply(
                end,
                FrameKind::Genesis,
                next,
                batch(vec![entry(3, 1, 1)], None),
            ),
            JournalError::NotRaftBatch { offset: end },
        ),
        (
            apply(end, raft, next + 1, batch(vec![entry(3, 1, 1)], None)),
            JournalError::Sequence {
                expected: next,
                found: next + 1,
            },
        ),
        (
            apply(end + 1, raft, next, batch(vec![entry(3, 1, 1)], None)),
            JournalError::Offset {
                expected: end,
                found: end + 1,
            },
        ),
        (
            apply(end, raft, next, batch(vec![entry(4, 1, 1)], None)),
            JournalError::Gap {
                expected: 3,
                found: 4,
            },
        ),
        (
            apply(end, raft, next, batch(vec![entry(2, 2, 1)], None)),
            JournalError::TruncatesCommitted {
                first: 2,
                commit: 2,
            },
        ),
        (
            apply(
                end,
                raft,
                next,
                batch(vec![entry(3, 1, 1)], Some(state(1, 4))),
            ),
            JournalError::CommitBeyondLog { commit: 4, last: 3 },
        ),
    ];
    for ((result, after), expected) in cases {
        assert_eq!(result, Err(expected.clone()), "rule: {expected} is refused");
        assert_eq!(
            after, before,
            "rule: refusing {expected} changes nothing, not even entries it would have \
             replaced (§7.6, br-biok)"
        );
    }
    let mut refusing = log.clone();
    assert_eq!(
        refusing.append(batch(vec![entry(1, 2, 1)], None)),
        Err(JournalError::TruncatesCommitted {
            first: 1,
            commit: 2
        })
    );
    assert_eq!(
        refusing, before,
        "rule: a refused frame changes nothing (§7.6)"
    );
}

#[test]
fn a_live_append_voting_for_a_non_voter_is_refused_before_anything_changes() {
    let (mut log, mut memory) = fresh(1 << 20);
    append(
        &mut log,
        &mut memory,
        batch(vec![entry(1, 1, 1)], Some(state(1, 0))),
    );
    let before = log.clone();
    let stray = HardState {
        vote: NodeId::new(9).ok(),
        ..state(1, 0)
    };
    assert_eq!(
        log.append(batch(vec![entry(2, 1, 1)], Some(stray))),
        Err(JournalError::VoteForNonVoter {
            vote: NodeId::new(9).unwrap()
        }),
        "rule: the live path refuses the vote recovery would refuse, so a node never writes \
         a journal it cannot restart from (CD-10, br-biok)"
    );
    assert_eq!(log, before, "rule: nothing changed");
}

#[test]
fn eviction_never_changes_results() {
    // CD-10, §7.6: the same appends against a roomy cache and a tiny one read back the
    // same entries; the tiny one reads through the port and never exceeds its bound.
    let (mut roomy, mut memory) = fresh(1 << 20);
    let (mut tiny, _) = fresh(100);
    for index in 1..=40_u64 {
        let term = 1 + index / 10;
        let next = batch(
            vec![entry(index, term, usize::try_from(index).unwrap())],
            Some(state(term, 0)),
        );
        let frame = roomy.append(next.clone()).unwrap();
        memory.bytes.extend_from_slice(&frame);
        tiny.append(next).unwrap();
        assert!(
            tiny.cached_bytes() <= 100,
            "rule: the cache stays within its bound (CD-9)"
        );
    }
    let truncating = batch(vec![entry(30, 9, 7)], Some(state(9, 0)));
    append(&mut roomy, &mut memory, truncating.clone());
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
    append(&mut log, &mut memory, batch(large, Some(state(1, 0))));
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
    append(
        &mut log,
        &mut memory,
        batch(vec![entry(1, 1, 4)], Some(state(1, 0))),
    );
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
        batch(vec![entry(1, 1, 1), entry(2, 1, 1)], Some(state(1, 0))),
    );
    append(
        &mut log,
        &mut memory,
        batch(vec![entry(2, 2, 500)], Some(state(2, 0))),
    );
    assert_eq!(
        log.entry(EntryIndex::new(2).unwrap(), &memory),
        Ok(entry(2, 2, 500)),
        "rule: after logical truncation the replacing entry is read, never the replaced one"
    );
}

/// A hard state in `term` with `vote`, committing `commit`.
fn voted(term: u64, vote: Option<u64>, commit: u64) -> HardState {
    HardState {
        term: Term::new(term).unwrap(),
        vote: vote.map(|id| NodeId::new(id).unwrap()),
        commit: LogIndex::new(commit).unwrap(),
    }
}

/// Appends `batches` to a fresh log, then asserts that appending `refused` fails with
/// `expected` and changes nothing.
fn assert_refused(
    batches: Vec<RaftBatch>,
    refused: RaftBatch,
    expected: &JournalError,
    rule: &str,
) {
    let (mut log, mut memory) = fresh(1 << 20);
    for batch in batches {
        append(&mut log, &mut memory, batch);
    }
    let before = log.clone();
    assert_eq!(log.append(refused), Err(expected.clone()), "rule: {rule}");
    assert_eq!(log, before, "rule: a refused frame changes nothing; {rule}");
}

#[test]
fn impossible_hard_states_are_refused_and_change_nothing() {
    // br-n60a's counterexample 1: entries 1-3 committed, then a commit regression, then a
    // replacement from index 2 that the regression would have let through.
    let committed = || {
        vec![batch(
            vec![entry(1, 1, 1), entry(2, 1, 1), entry(3, 1, 1)],
            Some(state(1, 3)),
        )]
    };
    assert_refused(
        committed(),
        batch(Vec::new(), Some(state(1, 1))),
        &JournalError::CommitRegression {
            persisted: 3,
            found: 1,
        },
        "a persisted commit never goes back (§7.6)",
    );
    assert_refused(
        committed(),
        batch(vec![entry(2, 2, 1)], Some(state(2, 1))),
        &JournalError::TruncatesCommitted {
            first: 2,
            commit: 3,
        },
        "committed entries are never replaced (§7.6)",
    );
    // Counterexample 2: a second vote in a term, and a term regression.
    let term_five = || vec![batch(Vec::new(), Some(voted(5, Some(2), 0)))];
    assert_refused(
        term_five(),
        batch(Vec::new(), Some(voted(5, Some(3), 0))),
        &JournalError::VoteChanged {
            term: Term::new(5).unwrap(),
            persisted: NodeId::new(2).unwrap(),
            found: NodeId::new(3).ok(),
        },
        "a vote never changes within its term (DUR-2)",
    );
    assert_refused(
        term_five(),
        batch(Vec::new(), Some(voted(5, None, 0))),
        &JournalError::VoteChanged {
            term: Term::new(5).unwrap(),
            persisted: NodeId::new(2).unwrap(),
            found: None,
        },
        "a vote is never withdrawn within its term (DUR-2)",
    );
    assert_refused(
        term_five(),
        batch(Vec::new(), Some(voted(3, Some(1), 0))),
        &JournalError::TermRegression {
            persisted: Term::new(5).unwrap(),
            found: Term::new(3).unwrap(),
        },
        "a persisted term never goes back (DUR-2)",
    );
}

#[test]
fn impossible_entry_terms_are_refused_and_change_nothing() {
    // Entry terms: never 0, never decreasing, never ahead of the term in force.
    assert_refused(
        Vec::new(),
        batch(vec![entry(1, 0, 1)], Some(state(1, 0))),
        &JournalError::ZeroTerm { index: 1 },
        "only the dummy index 0 has term 0",
    );
    assert_refused(
        Vec::new(),
        batch(vec![entry(1, 2, 1), entry(2, 1, 1)], Some(state(2, 0))),
        &JournalError::TermDecreases {
            index: 2,
            term: Term::new(1).unwrap(),
            previous: Term::new(2).unwrap(),
        },
        "entry terms never decrease within a batch",
    );
    assert_refused(
        vec![batch(vec![entry(1, 2, 1)], Some(state(2, 0)))],
        batch(vec![entry(2, 1, 1)], None),
        &JournalError::TermDecreases {
            index: 2,
            term: Term::new(1).unwrap(),
            previous: Term::new(2).unwrap(),
        },
        "entry terms never decrease from the kept entry before the batch",
    );
    // Counterexample 3: terms ahead of the persisted term.
    assert_refused(
        vec![batch(Vec::new(), Some(state(2, 0)))],
        batch(vec![entry(1, 9, 1), entry(2, 4, 1)], None),
        &JournalError::TermAhead {
            index: 1,
            term: Term::new(9).unwrap(),
            in_force: Term::new(2).unwrap(),
        },
        "an entry's term is at most the persisted term",
    );
    assert_refused(
        Vec::new(),
        batch(vec![entry(1, 3, 1)], Some(state(2, 0))),
        &JournalError::TermAhead {
            index: 1,
            term: Term::new(3).unwrap(),
            in_force: Term::new(2).unwrap(),
        },
        "an entry's term is at most its batch's hard-state term",
    );
    // The possible histories next to them are accepted.
    let (mut log, mut memory) = fresh(1 << 20);
    for state in [
        voted(4, None, 0),
        voted(4, Some(1), 0),
        voted(5, Some(3), 0),
    ] {
        append(&mut log, &mut memory, batch(Vec::new(), Some(state)));
    }
    append(
        &mut log,
        &mut memory,
        batch(vec![entry(1, 2, 1), entry(2, 5, 1)], None),
    );
    assert_eq!(
        (log.hard_state(), log.last_index().get()),
        (voted(5, Some(3), 0), 2),
        "rule: a first vote in a term, a new vote in a new term, and entries up to the \
         term in force are possible"
    );
}

#[test]
fn the_hard_stop_refuses_before_changing_anything() {
    let config = Config::parse(LOCAL, Path::new("config")).unwrap();
    assert_eq!(
        LogBounds::of(&config),
        bounds(8 << 20, 4 << 30),
        "rule: production bounds are the fixed entry-cache bound and max_journal_bytes \
         (DM-7, §7.11)"
    );
    let first = batch(vec![entry(1, 1, 10)], Some(state(1, 0)));
    let second = batch(vec![entry(2, 1, 20)], None);
    let frame_len = |sequence: u64, batch: &RaftBatch| {
        u64::try_from(
            encode(FrameKind::RaftBatch, sequence, &batch.encode())
                .unwrap()
                .len(),
        )
        .unwrap()
    };
    let full = 100 + frame_len(2, &first) + frame_len(3, &second);
    for stop in [full - 1, full, full + 1] {
        let mut log = LogicalLog::after_genesis(100, &voters(), bounds(1 << 20, stop));
        log.append(first.clone()).unwrap();
        let before = log.clone();
        let result = log.append(second.clone());
        if stop < full {
            assert_eq!(
                result,
                Err(JournalError::HardStop {
                    end: before.end(),
                    frame_len: frame_len(3, &second),
                    limit: stop
                }),
                "rule: a frame that would end past the stop ({stop}) is refused (DM-7)"
            );
            assert_eq!(log, before, "rule: the refusal comes before any change");
        } else {
            assert!(
                result.is_ok(),
                "rule: a frame ending at or before the stop ({stop}) is appended"
            );
        }
    }
    let mut log = LogicalLog::after_genesis(100, &voters(), bounds(1 << 20, full));
    log.append(first).unwrap();
    log.append(second).unwrap();
    let third = batch(Vec::new(), Some(state(1, 1)));
    assert_eq!(
        log.append(third.clone()),
        Err(JournalError::HardStop {
            end: full,
            frame_len: frame_len(4, &third),
            limit: full
        }),
        "rule: at the stop every further append is refused: the node stays failed (DM-7)"
    );
}

#[test]
fn the_cache_bound_holds_at_n_minus_one_n_and_n_plus_one() {
    // Three entries of 100 bytes each (32 fixed + 68 command bytes).
    for (limit, cached) in [(299, 200), (300, 300), (301, 300), (99, 0), (100, 100)] {
        let (mut log, mut memory) = fresh(limit);
        append(
            &mut log,
            &mut memory,
            batch(
                (1..=3).map(|index| entry(index, 1, 68)).collect(),
                Some(state(1, 0)),
            ),
        );
        assert_eq!(
            log.cached_bytes(),
            cached,
            "rule: a {limit}-byte cache holds {cached} bytes of three 100-byte entries (CD-9)"
        );
        for index in 1..=3 {
            assert_eq!(
                log.entry(EntryIndex::new(index).unwrap(), &memory).unwrap(),
                entry(index, 1, 68),
                "rule: eviction never changes results; limit={limit} index={index}"
            );
        }
    }
}

#[test]
fn the_journal_only_grows_and_is_never_compacted() {
    // DUR-4: no log compaction in v1. Every frame is appended at the end, no earlier byte
    // is rewritten or dropped, a logical truncation leaves the replaced frame in place, and
    // the earliest entry is still read from its original frame. Growth stops only at the
    // DM-7 hard stop (the_hard_stop_refuses_before_changing_anything).
    let (mut log, mut memory) = fresh(0);
    let mut ends = vec![log.end()];
    for next in [
        batch(vec![entry(1, 1, 8), entry(2, 1, 8)], Some(state(1, 1))),
        batch(vec![entry(3, 1, 8)], Some(state(1, 2))),
        batch(vec![entry(3, 2, 9), entry(4, 2, 9)], Some(state(2, 2))),
        batch(Vec::new(), Some(state(2, 4))),
        batch(vec![entry(5, 2, 8)], None),
    ] {
        let before = memory.bytes.clone();
        let end = log.end();
        let frame = log.append(next).unwrap();
        assert_eq!(
            log.end(),
            end + u64::try_from(frame.len()).unwrap(),
            "rule: every frame is appended at the journal's end (DUR-4)"
        );
        memory.bytes.extend_from_slice(&frame);
        assert!(
            memory.bytes.starts_with(&before),
            "rule: no earlier byte is ever rewritten (DUR-4)"
        );
        ends.push(log.end());
    }
    assert!(
        ends.windows(2).all(|pair| pair[0] < pair[1]),
        "rule: the journal only grows: {ends:?}"
    );
    let replaced = decode(&memory.bytes[usize::try_from(ends[1]).unwrap()..]).unwrap();
    assert_eq!(
        RaftBatch::decode(replaced.payload()).unwrap().entries(),
        &[entry(3, 1, 8)],
        "rule: a logical truncation leaves the replaced frame in the file (§7.6, DUR-4)"
    );
    assert_eq!(
        log.term(EntryIndex::new(3).unwrap()),
        Some(Term::new(2).unwrap()),
        "rule: the logical log reads the replacement"
    );
    assert_eq!(
        log.entry(EntryIndex::new(1).unwrap(), &memory),
        Ok(entry(1, 1, 8)),
        "rule: the earliest entry is read from its original frame: nothing is compacted"
    );
}
