#![cfg(test)]
//! Fixtures for slots, missed runs, resumption, arbitration and the generated units
//! (PLAN.md §10.10, §10.11, §11.12).

use super::*;

const DAY: u64 = 20_000 * 86_400;

fn record(job: Job, slot: u64, attempt: u32, state: State) -> Record {
    Record {
        id: record_id(job, slot, attempt),
        job,
        slot,
        attempt,
        state,
        queued_at: slot,
        started_at: None,
        finished_at: None,
        resumes: None,
        blocked_by: None,
        tree: None,
        receipt: None,
        seeds: Seeds::unmeasured(job.seed_target()),
        detail: None,
    }
}

const IDLE: Busy = Busy {
    commit_lock_held: false,
    campaign_lock_held: false,
    t1_owed: false,
};

// check: SCHEDULE-PLAN
#[test]
fn slots_are_hourly_for_t1_and_two_oclock_utc_for_t2() {
    assert_eq!(
        latest_slot(Job::T1, DAY + 3 * 3_600 + 59),
        Some(DAY + 3 * 3_600)
    );
    assert_eq!(latest_slot(Job::T1, DAY), Some(DAY));
    assert_eq!(latest_slot(Job::T2, DAY + 2 * 3_600), Some(DAY + 2 * 3_600));
    assert_eq!(
        latest_slot(Job::T2, DAY + 2 * 3_600 - 1),
        Some(DAY - 86_400 + 2 * 3_600),
        "rule: before 02:00 UTC the latest T2 slot is last night's"
    );
    assert_eq!(
        latest_slot(Job::T2, 3_600),
        None,
        "rule: no slot before the first"
    );
}

#[test]
fn slots_the_machine_slept_through_are_missed_and_never_passed() {
    let now = DAY + 5 * 3_600 + 10;
    let recorded: BTreeSet<u64> = [DAY + 3 * 3_600].into();
    let slots = missed(Job::T1, now, &recorded);
    assert!(
        slots.contains(&(DAY + 4 * 3_600)),
        "slot 04:00 was slept through: {slots:?}"
    );
    assert!(
        !slots.contains(&(DAY + 3 * 3_600)),
        "a recorded slot is not missed"
    );
    assert!(
        !slots.contains(&(DAY + 5 * 3_600)),
        "the latest slot is due, not missed"
    );
    assert!(
        slots.windows(2).all(|w| w[0] < w[1]),
        "oldest first: {slots:?}"
    );
    assert!(
        slots.len() as u64 <= LOOKBACK_SLOTS,
        "CD-9: the report is bounded"
    );
}

#[test]
fn only_the_latest_slot_is_owed_and_a_fresh_one_starts_at_attempt_one() {
    let now = DAY + 4 * 3_600 + 5;
    assert_eq!(
        due(Job::T1, now, &[]),
        Due::Owed {
            slot: DAY + 4 * 3_600,
            attempt: 1,
            resumes: None,
            queued: None
        },
        "rule: older unrecorded slots stay missed; only the latest is started"
    );
}

#[test]
fn a_finished_slot_owes_nothing_and_a_running_one_is_left_alone() {
    let slot = DAY + 4 * 3_600;
    let now = slot + 5;
    for ended in [State::Passed, State::Failed] {
        let rs = [record(Job::T1, slot, 1, ended)];
        assert!(
            matches!(due(Job::T1, now, &rs), Due::Done { .. }),
            "{ended:?} ends the slot"
        );
    }
    let rs = [record(Job::T1, slot, 1, State::Running)];
    assert!(matches!(due(Job::T1, now, &rs), Due::InProgress { .. }));
}

#[test]
fn an_interrupted_run_stays_incomplete_with_seeds_explicit_and_is_resumed_as_a_new_attempt() {
    let slot = DAY + 2 * 3_600;
    let mut running = record(Job::T2, slot, 1, State::Running);
    running.seeds = Seeds {
        target: 10_000,
        completed: Some(4_000),
        remaining: Some(6_000),
    };
    let dead = effective(&running, false);
    assert_eq!(
        dead.state,
        State::Incomplete,
        "rule: no lock holder means interrupted, not passed"
    );
    assert_eq!(dead.seeds.target, 10_000);
    assert_eq!(
        dead.seeds.remaining,
        Some(10_000),
        "rule: nothing durable, all seeds owed"
    );
    assert_eq!(
        effective(&running, true).state,
        State::Running,
        "a live holder keeps it running"
    );
    assert_eq!(
        due(Job::T2, slot + 60, std::slice::from_ref(&dead)),
        Due::Owed {
            slot,
            attempt: 2,
            resumes: Some(dead.id),
            queued: None
        }
    );
}

#[test]
fn a_queued_job_is_taken_over_not_duplicated() {
    let slot = DAY + 3_600;
    let queued = record(Job::T1, slot, 1, State::Queued);
    assert_eq!(
        due(Job::T1, slot + 9, std::slice::from_ref(&queued)),
        Due::Owed {
            slot,
            attempt: 1,
            resumes: None,
            queued: Some(queued.id)
        }
    );
}

#[test]
fn arbitration_puts_t0_before_t1_before_t2_and_runs_one_campaign() {
    let owed = |job| due(job, DAY + 2 * 3_600 + 1, &[]);
    let queue = |decision: Decision| match decision {
        Decision::Queue { blocker, .. } => Some(blocker),
        Decision::Run { .. } | Decision::Nothing(_) => None,
    };
    let t0 = Busy {
        commit_lock_held: true,
        ..IDLE
    };
    assert_eq!(
        queue(decide(Job::T1, owed(Job::T1), t0)),
        Some(Blocker::T0Commit)
    );
    assert_eq!(
        queue(decide(Job::T2, owed(Job::T2), t0)),
        Some(Blocker::T0Commit)
    );
    let busy = Busy {
        campaign_lock_held: true,
        ..IDLE
    };
    assert_eq!(
        queue(decide(Job::T1, owed(Job::T1), busy)),
        Some(Blocker::CampaignRunning)
    );
    let t1 = Busy {
        t1_owed: true,
        ..IDLE
    };
    assert_eq!(
        queue(decide(Job::T2, owed(Job::T2), t1)),
        Some(Blocker::T1Due)
    );
    assert_eq!(
        queue(decide(Job::T1, owed(Job::T1), t1)),
        None,
        "rule: T1 never waits for T2"
    );
    assert!(matches!(
        decide(Job::T2, owed(Job::T2), IDLE),
        Decision::Run { attempt: 1, .. }
    ));
    let done = Due::Done {
        slot: DAY,
        record: "x".to_owned(),
    };
    assert!(matches!(decide(Job::T1, done, t0), Decision::Nothing(_)));
}

#[test]
fn units_run_the_scheduler_at_low_priority_and_the_timers_are_persistent() {
    let units = unit_files("/srv/keyval", "/usr/bin");
    let names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "keyval-schedule-t1.service",
            "keyval-schedule-t1.timer",
            "keyval-schedule-t2.service",
            "keyval-schedule-t2.timer"
        ]
    );
    for unit in &units {
        if unit.name.ends_with(".service") {
            for needle in [
                "Nice=19",
                "CPUSchedulingPolicy=idle",
                "TimeoutStartSec=infinity",
                "IOSchedulingClass=idle",
                "WorkingDirectory=/srv/keyval",
                "/srv/keyval/dev schedule run",
            ] {
                assert!(
                    unit.content.contains(needle),
                    "{}: missing {needle}\n{}",
                    unit.name,
                    unit.content
                );
            }
        } else {
            assert!(
                unit.content.contains(" UTC\n"),
                "rule: calendars are UTC so slots do not move with the zone; {}",
                unit.name
            );
            assert!(
                unit.content.contains("Persistent=true"),
                "rule: a slept-through slot fires once on waking; {}",
                unit.name
            );
        }
    }
    let commands = apply_commands("/srv/units", &units);
    assert_eq!(commands.len(), 5, "four links and one enable: {commands:?}");
    assert_eq!(
        commands.last().map(|c| c.join(" ")),
        Some(
            "systemctl --user enable --now keyval-schedule-t1.timer keyval-schedule-t2.timer"
                .to_owned()
        )
    );
}
