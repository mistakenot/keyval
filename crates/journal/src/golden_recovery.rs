//! The recovery golden vectors (PLAN.md §7.6 "Golden vectors", DUR-5, DUR-6, DUR-9): one
//! committed journal per recovery case in `golden/recovery/`, each with its expected
//! outcome here and a README line saying what it represents.
//!
//! Every file was built field by field from §7.6 and `golden/README.md`, with checksums
//! from an independent bitwise CRC-32C, not by this crate's encoder. Most files are
//! journal **bodies**: the frames after the genesis, from sequence 2. The tests prepend
//! the genesis of node 1 under `config/local.toml`, so the files do not pin the semantic
//! fingerprint, which `genesis.bin` and the fingerprint tests own. Offsets below are
//! relative to the body. Goldens are oracles (P9): never regenerated to make a test pass.
#![cfg(test)]

use std::path::Path;

use types::{
    ByteCount, Config, Entry, EntryIndex, EntryPayload, HardState, InstanceId, JournalRead,
    LogIndex, NodeId, ReadLen, StorageError, Term,
};

use crate::{
    FrameError, FrameKind, Genesis, JournalError, LogBounds, LogicalLog, MAX_FRAME_LEN, Recovered,
    RecoveryError, encode, recover,
};

/// The repository's local cluster config.
const LOCAL: &str = include_str!("../../../config/local.toml");

/// The README, which must describe every golden.
const README: &str = include_str!("../golden/recovery/README.md");

/// The bounds in these tests: a 1 MiB entry cache and a hard stop far away.
const BOUNDS: LogBounds = LogBounds::new(ByteCount::new(1 << 20), ByteCount::new(1 << 40));

/// The length of the body's first frame: sequence 2, entries 1–2.
const F2: u64 = 113;
/// The length of the body's second frame: sequence 3, entry 3. `clean.bin` is these two.
const F3: u64 = 87;

/// A journal held in memory: the byte-storage port's read side.
struct Memory(Vec<u8>);

impl JournalRead for Memory {
    fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
        let start = usize::try_from(offset.get()).unwrap();
        Ok(self.0[start..start + len.get() as usize].to_vec())
    }
}

/// Node 1 and its configuration.
fn binding() -> (Config, NodeId) {
    (
        Config::parse(LOCAL, Path::new("config")).unwrap(),
        NodeId::new(1).unwrap(),
    )
}

/// The genesis frame the tests prepend to a body: node 1 under `config/local.toml`.
fn genesis_frame() -> Vec<u8> {
    let (config, node) = binding();
    let instance = InstanceId::parse("00112233445566778899aabbccddeeff").unwrap();
    let genesis = Genesis::new(&config, node, instance).unwrap();
    encode(FrameKind::Genesis, 1, &genesis.encode()).unwrap()
}

/// The genesis frame's length: body offsets are relative to it.
fn genesis_len() -> u64 {
    u64::try_from(genesis_frame().len()).unwrap()
}

/// Recovers a whole journal.
fn recover_journal(bytes: &[u8]) -> Result<Recovered, RecoveryError> {
    let (config, node) = binding();
    let len = ByteCount::new(u64::try_from(bytes.len()).unwrap());
    recover(&Memory(bytes.to_vec()), len, &config, node, BOUNDS)
}

/// Recovers a body after the prepended genesis.
fn recover_body(body: &[u8]) -> Result<Recovered, RecoveryError> {
    let mut journal = genesis_frame();
    journal.extend_from_slice(body);
    recover_journal(&journal)
}

/// A command entry.
fn entry(index: u64, term: u64, command: &[u8]) -> Entry {
    Entry {
        index: EntryIndex::new(index).unwrap(),
        term: Term::new(term).unwrap(),
        payload: EntryPayload::Command(command.to_vec()),
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

/// The entries and hard state `log` holds, read back through `body`'s journal.
fn contents(log: &LogicalLog, body: &[u8]) -> (Vec<Entry>, HardState) {
    let mut journal = genesis_frame();
    journal.extend_from_slice(body);
    let memory = Memory(journal);
    let mut log = log.clone();
    let entries = (1..=log.last_index().get())
        .map(|index| log.entry(EntryIndex::new(index).unwrap(), &memory).unwrap())
        .collect();
    (entries, log.hard_state())
}

/// The goldens, by file name.
const GOLDENS: [(&str, &[u8]); 18] = [
    ("empty.bin", include_bytes!("../golden/recovery/empty.bin")),
    (
        "incomplete-genesis.bin",
        include_bytes!("../golden/recovery/incomplete-genesis.bin"),
    ),
    ("clean.bin", include_bytes!("../golden/recovery/clean.bin")),
    (
        "logical-truncation.bin",
        include_bytes!("../golden/recovery/logical-truncation.bin"),
    ),
    (
        "torn-mid-header.bin",
        include_bytes!("../golden/recovery/torn-mid-header.bin"),
    ),
    (
        "torn-mid-payload.bin",
        include_bytes!("../golden/recovery/torn-mid-payload.bin"),
    ),
    (
        "torn-mid-trailer.bin",
        include_bytes!("../golden/recovery/torn-mid-trailer.bin"),
    ),
    (
        "bad-checksum-final.bin",
        include_bytes!("../golden/recovery/bad-checksum-final.bin"),
    ),
    (
        "zero-tail.bin",
        include_bytes!("../golden/recovery/zero-tail.bin"),
    ),
    (
        "garbage-tail.bin",
        include_bytes!("../golden/recovery/garbage-tail.bin"),
    ),
    (
        "crash-during-truncation.bin",
        include_bytes!("../golden/recovery/crash-during-truncation.bin"),
    ),
    (
        "bad-checksum-mid.bin",
        include_bytes!("../golden/recovery/bad-checksum-mid.bin"),
    ),
    (
        "corrupt-header-then-valid.bin",
        include_bytes!("../golden/recovery/corrupt-header-then-valid.bin"),
    ),
    (
        "sequence-gap.bin",
        include_bytes!("../golden/recovery/sequence-gap.bin"),
    ),
    (
        "tail-too-long.bin",
        include_bytes!("../golden/recovery/tail-too-long.bin"),
    ),
    (
        "truncation-below-commit.bin",
        include_bytes!("../golden/recovery/truncation-below-commit.bin"),
    ),
    (
        "impossible-version-final.bin",
        include_bytes!("../golden/recovery/impossible-version-final.bin"),
    ),
    (
        "commit-regression.bin",
        include_bytes!("../golden/recovery/commit-regression.bin"),
    ),
];

/// The golden named `name`.
fn golden(name: &str) -> &'static [u8] {
    GOLDENS
        .iter()
        .find(|(file, _)| *file == name)
        .map(|(_, bytes)| *bytes)
        .unwrap()
}

#[test]
fn every_golden_has_a_readme_line() {
    for (name, _) in GOLDENS {
        assert!(
            README.contains(&format!("| `{name}` |")),
            "rule: every golden has a README line saying what it represents (§7.6); \
             missing: {name}"
        );
    }
    let rows = README
        .lines()
        .filter(|line| line.starts_with("| `") && line.contains(".bin` |"))
        .count();
    assert_eq!(
        rows,
        GOLDENS.len(),
        "rule: the README describes exactly the goldens this test checks"
    );
}

#[test]
fn a_blank_or_partial_genesis_journal_permits_only_init() {
    for (name, expected) in [
        ("empty.bin", RecoveryError::Empty),
        ("incomplete-genesis.bin", RecoveryError::IncompleteGenesis),
    ] {
        let refused = recover_journal(golden(name)).unwrap_err();
        assert_eq!(refused, expected, "rule: {name} is refused (D-52)");
        assert!(
            refused.permits_init(),
            "rule: only explicit init may (re-)initialise {name} (§7.7 step 0)"
        );
    }
}

#[test]
fn the_clean_and_logically_truncated_journals_rebuild_their_logs() {
    let clean = golden("clean.bin");
    let recovered = recover_body(clean).unwrap();
    assert_eq!(
        contents(recovered.log(), clean),
        (
            vec![
                entry(1, 1, b"put a"),
                entry(2, 1, b"put b"),
                entry(3, 1, b"put c"),
            ],
            hard_state(1, 1, 2)
        ),
        "rule: clean.bin rebuilds entries 1-3 and the latest hard state (§7.7 step 4)"
    );
    assert_eq!(
        recovered.truncate_to(),
        None,
        "rule: clean.bin has no torn tail"
    );
    let replaced = golden("logical-truncation.bin");
    let recovered = recover_body(replaced).unwrap();
    assert_eq!(
        contents(recovered.log(), replaced),
        (
            vec![
                entry(1, 1, b"put a"),
                entry(2, 1, b"put b"),
                entry(3, 2, b"put c2"),
                entry(4, 2, b"put d"),
            ],
            hard_state(2, 3, 3)
        ),
        "rule: a batch at index 3 replaces the logical log from there; the old bytes stay"
    );
    assert_eq!(recovered.truncate_to(), None);
}

#[test]
fn every_torn_tail_golden_is_discarded() {
    let clean = recover_body(golden("clean.bin")).unwrap();
    for name in [
        "torn-mid-header.bin",
        "torn-mid-payload.bin",
        "torn-mid-trailer.bin",
        "bad-checksum-final.bin",
        "zero-tail.bin",
        "garbage-tail.bin",
    ] {
        let recovered = recover_body(golden(name)).unwrap();
        assert_eq!(
            (recovered.log(), recovered.truncate_to()),
            (clean.log(), Some(ByteCount::new(genesis_len() + F2 + F3))),
            "rule: {name} recovers clean.bin's log and truncates its tail (DUR-6, DUR-9)"
        );
    }
}

#[test]
fn a_crash_during_tail_truncation_converges() {
    let torn = recover_body(golden("torn-mid-payload.bin")).unwrap();
    let partial = recover_body(golden("crash-during-truncation.bin")).unwrap();
    assert_eq!(
        (partial.log(), partial.truncate_to()),
        (torn.log(), torn.truncate_to()),
        "rule: a partly truncated tail recovers to the same answer (D-50)"
    );
    let finished = &golden("crash-during-truncation.bin")[..usize::try_from(F2 + F3).unwrap()];
    assert_eq!(
        finished,
        golden("clean.bin"),
        "rule: finishing the truncation leaves clean.bin"
    );
}

#[test]
fn every_corruption_golden_is_refused() {
    for (name, expected) in [
        (
            "bad-checksum-mid.bin",
            RecoveryError::Corruption {
                damaged_at: 0,
                valid_at: F2,
                sequence: 3,
            },
        ),
        (
            "corrupt-header-then-valid.bin",
            RecoveryError::Corruption {
                damaged_at: F2,
                valid_at: F2 + F3,
                sequence: 4,
            },
        ),
        (
            "sequence-gap.bin",
            RecoveryError::Corruption {
                damaged_at: F2,
                valid_at: F2,
                sequence: 4,
            },
        ),
        (
            "tail-too-long.bin",
            RecoveryError::TailTooLong {
                start: F2 + F3,
                len: MAX_FRAME_LEN + 1,
            },
        ),
        (
            "truncation-below-commit.bin",
            RecoveryError::Apply {
                at: F2 + F3,
                error: JournalError::TruncatesCommitted {
                    first: 2,
                    commit: 2,
                },
            },
        ),
        (
            "impossible-version-final.bin",
            RecoveryError::ImpossibleHeader {
                at: F2 + F3,
                error: FrameError::UnsupportedVersion { version: 2 },
            },
        ),
        (
            "commit-regression.bin",
            RecoveryError::Apply {
                at: F2 + F3,
                error: JournalError::CommitRegression {
                    persisted: 2,
                    found: 1,
                },
            },
        ),
    ] {
        let refused = recover_body(golden(name)).unwrap_err();
        assert_eq!(
            relative(refused),
            expected,
            "rule: {name} is refused with its specific error (DUR-6, D-60)"
        );
    }
}

/// `error` with its offsets made relative to the body.
fn relative(error: RecoveryError) -> RecoveryError {
    let base = genesis_len();
    match error {
        RecoveryError::Corruption {
            damaged_at,
            valid_at,
            sequence,
        } => RecoveryError::Corruption {
            damaged_at: damaged_at - base,
            valid_at: valid_at - base,
            sequence,
        },
        RecoveryError::TailTooLong { start, len } => RecoveryError::TailTooLong {
            start: start - base,
            len,
        },
        RecoveryError::Apply { at, error } => RecoveryError::Apply {
            at: at - base,
            error,
        },
        RecoveryError::Record { at, error } => RecoveryError::Record {
            at: at - base,
            error,
        },
        RecoveryError::VoteForNonVoter { at, vote } => RecoveryError::VoteForNonVoter {
            at: at - base,
            vote,
        },
        RecoveryError::ImpossibleHeader { at, error } => RecoveryError::ImpossibleHeader {
            at: at - base,
            error,
        },
        RecoveryError::Empty
        | RecoveryError::IncompleteGenesis
        | RecoveryError::NotAnInterruptedInit { .. }
        | RecoveryError::NotGenesis { .. }
        | RecoveryError::GenesisRecord(_)
        | RecoveryError::Mismatch(_)
        | RecoveryError::Storage(_) => error,
    }
}
