//! The byte-storage contract suite (PLAN.md §7.2, W12): the ONE suite every
//! [`types::ByteStorage`] adapter must pass unmodified, so that what simulation proves
//! holds for production (P3, T-2).
//!
//! It is test-only source, never compiled into a production build (owner decision on
//! br-w12-1-contract-suite-v3e). An adapter crate includes this file in a test target,
//! mounted at the crate root as `storage_contract`, and gives every case of
//! [`all_cases`] its own named `#[test]`, so the registry gate can count them:
//!
//! ```ignore
//! #[path = "../../types/tests/storage_contract/mod.rs"]
//! mod storage_contract;
//!
//! mod contract_my_adapter {
//!     use crate::storage_contract::{self as contract, run};
//!
//!     #[test]
//!     fn a_blank_directory_has_no_journal() {
//!         run("a_blank_directory_has_no_journal", contract::a_blank_directory_has_no_journal, crate::MyHarness::default());
//!     }
//!     // … one per case, and a test that its list equals `all_cases` (see
//!     // `assert_every_case_runs`).
//! }
//! ```
//!
//! Each case returns a [`Violation`] naming the rule it found broken, so a broken adapter
//! is diagnosable from the failure alone. Crash behaviour (which unsynced bytes survive,
//! directory entries lost before their sync) differs by adapter and is tested by each:
//! the simulated disk in W12.2, the crash probes in W24.
#![cfg(test)]

use core::fmt;

use types::{
    ByteCount, ByteStorage, DataDir, Effect, Executed, Incarnation, Input, JournalRead, NodeId,
    ReadLen, StorageError, StorageOp, SyncSeq, execute,
};

/// How the suite reaches an adapter.
pub trait Harness {
    /// A data directory the adapter can open.
    type Dir;
    /// One process's handle on a data directory.
    type Storage: ByteStorage;
    /// A new, empty data directory.
    fn new_dir(&mut self) -> Self::Dir;
    /// Opens `dir` as a new process would. Dropping the handle ends that process cleanly.
    fn open(&mut self, dir: &Self::Dir) -> Self::Storage;
}

/// A broken contract rule, with what was observed.
#[derive(Debug, PartialEq, Eq)]
pub struct Violation {
    /// The rule, as the suite states it.
    pub rule: &'static str,
    /// What the adapter did instead.
    pub detail: String,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "byte-storage contract broken: {}; {} (re-run: ./dev check STORAGE-CONTRACT)",
            self.rule, self.detail
        )
    }
}

/// One case's result.
pub type Outcome = Result<(), Violation>;

/// One case of the suite.
pub type Case<H> = fn(&mut H) -> Outcome;

/// Checks that `observed` is `expected` under `rule`.
fn expect<T: PartialEq + fmt::Debug>(rule: &'static str, observed: &T, expected: &T) -> Outcome {
    if observed == expected {
        Ok(())
    } else {
        Err(Violation {
            rule,
            detail: format!("observed {observed:?}, expected {expected:?}"),
        })
    }
}

/// A byte offset or length.
fn bytes(len: usize) -> ByteCount {
    ByteCount::new(u64::try_from(len).unwrap())
}

/// A read length; the cases keep it within 1 to `ReadLen::MAX`.
fn read_len(len: usize) -> ReadLen {
    ReadLen::new(u32::try_from(len).unwrap()).unwrap()
}

/// The journal's whole contents, read back in chunks of at most `ReadLen::MAX`.
fn read_all(storage: &impl ByteStorage) -> Result<Vec<u8>, StorageError> {
    let len = usize::try_from(storage.journal_len()?.get()).unwrap();
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let chunk = (len - out.len()).min(ReadLen::MAX as usize);
        out.extend(storage.read_at(bytes(out.len()), read_len(chunk))?);
    }
    Ok(out)
}

/// `len` bytes of a pattern seeded by `seed`, so misplaced bytes never compare equal.
fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|at| seed.wrapping_add(u8::try_from(at % 251).unwrap()))
        .collect()
}

/// A fresh directory, opened, with an empty journal.
fn with_journal<H: Harness>(harness: &mut H) -> Result<(H::Dir, H::Storage), Violation> {
    let dir = harness.new_dir();
    let mut storage = harness.open(&dir);
    expect(
        "create_journal creates an empty journal in a blank directory",
        &storage.create_journal(),
        &Ok(()),
    )?;
    Ok((dir, storage))
}

/// Without a journal file, its length, reads, appends, truncation and sync are all
/// `Missing`: a blank directory is never mistaken for an empty journal (§7.7).
pub fn a_blank_directory_has_no_journal<H: Harness>(harness: &mut H) -> Outcome {
    let dir = harness.new_dir();
    let mut storage = harness.open(&dir);
    let rule = "without a journal file every journal operation is Missing";
    expect(rule, &storage.journal_len(), &Err(StorageError::Missing))?;
    expect(
        rule,
        &storage.read_at(bytes(0), read_len(1)),
        &Err(StorageError::Missing),
    )?;
    expect(rule, &storage.append(b"frame"), &Err(StorageError::Missing))?;
    expect(
        rule,
        &storage.truncate(bytes(0)),
        &Err(StorageError::Missing),
    )?;
    expect(rule, &storage.sync(), &Err(StorageError::Missing))
}

/// `create_journal` starts empty and refuses an existing journal, leaving its bytes.
pub fn create_starts_empty_and_never_clobbers<H: Harness>(harness: &mut H) -> Outcome {
    let (_dir, mut storage) = with_journal(harness)?;
    expect(
        "a new journal is empty",
        &storage.journal_len(),
        &Ok(bytes(0)),
    )?;
    expect(
        "appending to a new journal works",
        &storage.append(b"abc"),
        &Ok(()),
    )?;
    expect(
        "create_journal refuses an existing journal",
        &storage.create_journal(),
        &Err(StorageError::Failed),
    )?;
    expect(
        "a refused create_journal discards nothing",
        &read_all(&storage),
        &Ok(b"abc".to_vec()),
    )
}

/// Appends land at the end, in order, and read back exactly, at any offset and in
/// chunks up to `ReadLen::MAX`.
pub fn appends_read_back_exactly<H: Harness>(harness: &mut H) -> Outcome {
    let (_dir, mut storage) = with_journal(harness)?;
    let frames: Vec<Vec<u8>> = [1, 300, 4096, 300 * 1024]
        .iter()
        .zip(1_u8..)
        .map(|(len, seed)| pattern(seed, *len))
        .collect();
    let mut offset = 0;
    for frame in &frames {
        expect(
            "append succeeds on a journal",
            &storage.append(frame),
            &Ok(()),
        )?;
        expect(
            "each frame reads back at its offset",
            &storage.read_at(
                bytes(offset),
                read_len(frame.len().min(ReadLen::MAX as usize)),
            ),
            &Ok(frame[..frame.len().min(ReadLen::MAX as usize)].to_vec()),
        )?;
        offset += frame.len();
        expect(
            "journal_len counts every appended byte",
            &storage.journal_len(),
            &Ok(bytes(offset)),
        )?;
    }
    expect(
        "a read of exactly ReadLen::MAX bytes is served whole",
        &storage
            .read_at(bytes(1), read_len(ReadLen::MAX as usize))
            .map(|read| read.len()),
        &Ok(ReadLen::MAX as usize),
    )?;
    expect(
        "the journal is the concatenation of its appends",
        &read_all(&storage),
        &Ok(frames.concat()),
    )
}

/// A read past the end is `ShortRead` with the bytes that exist, never silently short.
pub fn reads_are_never_silently_short<H: Harness>(harness: &mut H) -> Outcome {
    let (_dir, mut storage) = with_journal(harness)?;
    expect("append succeeds", &storage.append(&pattern(7, 10)), &Ok(()))?;
    let rule = "a read past the end is ShortRead, with the bytes available";
    expect(
        rule,
        &storage.read_at(bytes(5), read_len(10)),
        &Err(StorageError::ShortRead {
            wanted: read_len(10),
            got: 5,
        }),
    )?;
    expect(
        rule,
        &storage.read_at(bytes(10), read_len(1)),
        &Err(StorageError::ShortRead {
            wanted: read_len(1),
            got: 0,
        }),
    )?;
    expect(
        rule,
        &storage.read_at(bytes(64), read_len(4)),
        &Err(StorageError::ShortRead {
            wanted: read_len(4),
            got: 0,
        }),
    )
}

/// Truncation shortens the journal at once, and appends continue at the new end.
pub fn truncate_shortens_and_appends_continue_there<H: Harness>(harness: &mut H) -> Outcome {
    let (_dir, mut storage) = with_journal(harness)?;
    let head = pattern(3, 100);
    expect("append succeeds", &storage.append(&head), &Ok(()))?;
    expect(
        "truncate to a shorter length succeeds",
        &storage.truncate(bytes(40)),
        &Ok(()),
    )?;
    expect(
        "journal_len shows the truncation before any sync",
        &storage.journal_len(),
        &Ok(bytes(40)),
    )?;
    expect(
        "reads never see truncated bytes",
        &storage.read_at(bytes(30), read_len(20)),
        &Err(StorageError::ShortRead {
            wanted: read_len(20),
            got: 10,
        }),
    )?;
    expect(
        "append after truncate succeeds",
        &storage.append(b"tail"),
        &Ok(()),
    )?;
    let mut expected = head[..40].to_vec();
    expected.extend_from_slice(b"tail");
    expect(
        "an append after a truncation lands at the new end",
        &read_all(&storage),
        &Ok(expected),
    )?;
    expect(
        "truncating to the current length changes nothing",
        &storage
            .truncate(bytes(44))
            .and_then(|()| storage.journal_len()),
        &Ok(bytes(44)),
    )?;
    expect(
        "truncating to zero empties the journal",
        &storage
            .truncate(bytes(0))
            .and_then(|()| storage.journal_len()),
        &Ok(bytes(0)),
    )
}

/// Truncation never lengthens the journal; a refused truncation changes nothing.
pub fn truncate_never_lengthens<H: Harness>(harness: &mut H) -> Outcome {
    let (_dir, mut storage) = with_journal(harness)?;
    let head = pattern(5, 10);
    expect("append succeeds", &storage.append(&head), &Ok(()))?;
    expect(
        "truncate past the end is refused",
        &storage.truncate(bytes(11)),
        &Err(StorageError::Failed),
    )?;
    expect(
        "a refused truncation changes nothing",
        &read_all(&storage),
        &Ok(head),
    )
}

/// Syncs succeed on a journal, and synced appends and truncations survive a clean
/// restart: the next process sees exactly the synced journal and can take the lock.
pub fn synced_state_survives_a_clean_restart<H: Harness>(harness: &mut H) -> Outcome {
    let dir = harness.new_dir();
    let expected = {
        let mut first = harness.open(&dir);
        expect("the first process takes the lock", &first.lock(), &Ok(()))?;
        expect("create_journal succeeds", &first.create_journal(), &Ok(()))?;
        expect("sync_dir succeeds", &first.sync_dir(), &Ok(()))?;
        let a = pattern(11, 64);
        let b = pattern(13, 32);
        expect("append succeeds", &first.append(&a), &Ok(()))?;
        expect("sync succeeds", &first.sync(), &Ok(()))?;
        expect("append succeeds", &first.append(&b), &Ok(()))?;
        expect("truncate succeeds", &first.truncate(bytes(66)), &Ok(()))?;
        expect("sync succeeds", &first.sync(), &Ok(()))?;
        expect(
            "a sync with nothing pending succeeds",
            &first.sync(),
            &Ok(()),
        )?;
        let mut expected = a;
        expected.extend_from_slice(&b[..2]);
        expected
    };
    let mut second = harness.open(&dir);
    expect(
        "a clean exit releases the lock for the next process",
        &second.lock(),
        &Ok(()),
    )?;
    expect(
        "the next process sees exactly the synced journal",
        &read_all(&second),
        &Ok(expected),
    )
}

/// The data directory's lock refuses a second holder while the first lives (DUR-8).
pub fn the_lock_refuses_a_second_holder<H: Harness>(harness: &mut H) -> Outcome {
    let dir = harness.new_dir();
    let mut first = harness.open(&dir);
    expect("the first process takes the lock", &first.lock(), &Ok(()))?;
    let mut second = harness.open(&dir);
    expect(
        "a second process is refused while the first holds the lock",
        &second.lock(),
        &Err(StorageError::Locked),
    )?;
    let other = harness.new_dir();
    let mut elsewhere = harness.open(&other);
    expect(
        "another directory's lock is independent",
        &elsewhere.lock(),
        &Ok(()),
    )?;
    drop(first);
    expect(
        "the lock is free once its holder exits",
        &second.lock(),
        &Ok(()),
    )
}

/// [`types::execute`] over the adapter, failures: any storage effect the adapter cannot
/// execute is `StorageFailed` with its op and the current incarnation, and an effect for
/// another port is not storage at all (§7.2, §7.11).
pub fn failed_effects_report_storage_failed<H: Harness>(harness: &mut H) -> Outcome {
    let dir = harness.new_dir();
    let mut storage = harness.open(&dir);
    let current = Incarnation::new(7);
    let rule = "execute reports a storage effect that fails as StorageFailed";
    let failed = |op| {
        Executed::Completion(Input::StorageFailed {
            op,
            incarnation: current,
        })
    };
    expect(
        rule,
        &execute(
            &mut storage,
            &Effect::Append {
                frame: b"x".to_vec(),
            },
            current,
        ),
        &failed(StorageOp::Append),
    )?;
    let requested = Incarnation::new(6);
    expect(
        "a failed sync reports the incarnation the sync carries",
        &execute(
            &mut storage,
            &Effect::Sync {
                seq: SyncSeq::new(3),
                incarnation: requested,
            },
            current,
        ),
        &Executed::Completion(Input::StorageFailed {
            op: StorageOp::Sync(SyncSeq::new(3)),
            incarnation: requested,
        }),
    )?;
    expect(
        rule,
        &execute(&mut storage, &Effect::CreateJournal, current),
        &Executed::Done,
    )?;
    expect(
        rule,
        &execute(&mut storage, &Effect::CreateJournal, current),
        &failed(StorageOp::CreateJournal),
    )?;
    expect(
        rule,
        &execute(&mut storage, &Effect::Truncate { len: bytes(1) }, current),
        &failed(StorageOp::Truncate),
    )?;
    expect(
        "execute leaves effects for other ports alone",
        &execute(
            &mut storage,
            &Effect::Send {
                to: NodeId::new(2).unwrap(),
                bytes: Vec::new(),
            },
            current,
        ),
        &Executed::NotStorage,
    )
}

/// [`types::execute`] over the adapter, completions: a sync's `SyncDone` echoes the
/// sequence and incarnation the sync carries, and effects without a completion report
/// nothing on success (§7.2).
pub fn syncs_complete_with_their_sequence_and_incarnation<H: Harness>(harness: &mut H) -> Outcome {
    let dir = harness.new_dir();
    let mut storage = harness.open(&dir);
    let current = Incarnation::new(7);
    let requested = Incarnation::new(6);
    let rule = "execute completes each sync with its own sequence and incarnation";
    let done = |seq| {
        Executed::Completion(Input::SyncDone {
            seq: SyncSeq::new(seq),
            incarnation: requested,
        })
    };
    expect(
        rule,
        &execute(&mut storage, &Effect::CreateJournal, current),
        &Executed::Done,
    )?;
    expect(
        rule,
        &execute(
            &mut storage,
            &Effect::SyncDir {
                seq: SyncSeq::new(4),
                incarnation: requested,
            },
            current,
        ),
        &done(4),
    )?;
    expect(
        rule,
        &execute(
            &mut storage,
            &Effect::Append {
                frame: pattern(1, 8),
            },
            current,
        ),
        &Executed::Done,
    )?;
    expect(
        rule,
        &execute(&mut storage, &Effect::Truncate { len: bytes(4) }, current),
        &Executed::Done,
    )?;
    expect(
        rule,
        &execute(
            &mut storage,
            &Effect::Sync {
                seq: SyncSeq::new(5),
                incarnation: requested,
            },
            current,
        ),
        &done(5),
    )?;
    expect(
        "execute leaves the journal as its effects say",
        &read_all(&storage),
        &Ok(pattern(1, 4)),
    )
}

/// Runs `case` against `harness` for a `#[test]` wrapper, panicking with the violation.
pub fn run<H: Harness>(name: &str, case: Case<H>, mut harness: H) {
    if let Err(violation) = case(&mut harness) {
        panic!("{violation} [case {name}]");
    }
}

/// Asserts that an adapter's test target wraps exactly the suite's cases, so none is
/// skipped (`wrapped`: the names of its wrappers).
pub fn assert_every_case_runs<H: Harness>(wrapped: &[&str]) {
    let mut cases: Vec<&str> = all_cases::<H>().into_iter().map(|(name, _)| name).collect();
    let mut wrapped = wrapped.to_vec();
    cases.sort_unstable();
    wrapped.sort_unstable();
    assert_eq!(
        wrapped, cases,
        "rule: an adapter's test target runs every contract case, each in its own #[test]"
    );
}

/// Every case, by name: the list each adapter's wrappers must match.
pub fn all_cases<H: Harness>() -> Vec<(&'static str, Case<H>)> {
    vec![
        (
            "a_blank_directory_has_no_journal",
            a_blank_directory_has_no_journal::<H>,
        ),
        (
            "create_starts_empty_and_never_clobbers",
            create_starts_empty_and_never_clobbers::<H>,
        ),
        ("appends_read_back_exactly", appends_read_back_exactly::<H>),
        (
            "reads_are_never_silently_short",
            reads_are_never_silently_short::<H>,
        ),
        (
            "truncate_shortens_and_appends_continue_there",
            truncate_shortens_and_appends_continue_there::<H>,
        ),
        ("truncate_never_lengthens", truncate_never_lengthens::<H>),
        (
            "synced_state_survives_a_clean_restart",
            synced_state_survives_a_clean_restart::<H>,
        ),
        (
            "the_lock_refuses_a_second_holder",
            the_lock_refuses_a_second_holder::<H>,
        ),
        (
            "failed_effects_report_storage_failed",
            failed_effects_report_storage_failed::<H>,
        ),
        (
            "syncs_complete_with_their_sequence_and_incarnation",
            syncs_complete_with_their_sequence_and_incarnation::<H>,
        ),
    ]
}
