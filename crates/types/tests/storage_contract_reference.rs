//! The byte-storage contract suite run against an in-memory reference adapter, and against
//! broken copies of it that each break one rule of the contract, every one of which the
//! suite must catch (PLAN.md §7.2, W12.1). The real adapters run the same suite in W12.2
//! (simulated disk) and W12.3 (real files).
#![cfg(test)]

// check: STORAGE-CONTRACT

use std::cell::RefCell;
use std::rc::Rc;

use types::{ByteCount, ByteStorage, DataDir, JournalRead, ReadLen, StorageError};

#[path = "storage_contract/mod.rs"]
mod storage_contract;

/// A contract rule a broken reference breaks on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Break {
    /// Without a journal, its length reads as zero.
    MissingIsEmpty,
    /// An append without a journal creates one.
    AppendCreatesJournal,
    /// `create_journal` replaces an existing journal.
    CreateClobbers,
    /// Appends overwrite the start of the journal.
    AppendAtStart,
    /// A read past the end returns fewer bytes without an error.
    SilentShortRead,
    /// Truncation does nothing.
    TruncateIgnored,
    /// Truncation past the end pads with zeros.
    TruncateLengthens,
    /// `sync` fails.
    SyncFails,
    /// The lock is granted to every handle.
    SharedLock,
    /// A handle's lock outlives it.
    LockKeptAfterExit,
    /// Opening a directory loses its journal's bytes.
    LostOnRestart,
}

/// One data directory: the journal, if created, and the lock's holder.
#[derive(Debug, Default)]
struct Disk {
    /// The journal's bytes.
    journal: Option<Vec<u8>>,
    /// The handle holding the lock.
    holder: Option<u64>,
}

/// The reference adapter: one process's handle on a [`Disk`]. Nothing is lost without a
/// crash, and it never crashes, so durability is trivially kept; crash behaviour is each
/// real adapter's own test (W12.2, W24).
struct Reference {
    /// The directory.
    disk: Rc<RefCell<Disk>>,
    /// This handle's identity, for the lock.
    id: u64,
    /// The rule this copy breaks, if any.
    broken: Option<Break>,
}

impl Reference {
    /// Whether this copy breaks `rule`.
    fn breaks(&self, rule: Break) -> bool {
        self.broken == Some(rule)
    }
}

impl JournalRead for Reference {
    fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
        let disk = self.disk.borrow();
        let journal = disk.journal.as_ref().ok_or(StorageError::Missing)?;
        let start = usize::try_from(offset.get()).unwrap().min(journal.len());
        let end = (start + len.get() as usize).min(journal.len());
        let read = journal[start..end].to_vec();
        if read.len() < len.get() as usize && !self.breaks(Break::SilentShortRead) {
            return Err(StorageError::ShortRead {
                wanted: len,
                got: u32::try_from(read.len()).unwrap(),
            });
        }
        Ok(read)
    }
}

impl DataDir for Reference {
    fn lock(&mut self) -> Result<(), StorageError> {
        let mut disk = self.disk.borrow_mut();
        if let Some(holder) = disk.holder
            && holder != self.id
            && !self.breaks(Break::SharedLock)
        {
            return Err(StorageError::Locked);
        }
        disk.holder = Some(self.id);
        Ok(())
    }
}

impl ByteStorage for Reference {
    fn journal_len(&self) -> Result<ByteCount, StorageError> {
        let disk = self.disk.borrow();
        if let Some(journal) = &disk.journal {
            return Ok(ByteCount::new(u64::try_from(journal.len()).unwrap()));
        }
        if self.breaks(Break::MissingIsEmpty) {
            return Ok(ByteCount::new(0));
        }
        Err(StorageError::Missing)
    }

    fn create_journal(&mut self) -> Result<(), StorageError> {
        let mut disk = self.disk.borrow_mut();
        if disk.journal.is_some() && !self.breaks(Break::CreateClobbers) {
            return Err(StorageError::Failed);
        }
        disk.journal = Some(Vec::new());
        Ok(())
    }

    fn sync_dir(&mut self) -> Result<(), StorageError> {
        Ok(())
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), StorageError> {
        let at_start = self.breaks(Break::AppendAtStart);
        let creates = self.breaks(Break::AppendCreatesJournal);
        let mut disk = self.disk.borrow_mut();
        if disk.journal.is_none() && creates {
            disk.journal = Some(Vec::new());
        }
        let journal = disk.journal.as_mut().ok_or(StorageError::Missing)?;
        if at_start && !journal.is_empty() {
            let overlap = bytes.len().min(journal.len());
            journal.splice(0..overlap, bytes.iter().copied());
        } else {
            journal.extend_from_slice(bytes);
        }
        Ok(())
    }

    fn truncate(&mut self, len: ByteCount) -> Result<(), StorageError> {
        let ignored = self.breaks(Break::TruncateIgnored);
        let lengthens = self.breaks(Break::TruncateLengthens);
        let mut disk = self.disk.borrow_mut();
        let journal = disk.journal.as_mut().ok_or(StorageError::Missing)?;
        let len = usize::try_from(len.get()).unwrap();
        if ignored {
            return Ok(());
        }
        if len > journal.len() {
            if lengthens {
                journal.resize(len, 0);
                return Ok(());
            }
            return Err(StorageError::Failed);
        }
        journal.truncate(len);
        Ok(())
    }

    fn sync(&mut self) -> Result<(), StorageError> {
        if self.disk.borrow().journal.is_none() {
            return Err(StorageError::Missing);
        }
        if self.breaks(Break::SyncFails) {
            return Err(StorageError::Failed);
        }
        Ok(())
    }
}

impl Drop for Reference {
    fn drop(&mut self) {
        if self.breaks(Break::LockKeptAfterExit) {
            return;
        }
        let mut disk = self.disk.borrow_mut();
        if disk.holder == Some(self.id) {
            disk.holder = None;
        }
    }
}

/// The reference harness: each directory is a shared in-memory [`Disk`]; `broken`
/// applies to every handle it opens.
#[derive(Default)]
struct ReferenceHarness {
    /// The rule broken, if any.
    broken: Option<Break>,
    /// Handles opened so far, for their identities.
    opened: u64,
}

impl storage_contract::Harness for ReferenceHarness {
    type Dir = Rc<RefCell<Disk>>;
    type Storage = Reference;

    fn new_dir(&mut self) -> Self::Dir {
        Rc::default()
    }

    fn open(&mut self, dir: &Self::Dir) -> Reference {
        self.opened += 1;
        if self.broken == Some(Break::LostOnRestart) {
            let mut disk = dir.borrow_mut();
            if disk.journal.is_some() {
                disk.journal = Some(Vec::new());
            }
        }
        Reference {
            disk: Rc::clone(dir),
            id: self.opened,
            broken: self.broken,
        }
    }
}

/// The suite against the reference: every case passes, each in its own test.
mod contract_reference {
    use crate::ReferenceHarness;
    use crate::storage_contract::{self as contract, run};

    #[test]
    fn a_blank_directory_has_no_journal() {
        run(
            "a_blank_directory_has_no_journal",
            contract::a_blank_directory_has_no_journal,
            ReferenceHarness::default(),
        );
    }

    #[test]
    fn create_starts_empty_and_never_clobbers() {
        run(
            "create_starts_empty_and_never_clobbers",
            contract::create_starts_empty_and_never_clobbers,
            ReferenceHarness::default(),
        );
    }

    #[test]
    fn appends_read_back_exactly() {
        run(
            "appends_read_back_exactly",
            contract::appends_read_back_exactly,
            ReferenceHarness::default(),
        );
    }

    #[test]
    fn reads_are_never_silently_short() {
        run(
            "reads_are_never_silently_short",
            contract::reads_are_never_silently_short,
            ReferenceHarness::default(),
        );
    }

    #[test]
    fn truncate_shortens_and_appends_continue_there() {
        run(
            "truncate_shortens_and_appends_continue_there",
            contract::truncate_shortens_and_appends_continue_there,
            ReferenceHarness::default(),
        );
    }

    #[test]
    fn truncate_never_lengthens() {
        run(
            "truncate_never_lengthens",
            contract::truncate_never_lengthens,
            ReferenceHarness::default(),
        );
    }

    #[test]
    fn synced_state_survives_a_clean_restart() {
        run(
            "synced_state_survives_a_clean_restart",
            contract::synced_state_survives_a_clean_restart,
            ReferenceHarness::default(),
        );
    }

    #[test]
    fn the_lock_refuses_a_second_holder() {
        run(
            "the_lock_refuses_a_second_holder",
            contract::the_lock_refuses_a_second_holder,
            ReferenceHarness::default(),
        );
    }

    #[test]
    fn failed_effects_report_storage_failed() {
        run(
            "failed_effects_report_storage_failed",
            contract::failed_effects_report_storage_failed,
            ReferenceHarness::default(),
        );
    }

    #[test]
    fn syncs_complete_with_their_sequence_and_incarnation() {
        run(
            "syncs_complete_with_their_sequence_and_incarnation",
            contract::syncs_complete_with_their_sequence_and_incarnation,
            ReferenceHarness::default(),
        );
    }

    #[test]
    fn every_contract_case_runs_here() {
        contract::assert_every_case_runs::<ReferenceHarness>(&[
            "a_blank_directory_has_no_journal",
            "create_starts_empty_and_never_clobbers",
            "appends_read_back_exactly",
            "reads_are_never_silently_short",
            "truncate_shortens_and_appends_continue_there",
            "truncate_never_lengthens",
            "synced_state_survives_a_clean_restart",
            "the_lock_refuses_a_second_holder",
            "failed_effects_report_storage_failed",
            "syncs_complete_with_their_sequence_and_incarnation",
        ]);
    }
}

/// The suite against broken references: each broken rule is caught by the case that
/// states it (and possibly others).
mod contract_broken {
    use super::{Break, ReferenceHarness, storage_contract};

    /// The cases that report a violation against a reference breaking `rule`.
    fn caught_by(rule: Break) -> Vec<&'static str> {
        storage_contract::all_cases::<ReferenceHarness>()
            .into_iter()
            .filter(|(_, case)| {
                let mut harness = ReferenceHarness {
                    broken: Some(rule),
                    opened: 0,
                };
                case(&mut harness).is_err()
            })
            .map(|(name, _)| name)
            .collect()
    }

    /// Asserts the suite catches `rule` in the case `expected`.
    fn assert_caught(rule: Break, expected: &str) {
        let caught = caught_by(rule);
        assert!(
            caught.contains(&expected),
            "rule: the contract suite catches an adapter that breaks {rule:?} in case \
             {expected}; it was caught by {caught:?}"
        );
    }

    #[test]
    fn a_missing_journal_read_as_empty_is_caught() {
        assert_caught(Break::MissingIsEmpty, "a_blank_directory_has_no_journal");
    }

    #[test]
    fn an_append_that_creates_the_journal_is_caught() {
        assert_caught(
            Break::AppendCreatesJournal,
            "a_blank_directory_has_no_journal",
        );
    }

    #[test]
    fn a_create_that_clobbers_is_caught() {
        assert_caught(
            Break::CreateClobbers,
            "create_starts_empty_and_never_clobbers",
        );
    }

    #[test]
    fn a_misplaced_append_is_caught() {
        assert_caught(Break::AppendAtStart, "appends_read_back_exactly");
    }

    #[test]
    fn a_silent_short_read_is_caught() {
        assert_caught(Break::SilentShortRead, "reads_are_never_silently_short");
    }

    #[test]
    fn an_ignored_truncate_is_caught() {
        assert_caught(
            Break::TruncateIgnored,
            "truncate_shortens_and_appends_continue_there",
        );
    }

    #[test]
    fn a_lengthening_truncate_is_caught() {
        assert_caught(Break::TruncateLengthens, "truncate_never_lengthens");
    }

    #[test]
    fn a_failing_sync_is_caught() {
        assert_caught(Break::SyncFails, "synced_state_survives_a_clean_restart");
        assert_caught(
            Break::SyncFails,
            "syncs_complete_with_their_sequence_and_incarnation",
        );
    }

    #[test]
    fn a_shared_lock_is_caught() {
        assert_caught(Break::SharedLock, "the_lock_refuses_a_second_holder");
    }

    #[test]
    fn a_lock_kept_after_exit_is_caught() {
        assert_caught(Break::LockKeptAfterExit, "the_lock_refuses_a_second_holder");
    }

    #[test]
    fn bytes_lost_on_restart_are_caught() {
        assert_caught(
            Break::LostOnRestart,
            "synced_state_survives_a_clean_restart",
        );
    }

    #[test]
    fn the_unbroken_reference_passes_every_case() {
        let failing: Vec<&str> = storage_contract::all_cases::<ReferenceHarness>()
            .into_iter()
            .filter(|(_, case)| case(&mut ReferenceHarness::default()).is_err())
            .map(|(name, _)| name)
            .collect();
        assert!(
            failing.is_empty(),
            "rule: the reference adapter keeps the contract; failing: {failing:?}"
        );
    }
}
