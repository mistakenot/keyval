//! The pure part of the local scheduler (PLAN.md §10.10 "Scheduled gates", §11.12; W46.2):
//! which hourly and nightly slots exist, which of them were missed, what is due now, who
//! may start it, and the systemd units that fire it. Nothing here reads the clock, a file
//! or a lock: callers pass the time and the facts, so every rule is a fixture away from a
//! test.
//!
//! The rules it fixes:
//!
//! * A **slot** is a point in time a job is owed: T1 at the top of every UTC hour, T2 at
//!   02:00 UTC every night. A stopped or sleeping machine has **missed** the slots it slept
//!   through; a missed slot is reported as missed and never as passed (§10.10).
//! * Only the **latest** slot is ever started. An older slot with no record stays missed.
//! * A slot whose run was interrupted stays `incomplete`, with its completed and remaining
//!   seeds explicit, and the latest slot resumes it as a new attempt that names the record
//!   it resumes.
//! * **Arbitration** (§11.12): T0 has priority over T1 and T1 over T2, and at most one
//!   campaign runs at a time. A job that has to wait is **queued**, visibly, never dropped.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::gate::GateTier;

/// Seconds between T1 slots.
pub const T1_PERIOD: u64 = 3_600;
/// Seconds between T2 slots.
pub const T2_PERIOD: u64 = 86_400;
/// Seconds after midnight UTC of the T2 slot.
pub const T2_OFFSET: u64 = 2 * 3_600;
/// How many slots back a status reports missed ones (CD-9: a bound on the report).
pub const LOOKBACK_SLOTS: u64 = 168;

/// A scheduled job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Job {
    /// T1 integration, hourly.
    T1,
    /// T2 campaign, nightly.
    T2,
}

impl Job {
    /// Every job, in priority order (T1 before T2).
    pub const ALL: [Self; 2] = [Self::T1, Self::T2];

    /// The name used on the command line and in record IDs.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::T1 => "t1",
            Self::T2 => "t2",
        }
    }

    /// The gate tier the job runs.
    #[must_use]
    pub const fn gate_tier(self) -> GateTier {
        match self {
            Self::T1 => GateTier::Integration,
            Self::T2 => GateTier::Campaign,
        }
    }

    /// Seconds between slots.
    #[must_use]
    pub const fn period(self) -> u64 {
        match self {
            Self::T1 => T1_PERIOD,
            Self::T2 => T2_PERIOD,
        }
    }

    /// Seconds into the UTC epoch day of the first slot of each day.
    const fn offset(self) -> u64 {
        match self {
            Self::T1 => 0,
            Self::T2 => T2_OFFSET,
        }
    }

    /// The tier's initial seed target (§10.10), which is sized to fit and never a floor.
    #[must_use]
    pub const fn seed_target(self) -> u64 {
        match self {
            Self::T1 => 2_000,
            Self::T2 => 10_000,
        }
    }

    /// The `OnCalendar=` expression of the job's timer. T1 retries every ten minutes, so a job
    /// that had to wait for a T0 commit or a campaign starts once the machine is free; the
    /// run does nothing when no slot is owed.
    const fn on_calendar(self) -> &'static str {
        match self {
            Self::T1 => "*-*-* *:00/10:00 UTC",
            Self::T2 => "*-*-* 02:00:00 UTC",
        }
    }
}

/// The latest slot at or before `now`, as seconds since the epoch.
#[must_use]
pub const fn latest_slot(job: Job, now: u64) -> Option<u64> {
    let offset = job.offset();
    if now < offset {
        return None;
    }
    Some(now - (now - offset) % job.period())
}

/// The slots among the [`LOOKBACK_SLOTS`] before the latest one that no record covers,
/// oldest first. These are **missed**: the machine was not there to run them.
#[must_use]
pub fn missed(job: Job, now: u64, recorded: &BTreeSet<u64>) -> Vec<u64> {
    let Some(latest) = latest_slot(job, now) else {
        return Vec::new();
    };
    let mut slots: Vec<u64> = (1..=LOOKBACK_SLOTS)
        .filter_map(|back| latest.checked_sub(back * job.period()))
        .filter(|slot| *slot >= job.offset() && !recorded.contains(slot))
        .collect();
    slots.sort_unstable();
    slots
}

/// What became of one scheduled run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Waiting for a higher-priority job; it starts on a later invocation.
    Queued,
    /// Holds the campaign lock.
    Running,
    /// The gate ran to the end and passed.
    Passed,
    /// The gate ran to the end and failed. The failure outbox (W46.3) takes it from here.
    Failed,
    /// The run was interrupted. Never a pass (P10).
    Incomplete,
}

/// What a job had to wait behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Blocker {
    /// A T0 commit gate holds the commit lock.
    T0Commit,
    /// Another campaign is running; only one runs at a time.
    CampaignRunning,
    /// T1 is due and goes before T2.
    T1Due,
}

/// How far a campaign got, in seeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Seeds {
    /// The seed target of the tier.
    pub target: u64,
    /// Fresh seeds the run completed; `None` when it measured none.
    pub completed: Option<u64>,
    /// Seeds still owed. For an interrupted run with nothing measured, all of `target`.
    pub remaining: Option<u64>,
}

impl Seeds {
    /// The seeds of a run that has not measured anything.
    #[must_use]
    pub const fn unmeasured(target: u64) -> Self {
        Self {
            target,
            completed: None,
            remaining: None,
        }
    }
}

/// One scheduled run, as the ledger stores it.
///
/// Invariants: `id` is [`record_id`] of `job`, `slot` and `attempt`; `attempt` starts at 1;
/// a record with `state` `Passed` or `Failed` has `finished_at`; `resumes` names an
/// `Incomplete` record of the same job and slot with attempt `attempt - 1`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// `<job>-<slot>-a<attempt>`.
    pub id: String,
    /// The job.
    pub job: Job,
    /// The slot it serves, seconds since the epoch.
    pub slot: u64,
    /// 1 for the first run of the slot, +1 for each resume.
    pub attempt: u32,
    /// Where it stands.
    pub state: State,
    /// When it was queued or, if it never waited, started.
    pub queued_at: u64,
    /// When it started running.
    pub started_at: Option<u64>,
    /// When it ended.
    pub finished_at: Option<u64>,
    /// The incomplete record this attempt resumes.
    pub resumes: Option<String>,
    /// What it waited for, if it did.
    pub blocked_by: Option<Blocker>,
    /// The tree the gate judged.
    pub tree: Option<String>,
    /// The gate receipt.
    pub receipt: Option<String>,
    /// How far it got.
    pub seeds: Seeds,
    /// Why it ended as it did.
    pub detail: Option<String>,
}

/// The record ID of attempt `attempt` of the `slot` run of `job`.
#[must_use]
pub fn record_id(job: Job, slot: u64, attempt: u32) -> String {
    format!("{}-{slot}-a{attempt}", job.name())
}

/// A record as it stands when a campaign lock is `locked` (held by some process) or not.
/// A `Running` record with no lock holder was interrupted: its process is gone.
#[must_use]
pub fn effective(record: &Record, locked: bool) -> Record {
    if record.state != State::Running || locked {
        return record.clone();
    }
    Record {
        state: State::Incomplete,
        seeds: Seeds {
            remaining: Some(record.seeds.target),
            ..record.seeds
        },
        detail: Some(
            "interrupted: the process that ran it is gone and nothing durable was measured; \
             all seeds are still owed"
                .to_owned(),
        ),
        ..record.clone()
    }
}

/// What `job` owes at `now`, given every record of it (already [`effective`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "due", rename_all = "snake_case")]
pub enum Due {
    /// No slot has come yet.
    NotYet,
    /// The latest slot ended in a pass or a failure; nothing is owed.
    Done {
        /// The slot.
        slot: u64,
        /// The record that ended it.
        record: String,
    },
    /// The latest slot is being run.
    InProgress {
        /// The slot.
        slot: u64,
        /// The running record.
        record: String,
    },
    /// The latest slot is owed: start it, resume it or take it off the queue.
    Owed {
        /// The slot.
        slot: u64,
        /// The attempt number to run.
        attempt: u32,
        /// The incomplete record this attempt resumes.
        resumes: Option<String>,
        /// The queued record to take over, if it waited before.
        queued: Option<String>,
    },
}

/// Works out what the latest slot of `job` owes.
#[must_use]
pub fn due(job: Job, now: u64, records: &[Record]) -> Due {
    let Some(slot) = latest_slot(job, now) else {
        return Due::NotYet;
    };
    let mut mine: Vec<&Record> = records
        .iter()
        .filter(|r| r.job == job && r.slot == slot)
        .collect();
    mine.sort_by_key(|r| r.attempt);
    if let Some(ended) = mine
        .iter()
        .find(|r| r.state == State::Passed || r.state == State::Failed)
    {
        return Due::Done {
            slot,
            record: ended.id.clone(),
        };
    }
    let Some(last) = mine.last() else {
        return Due::Owed {
            slot,
            attempt: 1,
            resumes: None,
            queued: None,
        };
    };
    match last.state {
        State::Running => Due::InProgress {
            slot,
            record: last.id.clone(),
        },
        State::Queued => Due::Owed {
            slot,
            attempt: last.attempt,
            resumes: last.resumes.clone(),
            queued: Some(last.id.clone()),
        },
        State::Incomplete => Due::Owed {
            slot,
            attempt: last.attempt + 1,
            resumes: Some(last.id.clone()),
            queued: None,
        },
        State::Passed | State::Failed => Due::Done {
            slot,
            record: last.id.clone(),
        },
    }
}

/// What the machine is busy with, as seen by a job that wants to start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Busy {
    /// A T0 commit gate holds the commit lock.
    pub commit_lock_held: bool,
    /// Some process holds the campaign lock.
    pub campaign_lock_held: bool,
    /// T1 is owed (so T2 waits).
    pub t1_owed: bool,
}

/// What to do about a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Nothing is owed, or it is already being run.
    Nothing(Due),
    /// Run it now.
    Run {
        /// The slot.
        slot: u64,
        /// The attempt.
        attempt: u32,
        /// The incomplete record being resumed.
        resumes: Option<String>,
    },
    /// Wait: record it as queued.
    Queue {
        /// The slot.
        slot: u64,
        /// The attempt it will run as.
        attempt: u32,
        /// The incomplete record it will resume.
        resumes: Option<String>,
        /// What it waits behind.
        blocker: Blocker,
    },
}

/// Applies the arbitration rules of §11.12 to an owed job.
#[must_use]
pub fn decide(job: Job, owed: Due, busy: Busy) -> Decision {
    let Due::Owed {
        slot,
        attempt,
        resumes,
        queued: _,
    } = owed
    else {
        return Decision::Nothing(owed);
    };
    let blocker = if busy.commit_lock_held {
        Some(Blocker::T0Commit)
    } else if busy.campaign_lock_held {
        Some(Blocker::CampaignRunning)
    } else if job == Job::T2 && busy.t1_owed {
        Some(Blocker::T1Due)
    } else {
        None
    };
    match blocker {
        Some(blocker) => Decision::Queue {
            slot,
            attempt,
            resumes,
            blocker,
        },
        None => Decision::Run {
            slot,
            attempt,
            resumes,
        },
    }
}

/// A systemd unit file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unit {
    /// The file name, e.g. `keyval-schedule-t1.timer`.
    pub name: String,
    /// The file's text.
    pub content: String,
}

/// The service and timer units of both jobs. The service runs `./dev schedule run <job>`
/// at the lowest CPU and IO priority; the timer is `Persistent`, so a machine that slept
/// through a slot fires once on waking (the older slots stay missed).
#[must_use]
pub fn unit_files(root: &str, path_env: &str) -> Vec<Unit> {
    // `%` starts a specifier in a unit file.
    let (root, path_env) = (&root.replace('%', "%%"), &path_env.replace('%', "%%"));
    let mut units = Vec::new();
    for job in Job::ALL {
        let name = job.name();
        units.push(Unit {
            name: format!("keyval-schedule-{name}.service"),
            content: format!(
                "[Unit]\n\
                 Description=keyval {name} scheduled gate (PLAN.md section 10.10)\n\
                 \n\
                 [Service]\n\
                 Type=oneshot\n\
                 WorkingDirectory={root}\n\
                 Environment=\"PATH={path_env}\"\n\
                 ExecStart={root}/dev schedule run {name}\n\
                 Nice=19\n\
                 IOSchedulingClass=idle\n\
                 CPUSchedulingPolicy=idle\n\
                 TimeoutStartSec=infinity\n"
            ),
        });
        units.push(Unit {
            name: format!("keyval-schedule-{name}.timer"),
            content: format!(
                "[Unit]\n\
                 Description=keyval {name} gate timer\n\
                 \n\
                 [Timer]\n\
                 OnCalendar={}\n\
                 Persistent=true\n\
                 \n\
                 [Install]\n\
                 WantedBy=timers.target\n",
                job.on_calendar()
            ),
        });
    }
    units
}

/// The `systemctl --user` invocations that link and start `units` from `dir`.
#[must_use]
pub fn apply_commands(dir: &str, units: &[Unit]) -> Vec<Vec<String>> {
    let mut commands: Vec<Vec<String>> = units
        .iter()
        .map(|unit| {
            ["systemctl", "--user", "link"]
                .into_iter()
                .map(str::to_owned)
                .chain([format!("{dir}/{}", unit.name)])
                .collect()
        })
        .collect();
    let timers: Vec<String> = units
        .iter()
        .filter(|unit| unit.name.ends_with("timer"))
        .map(|unit| unit.name.clone())
        .collect();
    commands.push(
        ["systemctl", "--user", "enable", "--now"]
            .into_iter()
            .map(str::to_owned)
            .chain(timers)
            .collect(),
    );
    commands
}

#[cfg(test)]
mod tests;
