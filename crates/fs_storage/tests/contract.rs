//! The byte-storage contract suite (W12.1) run unmodified against the real-file adapter
//! (PLAN.md §7.2; W12.3): what the suite proves of the simulated disk holds for the files
//! production writes (P3, T-2).
#![cfg(test)]

// check: STORAGE-CONTRACT-FS

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use fs_storage::{FsStorage, create_data_dir};

#[path = "../../types/tests/storage_contract/mod.rs"]
mod storage_contract;

/// Real data directories under cargo's per-target temp dir, each created as init creates
/// one ([`create_data_dir`]).
struct FsHarness;

impl storage_contract::Harness for FsHarness {
    type Dir = PathBuf;
    type Storage = FsStorage;

    fn new_dir(&mut self) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "fs-storage-contract-{nanos}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        create_data_dir(&dir).unwrap();
        dir
    }

    fn open(&mut self, dir: &PathBuf) -> FsStorage {
        FsStorage::open(dir).unwrap()
    }
}

mod contract_fs {
    use crate::FsHarness;
    use crate::storage_contract::{self as contract, run};

    #[test]
    fn a_blank_directory_has_no_journal() {
        run(
            "a_blank_directory_has_no_journal",
            contract::a_blank_directory_has_no_journal,
            FsHarness,
        );
    }

    #[test]
    fn create_starts_empty_and_never_clobbers() {
        run(
            "create_starts_empty_and_never_clobbers",
            contract::create_starts_empty_and_never_clobbers,
            FsHarness,
        );
    }

    #[test]
    fn appends_read_back_exactly() {
        run(
            "appends_read_back_exactly",
            contract::appends_read_back_exactly,
            FsHarness,
        );
    }

    #[test]
    fn reads_are_never_silently_short() {
        run(
            "reads_are_never_silently_short",
            contract::reads_are_never_silently_short,
            FsHarness,
        );
    }

    #[test]
    fn truncate_shortens_and_appends_continue_there() {
        run(
            "truncate_shortens_and_appends_continue_there",
            contract::truncate_shortens_and_appends_continue_there,
            FsHarness,
        );
    }

    #[test]
    fn truncate_never_lengthens() {
        run(
            "truncate_never_lengthens",
            contract::truncate_never_lengthens,
            FsHarness,
        );
    }

    #[test]
    fn synced_state_survives_a_clean_restart() {
        run(
            "synced_state_survives_a_clean_restart",
            contract::synced_state_survives_a_clean_restart,
            FsHarness,
        );
    }

    #[test]
    fn the_lock_refuses_a_second_holder() {
        run(
            "the_lock_refuses_a_second_holder",
            contract::the_lock_refuses_a_second_holder,
            FsHarness,
        );
    }

    #[test]
    fn failed_effects_report_storage_failed() {
        run(
            "failed_effects_report_storage_failed",
            contract::failed_effects_report_storage_failed,
            FsHarness,
        );
    }

    #[test]
    fn syncs_complete_with_their_sequence_and_incarnation() {
        run(
            "syncs_complete_with_their_sequence_and_incarnation",
            contract::syncs_complete_with_their_sequence_and_incarnation,
            FsHarness,
        );
    }

    #[test]
    fn every_contract_case_runs() {
        contract::assert_every_case_runs::<FsHarness>(&[
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
