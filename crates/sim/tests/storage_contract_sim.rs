//! The byte-storage contract suite (W12.1), unmodified, against the simulated disk
//! (W12.2): what simulation proves holds for production only if both adapters keep the
//! same contract (P3, T-2).
#![cfg(test)]

// check: STORAGE-CONTRACT-SIM

use sim::{DiskHandle, SimDisk};

#[path = "../../types/tests/storage_contract/mod.rs"]
mod storage_contract;

/// The simulated disk as a contract harness: a directory is a [`SimDisk`], and opening
/// it is a new process's [`DiskHandle`]. No crash happens here; the crash model is
/// tested in `sim::disk`.
#[derive(Default)]
struct SimHarness;

impl storage_contract::Harness for SimHarness {
    type Dir = SimDisk;
    type Storage = DiskHandle;

    fn new_dir(&mut self) -> SimDisk {
        SimDisk::new()
    }

    fn open(&mut self, dir: &SimDisk) -> DiskHandle {
        dir.open()
    }
}

/// Every contract case, each in its own test.
mod contract_sim_disk {
    use crate::SimHarness;
    use crate::storage_contract::{self as contract, run};

    #[test]
    fn a_blank_directory_has_no_journal() {
        run(
            "a_blank_directory_has_no_journal",
            contract::a_blank_directory_has_no_journal,
            SimHarness,
        );
    }

    #[test]
    fn create_starts_empty_and_never_clobbers() {
        run(
            "create_starts_empty_and_never_clobbers",
            contract::create_starts_empty_and_never_clobbers,
            SimHarness,
        );
    }

    #[test]
    fn appends_read_back_exactly() {
        run(
            "appends_read_back_exactly",
            contract::appends_read_back_exactly,
            SimHarness,
        );
    }

    #[test]
    fn reads_are_never_silently_short() {
        run(
            "reads_are_never_silently_short",
            contract::reads_are_never_silently_short,
            SimHarness,
        );
    }

    #[test]
    fn truncate_shortens_and_appends_continue_there() {
        run(
            "truncate_shortens_and_appends_continue_there",
            contract::truncate_shortens_and_appends_continue_there,
            SimHarness,
        );
    }

    #[test]
    fn truncate_never_lengthens() {
        run(
            "truncate_never_lengthens",
            contract::truncate_never_lengthens,
            SimHarness,
        );
    }

    #[test]
    fn synced_state_survives_a_clean_restart() {
        run(
            "synced_state_survives_a_clean_restart",
            contract::synced_state_survives_a_clean_restart,
            SimHarness,
        );
    }

    #[test]
    fn the_lock_refuses_a_second_holder() {
        run(
            "the_lock_refuses_a_second_holder",
            contract::the_lock_refuses_a_second_holder,
            SimHarness,
        );
    }

    #[test]
    fn failed_effects_report_storage_failed() {
        run(
            "failed_effects_report_storage_failed",
            contract::failed_effects_report_storage_failed,
            SimHarness,
        );
    }

    #[test]
    fn syncs_complete_with_their_sequence_and_incarnation() {
        run(
            "syncs_complete_with_their_sequence_and_incarnation",
            contract::syncs_complete_with_their_sequence_and_incarnation,
            SimHarness,
        );
    }

    #[test]
    fn every_contract_case_runs_here() {
        contract::assert_every_case_runs::<SimHarness>(&[
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
