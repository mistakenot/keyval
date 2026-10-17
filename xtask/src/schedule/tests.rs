#![cfg(test)]
//! Fixtures for the scheduler's ledger, locks, arbitration and runner (PLAN.md §10.10,
//! §10.11, §11.12). The gate is a script that prints a canned report.

use std::os::unix::fs::PermissionsExt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::*;
use crate::schedule_plan::Blocker;
use crate::test_support::scratch_dir;

const NOW: u64 = 20_000 * 86_400 + 5 * 3_600 + 30;
const SLOT: u64 = 20_000 * 86_400 + 5 * 3_600;

/// A child forked by one test shares a flock another test holds until it execs, so the
/// tests that spawn the gate or hold a lock take turns.
static TURN: Mutex<()> = Mutex::new(());

fn take_turn() -> MutexGuard<'static, ()> {
    TURN.lock().unwrap_or_else(PoisonError::into_inner)
}

fn root(name: &str) -> PathBuf {
    let root = scratch_dir(name);
    fs::create_dir_all(&root).unwrap();
    root
}

fn fake_gate(root: &Path, stdout: &str, exit: i32) {
    let path = root.join("dev");
    fs::write(
        &path,
        format!("#!/bin/sh\ncat <<'EOF_JSON'\n{stdout}\nEOF_JSON\nexit {exit}\n"),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
}

const PASSED: &str = r#"{"ok":true,"receipt_path":"artifacts/receipts/r.json","receipt":{"tree":{"id":"abc123"},"tier_plan":{"measured":{"fresh_seeds":2000}}}}"#;
const FAILED: &str = r#"{"ok":false,"receipt_path":"artifacts/receipts/r.json","receipt":{"tree":{"id":"abc123"},"tier_plan":{"measured":{"fresh_seeds":700}}}}"#;

fn run(root: &Path, job: Job) -> ScheduleReport {
    execute(root, &Action::Run(job), NOW)
}

fn ran(report: &ScheduleReport) -> &Ran {
    report.run.as_ref().expect("a run report")
}

// check: SCHEDULE-RUN
#[test]
fn a_passing_run_is_recorded_once_per_slot_with_its_seeds() {
    let _turn = take_turn();
    let root = root("schedule-pass");
    fake_gate(&root, PASSED, 0);
    let first = run(&root, Job::T1);
    let record = ran(&first).record.clone().expect("a record");
    assert_eq!(ran(&first).outcome, "ran", "report: {first:?}");
    assert_eq!(
        (record.state, record.attempt, record.slot),
        (State::Passed, 1, SLOT)
    );
    assert_eq!(record.tree.as_deref(), Some("abc123"));
    assert_eq!(
        record.seeds,
        Seeds {
            target: 2_000,
            completed: Some(2_000),
            remaining: Some(0)
        }
    );
    assert!(first.ok, "rule: a passed run is ok; {first:?}");
    let again = run(&root, Job::T1);
    assert_eq!(
        ran(&again).outcome,
        "nothing",
        "rule: a finished slot is not run twice"
    );
}

#[test]
fn a_failed_run_is_failed_with_its_remaining_seeds_and_the_report_is_not_ok() {
    let _turn = take_turn();
    let root = root("schedule-fail");
    fake_gate(&root, FAILED, 1);
    let report = run(&root, Job::T1);
    let record = ran(&report).record.clone().expect("a record");
    assert_eq!(record.state, State::Failed);
    assert_eq!(record.seeds.completed, Some(700));
    assert_eq!(record.seeds.remaining, Some(1_300));
    assert!(!report.ok, "rule: a failed gate fails the command (P10)");
}

#[test]
fn an_interrupted_run_stays_incomplete_and_the_next_one_resumes_it() {
    let _turn = take_turn();
    let root = root("schedule-resume");
    fake_gate(&root, "the gate died", 137);
    let first = run(&root, Job::T1);
    let incomplete = ran(&first).record.clone().expect("a record");
    assert_eq!(
        incomplete.state,
        State::Incomplete,
        "rule: no report is never a pass"
    );
    assert_eq!(
        incomplete.seeds.remaining,
        Some(2_000),
        "rule: remaining seeds are explicit"
    );
    assert!(!first.ok);
    fake_gate(&root, PASSED, 0);
    let second = run(&root, Job::T1);
    let resumed = ran(&second).record.clone().expect("a record");
    assert_eq!(
        (resumed.attempt, resumed.state),
        (2, State::Passed),
        "report: {second:?}"
    );
    assert_eq!(resumed.resumes.as_deref(), Some(incomplete.id.as_str()));
    let kept = load(&root.join(DIR)).unwrap();
    assert!(
        kept.iter()
            .any(|r| r.id == incomplete.id && r.state == State::Incomplete),
        "rule: the interrupted attempt stays on the ledger: {kept:?}"
    );
}

#[test]
fn a_job_waits_while_a_t0_commit_holds_the_commit_lock() {
    let _turn = take_turn();
    let root = root("schedule-t0");
    fake_gate(&root, PASSED, 0);
    let lock = root.join(LOCK_FILE);
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    let held = open_lock(&lock).unwrap();
    held.try_lock().unwrap();
    let report = run(&root, Job::T1);
    let record = ran(&report).record.clone().expect("a record");
    assert_eq!(
        (ran(&report).outcome.as_str(), record.state),
        ("queued", State::Queued)
    );
    assert_eq!(record.blocked_by, Some(Blocker::T0Commit));
    drop(held);
    let after = run(&root, Job::T1);
    let record = ran(&after).record.clone().expect("a record");
    assert_eq!(
        (record.id.as_str(), record.state),
        (record_id(Job::T1, SLOT, 1).as_str(), State::Passed),
        "rule: the queued job runs when T0 is out of the way, under the same record"
    );
}

#[test]
fn only_one_campaign_runs_at_a_time_and_a_running_one_is_visible() {
    let _turn = take_turn();
    let root = root("schedule-one");
    fake_gate(&root, PASSED, 0);
    let dir = root.join(DIR);
    let held = take_campaign_lock(&dir).unwrap().expect("the lock is free");
    let mut running = new_record(Job::T1, SLOT, 1, NOW);
    running.state = State::Running;
    save(&dir, &running).unwrap();
    let report = run(&root, Job::T2);
    assert_eq!(ran(&report).outcome, "queued", "report: {report:?}");
    assert_eq!(
        ran(&report).record.as_ref().and_then(|r| r.blocked_by),
        Some(Blocker::CampaignRunning)
    );
    let status = execute(&root, &Action::Status, NOW);
    let t1 = status.jobs.iter().find(|j| j.job == Job::T1).unwrap();
    assert!(
        matches!(t1.due, Due::InProgress { .. }),
        "rule: a live holder keeps it running: {t1:?}"
    );
    drop(held);
    let status = execute(&root, &Action::Status, NOW);
    let t1 = status.jobs.iter().find(|j| j.job == Job::T1).unwrap();
    assert!(
        t1.recent.iter().any(|r| r.state == State::Incomplete),
        "rule: a running record whose holder is gone reads as incomplete, never passed: {t1:?}"
    );
}

#[test]
fn t2_goes_after_an_owed_t1() {
    let _turn = take_turn();
    let root = root("schedule-order");
    fake_gate(&root, PASSED, 0);
    let now = 20_000 * 86_400 + 2 * 3_600 + 5;
    let report = execute(&root, &Action::Run(Job::T2), now);
    let record = ran(&report).record.clone().expect("a record");
    assert_eq!(
        (record.state, record.blocked_by),
        (State::Queued, Some(Blocker::T1Due))
    );
}

#[test]
fn status_names_missed_slots_and_changes_nothing() {
    let root = root("schedule-status");
    let report = execute(&root, &Action::Status, NOW);
    assert!(report.ok);
    let t1 = report.jobs.iter().find(|j| j.job == Job::T1).unwrap();
    assert_eq!(t1.latest_slot, Some(SLOT));
    assert!(
        !t1.missed.is_empty(),
        "rule: slots no record covers are missed: {t1:?}"
    );
    assert!(
        !root.join(DIR).join(RECORDS).exists(),
        "rule: status writes no record"
    );
    let json = serde_json::to_string(&report).unwrap();
    assert!(json.contains(SCHEMA));
}

#[test]
fn install_writes_the_units_and_enables_nothing_without_apply() {
    let root = root("schedule-install");
    let units = root.join("units");
    let report = execute(
        &root,
        &Action::Install {
            apply: false,
            dir: Some(units.clone()),
        },
        NOW,
    );
    let install = report.install.as_ref().expect("an install report");
    assert!(report.ok && !install.applied, "report: {report:?}");
    for name in &install.units {
        assert!(units.join(name).is_file(), "rule: {name} was written");
    }
    assert_eq!(install.units.len(), 4);
}

#[test]
fn the_gate_report_is_read_without_guessing() {
    let ok = judge(PASSED, true, "");
    assert_eq!((ok.state, ok.fresh_seeds), (State::Passed, Some(2_000)));
    let bad = judge(FAILED, false, "");
    assert_eq!(bad.state, State::Failed);
    let exit_mismatch = judge(PASSED, false, "");
    assert_eq!(
        exit_mismatch.state,
        State::Failed,
        "rule: a pass needs both the report and the exit status"
    );
    let refused = judge(
        r#"{"schema":"keyval.dev.error/v1","message":"working tree is dirty"}"#,
        false,
        "",
    );
    assert_eq!(
        refused.state,
        State::Queued,
        "rule: a refusal judged nothing, so it is neither a pass nor a failure; the slot stays owed"
    );
    assert!(refused.detail.as_deref().unwrap_or("").contains("dirty"));
    assert_eq!(judge("", false, "boom").state, State::Incomplete);
}

#[test]
fn a_refusal_keeps_the_slot_queued_on_one_record_instead_of_burning_attempts() {
    let _turn = take_turn();
    let root = root("schedule-refused");
    fake_gate(&root, r#"{"message":"working tree is dirty"}"#, 2);
    for _ in 0..3 {
        let report = run(&root, Job::T1);
        let record = ran(&report).record.clone().expect("a record");
        assert_eq!(
            (ran(&report).outcome.as_str(), record.state, record.attempt),
            ("queued", State::Queued, 1),
            "report: {report:?}"
        );
        assert!(record.detail.as_deref().unwrap_or("").contains("dirty"));
    }
    assert_eq!(
        load(&root.join(DIR)).unwrap().len(),
        1,
        "rule: one record, however often it is retried"
    );
    fake_gate(&root, PASSED, 0);
    let record = ran(&run(&root, Job::T1)).record.clone().expect("a record");
    assert_eq!((record.attempt, record.state), (1, State::Passed));
}

#[test]
fn a_run_that_finds_a_dead_runner_rewrites_it_incomplete_and_resumes_it() {
    let _turn = take_turn();
    let root = root("schedule-dead");
    fake_gate(&root, PASSED, 0);
    let dir = root.join(DIR);
    let mut dead = new_record(Job::T1, SLOT, 1, NOW);
    dead.state = State::Running;
    save(&dir, &dead).unwrap();
    let report = run(&root, Job::T1);
    let record = ran(&report).record.clone().expect("a record");
    assert_eq!(
        (record.attempt, record.resumes.as_deref()),
        (2, Some(dead.id.as_str()))
    );
    let kept = load(&dir).unwrap();
    let first = kept
        .iter()
        .find(|r| r.id == dead.id)
        .expect("the first attempt stays");
    assert_eq!(
        first.state,
        State::Incomplete,
        "rule: rewritten on the ledger, not only in the report"
    );
}

#[test]
fn a_run_starts_what_else_is_owed_once_it_is_done() {
    let _turn = take_turn();
    let root = root("schedule-drain");
    fake_gate(&root, PASSED, 0);
    let now = 20_000 * 86_400 + 2 * 3_600 + 5;
    let report = execute(&root, &Action::Run(Job::T1), now);
    assert!(report.ok, "report: {report:?}");
    let t2 = report.jobs.iter().find(|j| j.job == Job::T2).unwrap();
    assert!(
        matches!(t2.due, Due::Done { .. }),
        "rule: T2, which would have queued behind T1, runs as soon as T1 is done: {t2:?}"
    );
}

#[test]
fn an_unreadable_record_is_set_aside_and_scheduling_goes_on() {
    let _turn = take_turn();
    let root = root("schedule-garbage");
    fake_gate(&root, PASSED, 0);
    let records = root.join(DIR).join(RECORDS);
    fs::create_dir_all(&records).unwrap();
    fs::write(records.join("t1-1-a1.json"), "{ truncated").unwrap();
    let report = run(&root, Job::T1);
    assert_eq!(ran(&report).outcome, "ran", "report: {report:?}");
    assert!(
        records.join("t1-1-a1.bad").is_file(),
        "rule: set aside, never deleted"
    );
}

#[test]
fn the_ledger_view_is_bounded_at_n_minus_one_n_and_n_plus_one() {
    let make = |n: u64| -> Vec<Record> { (0..n).map(|i| new_record(Job::T1, i, 1, i)).collect() };
    for (given, kept) in [(2, 2), (3, 3), (4, 3)] {
        let out = keep_newest(make(given), 3);
        assert_eq!(out.len(), kept, "rule: bound 3, given {given}");
        assert_eq!(
            out.last().map(|r| r.slot),
            Some(given - 1),
            "rule: the newest records are the ones kept"
        );
    }
}
