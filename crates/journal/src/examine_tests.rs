//! The doctor core's tests (PLAN.md O-7, W23.1): `examine` reaches recovery's verdict on
//! every recovery golden, describes the torn tail, and reports what it read.
#![cfg(test)]

// check: JOURNAL-EXAMINE

use std::collections::BTreeSet;
use std::path::Path;

use types::{
    ByteCount, Config, HardState, InstanceId, JournalRead, LogIndex, NodeId, ReadLen, StorageError,
    Term,
};

use crate::{
    Binding, FrameKind, Genesis, GenesisMismatch, LogBounds, RecoveryError, TailShape, Verdict,
    encode, examine, recover,
};

/// The repository's local cluster config.
const LOCAL: &str = include_str!("../../../config/local.toml");

/// The bounds in these tests.
const BOUNDS: LogBounds = LogBounds::new(ByteCount::new(1 << 20), ByteCount::new(1 << 40));

/// The recovery goldens' body frames: F2 is 113 bytes, F3 87, F4 114 (golden/recovery).
const F2: u64 = 113;
/// The second body frame's length.
const F3: u64 = 87;
/// The third body frame's length.
const F4: u64 = 114;

/// A journal held in memory.
struct Memory(Vec<u8>);

impl JournalRead for Memory {
    fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
        let start = usize::try_from(offset.get()).unwrap().min(self.0.len());
        let end = (start + len.get() as usize).min(self.0.len());
        if end - start < len.get() as usize {
            return Err(StorageError::ShortRead {
                wanted: len,
                got: u32::try_from(end - start).unwrap(),
            });
        }
        Ok(self.0[start..end].to_vec())
    }
}

/// `config/local.toml`.
fn config() -> Config {
    Config::parse(LOCAL, Path::new("config")).unwrap()
}

/// A node ID.
fn node(id: u64) -> NodeId {
    NodeId::new(id).unwrap()
}

/// The genesis frame of `node` under `config/local.toml`.
fn genesis_frame(id: u64) -> Vec<u8> {
    let instance = InstanceId::parse("00112233445566778899aabbccddeeff").unwrap();
    let genesis = Genesis::new(&config(), node(id), instance).unwrap();
    encode(FrameKind::Genesis, 1, &genesis.encode()).unwrap()
}

/// Node 1's journal holding `body` after its genesis.
fn journal(body: &[u8]) -> Vec<u8> {
    let mut bytes = genesis_frame(1);
    bytes.extend_from_slice(body);
    bytes
}

/// The genesis frame's length.
fn genesis_len() -> u64 {
    u64::try_from(genesis_frame(1).len()).unwrap()
}

/// A byte count.
fn len(bytes: &[u8]) -> ByteCount {
    ByteCount::new(u64::try_from(bytes.len()).unwrap())
}

/// Examines `bytes` as node 1's journal, with or without its configuration.
fn examined(bytes: &[u8], bound: bool) -> crate::Examination {
    let config = config();
    let binding = bound.then_some((&config, node(1)));
    examine(&Memory(bytes.to_vec()), len(bytes), binding, BOUNDS)
}

/// The goldens' README: the index of `golden/recovery/`, one row per `.bin` file, which
/// `golden_recovery` holds to its own table both ways.
const README: &str = include_str!("../golden/recovery/README.md");

/// The recovery goldens: name, bytes, whether it is a whole journal (no genesis prepended),
/// and the torn tail's shape if recovery discards one.
const GOLDENS: [(&str, &[u8], bool, Option<TailShape>); 18] = [
    (
        "empty",
        include_bytes!("../golden/recovery/empty.bin"),
        true,
        None,
    ),
    (
        "incomplete-genesis",
        include_bytes!("../golden/recovery/incomplete-genesis.bin"),
        true,
        None,
    ),
    (
        "clean",
        include_bytes!("../golden/recovery/clean.bin"),
        false,
        None,
    ),
    (
        "logical-truncation",
        include_bytes!("../golden/recovery/logical-truncation.bin"),
        false,
        None,
    ),
    (
        "torn-mid-header",
        include_bytes!("../golden/recovery/torn-mid-header.bin"),
        false,
        Some(TailShape::Unreadable),
    ),
    (
        "torn-mid-payload",
        include_bytes!("../golden/recovery/torn-mid-payload.bin"),
        false,
        Some(TailShape::Incomplete { frame_len: F4 }),
    ),
    (
        "torn-mid-trailer",
        include_bytes!("../golden/recovery/torn-mid-trailer.bin"),
        false,
        Some(TailShape::Incomplete { frame_len: F4 }),
    ),
    (
        "bad-checksum-final",
        include_bytes!("../golden/recovery/bad-checksum-final.bin"),
        false,
        Some(TailShape::ChecksumFailed),
    ),
    (
        "zero-tail",
        include_bytes!("../golden/recovery/zero-tail.bin"),
        false,
        Some(TailShape::Unreadable),
    ),
    (
        "garbage-tail",
        include_bytes!("../golden/recovery/garbage-tail.bin"),
        false,
        Some(TailShape::Unreadable),
    ),
    (
        "crash-during-truncation",
        include_bytes!("../golden/recovery/crash-during-truncation.bin"),
        false,
        Some(TailShape::Unreadable),
    ),
    (
        "bad-checksum-mid",
        include_bytes!("../golden/recovery/bad-checksum-mid.bin"),
        false,
        None,
    ),
    (
        "corrupt-header-then-valid",
        include_bytes!("../golden/recovery/corrupt-header-then-valid.bin"),
        false,
        None,
    ),
    (
        "sequence-gap",
        include_bytes!("../golden/recovery/sequence-gap.bin"),
        false,
        None,
    ),
    (
        "tail-too-long",
        include_bytes!("../golden/recovery/tail-too-long.bin"),
        false,
        None,
    ),
    (
        "truncation-below-commit",
        include_bytes!("../golden/recovery/truncation-below-commit.bin"),
        false,
        None,
    ),
    (
        "commit-regression",
        include_bytes!("../golden/recovery/commit-regression.bin"),
        false,
        None,
    ),
    (
        "impossible-version-final",
        include_bytes!("../golden/recovery/impossible-version-final.bin"),
        false,
        None,
    ),
];

#[test]
fn the_table_holds_every_recovery_golden() {
    let rows: BTreeSet<&str> = README
        .lines()
        .filter_map(|line| {
            line.strip_prefix("| `")?
                .split_once(".bin` |")
                .map(|(name, _)| name)
        })
        .collect();
    let table: BTreeSet<&str> = GOLDENS.iter().map(|golden| golden.0).collect();
    assert_eq!(
        table, rows,
        "rule: the doctor is checked on every recovery golden the README indexes (O-7, \
         §12.2 W23); a new golden fails here until it is added (br-ozvx)"
    );
}

/// The golden journal named `name`.
fn golden(name: &str) -> Vec<u8> {
    let (_, bytes, whole, _) = GOLDENS.iter().find(|golden| golden.0 == name).unwrap();
    if *whole {
        bytes.to_vec()
    } else {
        journal(bytes)
    }
}

#[test]
fn every_recovery_golden_gets_recoverys_verdict_and_its_tail_shape() {
    let config = config();
    for (name, _, _, shape) in GOLDENS {
        let bytes = golden(name);
        let examination = examined(&bytes, true);
        let recovered = recover(
            &Memory(bytes.clone()),
            len(&bytes),
            &config,
            node(1),
            BOUNDS,
        );
        let expected = match &recovered {
            Ok(recovered) => recovered.truncate_to().map_or_else(
                || "clean".to_owned(),
                |start| format!("discard from {}", start.get()),
            ),
            Err(error) => format!("refuse: {error}"),
        };
        let observed = match examination.verdict() {
            Verdict::Clean => "clean".to_owned(),
            Verdict::DiscardTail(tail) => {
                assert_eq!(
                    Some(tail.shape()),
                    shape,
                    "rule: the doctor describes {name}'s torn tail as it is (O-7)"
                );
                assert_eq!(
                    tail.start().get() + tail.len().get(),
                    len(&bytes).get(),
                    "rule: the tail runs to the journal's end; golden={name}"
                );
                format!("discard from {}", tail.start().get())
            }
            Verdict::Refuse(error) => format!("refuse: {error}"),
        };
        assert_eq!(
            observed, expected,
            "rule: the doctor reaches exactly startup recovery's verdict (O-7, CD-10); \
             golden={name}"
        );
        if shape.is_none() {
            assert!(
                !matches_discard(examination.verdict()),
                "rule: {name} has no torn tail to describe"
            );
        }
    }
}

/// Whether `verdict` discards a tail.
const fn matches_discard(verdict: &Verdict) -> bool {
    match verdict {
        Verdict::DiscardTail(_) => true,
        Verdict::Clean | Verdict::Refuse(_) => false,
    }
}

#[test]
fn a_clean_journal_reports_what_was_read() {
    let clean = examined(&golden("clean"), true);
    assert_eq!(
        (
            clean.verdict().clone(),
            clean.binding(),
            clean.frames(),
            clean.last_index(),
            clean.last_term(),
            clean.durable_len(),
            clean.journal_len(),
        ),
        (
            Verdict::Clean,
            Binding::Verified,
            3,
            LogIndex::new(3).ok(),
            Term::new(1).ok(),
            Some(ByteCount::new(genesis_len() + F2 + F3)),
            ByteCount::new(genesis_len() + F2 + F3),
        ),
        "rule: the report counts the genesis and both batches, entries 1-3 in term 1"
    );
    assert_eq!(
        clean.hard_state(),
        Some(HardState {
            term: Term::new(1).unwrap(),
            vote: NodeId::new(1).ok(),
            commit: LogIndex::new(2).unwrap(),
        }),
        "rule: the persisted hard state, commit 2 within the last index 3"
    );
    assert_eq!(
        clean.genesis().map(Genesis::node_id),
        Some(node(1)),
        "rule: the genesis record is reported"
    );
    let replaced = examined(&golden("logical-truncation"), true);
    assert_eq!(
        (
            replaced.frames(),
            replaced.last_index(),
            replaced.last_term()
        ),
        (4, LogIndex::new(4).ok(), Term::new(2).ok()),
        "rule: a logical truncation's replacement entries are what the log holds"
    );
}

#[test]
fn the_binding_is_verified_only_with_a_configuration() {
    let bytes = golden("clean");
    let unchecked = examined(&bytes, false);
    assert_eq!(
        (unchecked.binding(), unchecked.verdict().clone()),
        (Binding::Unchecked, Verdict::Clean),
        "rule: without a configuration the doctor checks everything but the binding"
    );
    let config = config();
    let other = examine(
        &Memory(bytes.clone()),
        len(&bytes),
        Some((&config, node(2))),
        BOUNDS,
    );
    assert_eq!(
        other.verdict(),
        &Verdict::Refuse(RecoveryError::Mismatch(GenesisMismatch::Node {
            bound: node(1),
            starting: node(2),
        })),
        "rule: with a configuration, another node's directory is refused (DUR-8)"
    );
    assert_eq!(
        (other.frames(), other.genesis().map(Genesis::node_id)),
        (1, Some(node(1))),
        "rule: the refused genesis is still reported"
    );
    assert_eq!(
        (other.binding(), other.durable_len(), other.last_index()),
        (Binding::Verified, None, None),
        "rule: the genesis was checked; no logical log was built, so nothing reads as an \
         empty log (br-ljgh)"
    );
    assert_eq!(
        examined(&golden("empty"), true).binding(),
        Binding::Unchecked,
        "rule: without a genesis record nothing was verified, configuration or not (br-ljgh)"
    );
    let mut damaged = golden("clean");
    damaged.truncate(usize::try_from(genesis_len()).unwrap());
    damaged[3] ^= 0x40;
    damaged.resize(damaged.len() + 200, 0);
    assert_eq!(
        examined(&damaged, true).verdict(),
        &Verdict::Refuse(RecoveryError::NotAnInterruptedInit {
            len: len(&damaged).get(),
            genesis_len: genesis_len(),
        }),
        "rule: with the configuration, content longer than its genesis frame is no \
         interrupted init (§7.7 step 0)"
    );
    assert_eq!(
        examined(&damaged, false).verdict(),
        &Verdict::Refuse(RecoveryError::IncompleteGenesis),
        "rule: without it the genesis frame's length is unknown, and the binding is \
         reported unchecked"
    );
}

#[test]
fn a_refusal_keeps_what_was_read_before_it() {
    for (name, frames, last) in [
        ("empty", 0, None),
        ("incomplete-genesis", 0, None),
        ("bad-checksum-mid", 1, Some(0)),
        ("corrupt-header-then-valid", 2, Some(2)),
        ("sequence-gap", 2, Some(2)),
        ("truncation-below-commit", 3, Some(3)),
        ("commit-regression", 3, Some(3)),
    ] {
        let examination = examined(&golden(name), true);
        assert!(
            !matches_clean(examination.verdict()),
            "rule: {name} is refused; got {:?}",
            examination.verdict()
        );
        assert_eq!(
            (
                examination.frames(),
                examination.last_index().map(LogIndex::get)
            ),
            (frames, last),
            "rule: the report keeps the frames read before the refusal; golden={name}"
        );
    }
}

/// Whether `verdict` starts cleanly.
const fn matches_clean(verdict: &Verdict) -> bool {
    match verdict {
        Verdict::Clean => true,
        Verdict::DiscardTail(_) | Verdict::Refuse(_) => false,
    }
}

#[test]
fn the_report_says_what_recovery_would_discard() {
    let torn = examined(&golden("torn-mid-payload"), true);
    let Verdict::DiscardTail(tail) = torn.verdict() else {
        panic!(
            "rule: a torn final frame is discarded; got {:?}",
            torn.verdict()
        );
    };
    assert_eq!(
        tail.to_string(),
        format!(
            "discard the 44-byte incomplete final frame (of {F4} bytes) at offset {}",
            genesis_len() + F2 + F3
        ),
        "rule: the doctor says what startup recovery would do (O-7)"
    );
    let mut stale = golden("clean");
    let body = include_bytes!("../golden/recovery/clean.bin");
    stale.extend_from_slice(&body[..usize::try_from(F2).unwrap()]);
    let examination = examined(&stale, true);
    let Verdict::DiscardTail(tail) = examination.verdict() else {
        panic!(
            "rule: a stale copy is a torn tail; got {:?}",
            examination.verdict()
        );
    };
    assert_eq!(
        tail.to_string(),
        format!(
            "discard the {F2}-byte stale copy of frame 2 at offset {}",
            genesis_len() + F2 + F3
        ),
        "rule: a stale copy of an earlier frame is named as one"
    );
}
