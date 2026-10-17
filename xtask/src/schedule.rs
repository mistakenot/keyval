//! `./dev schedule install|status|run` (PLAN.md §10.10 "Scheduled gates", §11.12; W46.2): the
//! local hourly T1 and nightly T2 timers, their ledger, and the arbitration between jobs.
//!
//! The pure rules (slots, missed runs, resumption, arbitration, unit text) are in
//! [`crate::schedule_plan`]. This module owns the facts they are given and the effects of
//! their answers:
//!
//! * **Ledger.** One JSON file per run under `.local/schedule/records/`, written by rename,
//!   so a crash leaves the previous version of a record or none. A record is never deleted
//!   here; `./dev prune` owns `.local/` retention (Q9).
//! * **Campaign lock.** `.local/schedule/campaign.lock`, an `flock` held for the whole run:
//!   at most one campaign runs at a time, and the lock disappears with a process that dies,
//!   so a `running` record with no holder is read as `incomplete`, never as a pass (P10).
//! * **T0 first.** A job does not start while the commit lock is held (a T0 gate is
//!   running); it is recorded as `queued` instead. A commit that begins after a job has
//!   started is not blocked: the job runs at the lowest CPU priority instead.
//! * **The run** is `nice -n 19 ./dev --json gate t1|t2`, which makes its own run tree.

use std::fs::{self, File, TryLockError};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::clock::Utc;
use crate::commit::LOCK_FILE;
use crate::output::Report;
use crate::process;
use crate::schedule_plan::{
    self as plan, Busy, Decision, Due, Job, Record, Seeds, State, apply_commands, decide, due,
    effective, missed, record_id, unit_files,
};

/// Schema identifier of the `--json` report.
pub const SCHEMA: &str = "keyval.dev.schedule/v1";

/// The scheduler's directory, relative to the checkout root.
pub const DIR: &str = ".local/schedule";

/// Tries of the campaign lock before another campaign is taken to hold it.
const PROBE_RETRIES: u32 = 4;
const RECORDS: &str = "records";
const CAMPAIGN_LOCK: &str = "campaign.lock";
/// Records shown per job in a report (CD-9: a bound on the output).
const RECENT: usize = 10;
/// Most records read from the ledger (CD-9).
const MAX_RECORDS: usize = 4_096;
/// How long one scheduled run may take; a bound on a hung process, never a budget (§10.11).
const RUN_LIMIT: Duration = Duration::from_hours(24);
const SYSTEMCTL_LIMIT: Duration = Duration::from_secs(60);

/// What `./dev schedule` was asked to do.
#[derive(Clone, Debug)]
pub enum Action {
    /// Write the systemd units, and with `apply` link and enable them.
    Install {
        /// Link and enable the units with `systemctl --user`.
        apply: bool,
        /// Where to write the units; default `~/.config/systemd/user`.
        dir: Option<PathBuf>,
    },
    /// Report what is due, running, queued, incomplete and missed.
    Status,
    /// Run a job if it is owed and the machine allows (what the timers invoke).
    Run(Job),
}

/// A job's standing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JobStatus {
    /// The job.
    pub job: Job,
    /// The latest slot, seconds since the epoch.
    pub latest_slot: Option<u64>,
    /// What the latest slot owes.
    pub due: Due,
    /// Slots that went by with no run: missed, never passed.
    pub missed: Vec<u64>,
    /// Its newest records.
    pub recent: Vec<Record>,
}

/// What `install` did.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Install {
    /// Where the units were written.
    pub dir: String,
    /// The unit files written.
    pub units: Vec<String>,
    /// Whether they were linked and enabled.
    pub applied: bool,
    /// The `systemctl` invocations `--apply` makes (or would make).
    pub commands: Vec<Vec<String>>,
}

/// What `run` did.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ran {
    /// `ran`, `queued` or `nothing`.
    pub outcome: String,
    /// The record written, if any.
    pub record: Option<Record>,
}

/// The `--json` report (`keyval.dev.schedule/v1`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScheduleReport {
    /// Schema identifier.
    pub schema: String,
    /// False when the action failed, or a run did not pass.
    pub ok: bool,
    /// `install`, `status` or `run`.
    pub action: String,
    /// The time the report was made at, seconds since the epoch.
    pub now: u64,
    /// RFC 3339 of `now`.
    pub at: String,
    /// Every job's standing.
    pub jobs: Vec<JobStatus>,
    /// Present for `install`.
    pub install: Option<Install>,
    /// Present for `run`.
    pub run: Option<Ran>,
    /// Why `ok` is false, if it is.
    pub error: Option<String>,
}

impl Report for ScheduleReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!("schedule {} at {}\n", self.action, self.at);
        for status in &self.jobs {
            let slot = status
                .latest_slot
                .map_or_else(|| "none yet".to_owned(), |s| Utc::from_unix(s).rfc3339());
            let _ = writeln!(
                out,
                "  {}: latest slot {slot}; {}; {} missed",
                status.job.name(),
                describe(&status.due),
                status.missed.len()
            );
            for record in &status.recent {
                let seeds = record.seeds.completed.map_or_else(
                    || format!("seeds unmeasured of {}", record.seeds.target),
                    |done| format!("{done}/{} seeds", record.seeds.target),
                );
                let _ = writeln!(out, "    {} {:?} ({seeds})", record.id, record.state);
            }
        }
        if let Some(install) = &self.install {
            let _ = writeln!(
                out,
                "  units written to {}{}",
                install.dir,
                if install.applied {
                    " and enabled"
                } else {
                    " (not enabled: pass --apply)"
                }
            );
        }
        if let Some(run) = &self.run {
            let _ = writeln!(out, "  run: {}", run.outcome);
        }
        if let Some(error) = &self.error {
            let _ = writeln!(out, "  error: {error}");
        }
        out
    }
}

fn describe(due: &Due) -> String {
    match due {
        Due::NotYet => "no slot yet".to_owned(),
        Due::Done { record, .. } => format!("done ({record})"),
        Due::InProgress { record, .. } => format!("running ({record})"),
        Due::Owed {
            attempt,
            resumes,
            queued,
            ..
        } => {
            let how = match (resumes, queued) {
                (Some(prior), _) => format!("owed, resumes {prior}"),
                (None, Some(_)) => "owed, queued".to_owned(),
                (None, None) => "owed".to_owned(),
            };
            format!("{how} as attempt {attempt}")
        }
    }
}

/// The current time, seconds since the epoch. A clock before 1970 reads as 0.
#[must_use]
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Whether some process holds the `flock` on `path`. The probe takes and drops the lock.
fn lock_held(path: &Path) -> Result<bool, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| format!("{}: {err}", parent.display()))?;
    }
    let file = open_lock(path)?;
    // A shared probe never excludes another probe, and the holder's exclusive lock still
    // shows through it.
    match file.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(err)) => Err(format!("{}: {err}", path.display())),
    }
}

fn open_lock(path: &Path) -> Result<File, String> {
    File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|err| format!("{}: {err}", path.display()))
}

/// Takes the campaign lock without waiting: `None` when another campaign holds it.
fn take_campaign_lock(dir: &Path) -> Result<Option<File>, String> {
    fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let path = dir.join(CAMPAIGN_LOCK);
    let file = open_lock(&path)?;
    // A status probe holds the lock for an instant; wait that out before concluding that a
    // campaign is running.
    for _ in 0..PROBE_RETRIES {
        match file.try_lock() {
            Ok(()) => return Ok(Some(file)),
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(50)),
            Err(TryLockError::Error(err)) => return Err(format!("{}: {err}", path.display())),
        }
    }
    Ok(None)
}

/// Reads the ledger.
///
/// # Errors
///
/// An unreadable directory, a record that does not parse, or more records than the bound.
pub fn load(dir: &Path) -> Result<Vec<Record>, String> {
    let records = dir.join(RECORDS);
    if !records.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&records).map_err(|err| format!("{}: {err}", records.display()))? {
        let path = entry
            .map_err(|err| format!("{}: {err}", records.display()))?
            .path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let text = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        let Ok(record) = serde_json::from_str::<Record>(&text) else {
            // A record that does not parse must not stop scheduling for good: set it aside
            // (`.bad`, never deleted) and say so where the timer's journal shows it.
            let bad = path.with_extension("bad");
            let _ = writeln!(
                std::io::stderr().lock(),
                "schedule: {} is not a schedule record; moved to {}",
                path.display(),
                bad.display()
            );
            fs::rename(&path, &bad).map_err(|err| format!("{}: {err}", path.display()))?;
            continue;
        };
        out.push(record);
    }
    out.sort_by_key(|r| (r.slot, r.attempt, r.job));
    Ok(keep_newest(out, MAX_RECORDS))
}

/// The newest `max` records. Only the latest slots matter to the scheduler, so a ledger
/// that outgrows the bound (CD-9) loses its oldest history from view, never its ability to run.
#[must_use]
pub fn keep_newest(mut records: Vec<Record>, max: usize) -> Vec<Record> {
    let excess = records.len().saturating_sub(max);
    records.drain(..excess);
    records
}

/// Writes `record` by rename.
///
/// # Errors
///
/// A directory or file that cannot be written.
pub fn save(dir: &Path, record: &Record) -> Result<(), String> {
    let records = dir.join(RECORDS);
    fs::create_dir_all(&records).map_err(|err| format!("{}: {err}", records.display()))?;
    let (tmp, last) = (
        records.join(format!("{}.tmp", record.id)),
        records.join(format!("{}.json", record.id)),
    );
    let text = serde_json::to_string_pretty(record).map_err(|err| err.to_string())?;
    let mut file = File::create(&tmp).map_err(|err| format!("{}: {err}", tmp.display()))?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|err| format!("{}: {err}", tmp.display()))?;
    fs::rename(&tmp, &last).map_err(|err| format!("{}: {err}", last.display()))
}

/// The ledger with every record read as it stands: `running` with no lock holder is
/// `incomplete`.
fn effective_records(dir: &Path) -> Result<Vec<Record>, String> {
    let locked = lock_held(&dir.join(CAMPAIGN_LOCK))?;
    Ok(load(dir)?.iter().map(|r| effective(r, locked)).collect())
}

fn job_status(job: Job, now: u64, records: &[Record]) -> JobStatus {
    let mine: Vec<&Record> = records.iter().filter(|r| r.job == job).collect();
    // A slot that was queued and never run is missed, not covered.
    let recorded = mine
        .iter()
        .filter(|r| r.state != State::Queued)
        .map(|r| r.slot)
        .collect();
    let skip = mine.len().saturating_sub(RECENT);
    JobStatus {
        job,
        latest_slot: plan::latest_slot(job, now),
        due: due(job, now, records),
        missed: missed(job, now, &recorded),
        recent: mine.into_iter().skip(skip).cloned().collect(),
    }
}

fn blank(action: &str, now: u64) -> ScheduleReport {
    ScheduleReport {
        schema: SCHEMA.to_owned(),
        ok: true,
        action: action.to_owned(),
        now,
        at: Utc::from_unix(now).rfc3339(),
        jobs: Vec::new(),
        install: None,
        run: None,
        error: None,
    }
}

fn failed(mut report: ScheduleReport, why: String) -> ScheduleReport {
    report.ok = false;
    report.error = Some(why);
    report
}

/// Runs `action` against the checkout at `root`.
#[must_use]
pub fn execute(root: &Path, action: &Action, now: u64) -> ScheduleReport {
    let dir = root.join(DIR);
    let report = blank(
        match action {
            Action::Install { .. } => "install",
            Action::Status => "status",
            Action::Run(_) => "run",
        },
        now,
    );
    let report = match action {
        Action::Install { apply, dir: units } => install(root, *apply, units.as_deref(), report),
        Action::Status | Action::Run(_) => report,
    };
    if !report.ok {
        return report;
    }
    let report = match action {
        Action::Run(job) => drain(root, &dir, *job, now, report),
        Action::Install { .. } | Action::Status => report,
    };
    match effective_records(&dir) {
        Ok(records) => ScheduleReport {
            jobs: Job::ALL.map(|job| job_status(job, now, &records)).into(),
            ..report
        },
        Err(why) => failed(report, why),
    }
}

fn install(
    root: &Path,
    apply: bool,
    units: Option<&Path>,
    mut report: ScheduleReport,
) -> ScheduleReport {
    let dir = match units {
        Some(dir) => dir.to_path_buf(),
        None => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".config/systemd/user"),
            None => return failed(report, "HOME is not set; pass --units-dir".to_owned()),
        },
    };
    let path_env = std::env::var("PATH").unwrap_or_default();
    let files = unit_files(&root.display().to_string(), &path_env);
    if let Err(err) = fs::create_dir_all(&dir) {
        return failed(report, format!("{}: {err}", dir.display()));
    }
    for unit in &files {
        let path = dir.join(&unit.name);
        if let Err(err) = fs::write(&path, &unit.content) {
            return failed(report, format!("{}: {err}", path.display()));
        }
    }
    let commands = apply_commands(&dir.display().to_string(), &files);
    let error = if apply {
        commands.iter().find_map(|argv| systemctl(argv).err())
    } else {
        None
    };
    report.install = Some(Install {
        dir: dir.display().to_string(),
        units: files.iter().map(|u| u.name.clone()).collect(),
        applied: apply && error.is_none(),
        commands,
    });
    error.map_or_else(|| report.clone(), |why| failed(report.clone(), why))
}

fn systemctl(argv: &[String]) -> Result<(), String> {
    let rest = match argv.split_first() {
        Some((first, rest)) if first == "systemctl" => rest,
        Some(_) | None => return Err(format!("not a systemctl command: {argv:?}")),
    };
    let mut cmd = Command::new("systemctl");
    cmd.args(rest);
    let out = process::run(&mut cmd, SYSTEMCTL_LIMIT).map_err(|err| format!("systemctl: {err}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "`{}` failed: {}",
            argv.join(" "),
            out.stderr.trim()
        ))
    }
}

/// Runs `first`, and when it ran, whatever else is owed, T1 before T2: a job that had to
/// wait is started as soon as the machine is free, not at its next slot (§11.12).
fn drain(root: &Path, dir: &Path, first: Job, now: u64, report: ScheduleReport) -> ScheduleReport {
    let mut report = run_job(root, dir, first, now, report);
    let mut next = Job::ALL.into_iter().filter(|job| *job != first);
    while report.run.as_ref().is_some_and(|ran| ran.outcome == "ran") {
        let Some(job) = next.next() else { break };
        let later = run_job(
            root,
            dir,
            job,
            now,
            ScheduleReport {
                run: None,
                error: None,
                ok: true,
                ..report.clone()
            },
        );
        // The report shows the job that was asked for; a later job only adds its failure.
        if later
            .run
            .as_ref()
            .is_some_and(|ran| ran.outcome != "nothing")
        {
            report.jobs = later.jobs;
            if !later.ok {
                report.ok = false;
                report.error = report.error.or(later.error);
            }
        }
    }
    report
}

fn run_job(
    root: &Path,
    dir: &Path,
    job: Job,
    now: u64,
    mut report: ScheduleReport,
) -> ScheduleReport {
    let outcome = start(root, dir, job, now);
    match outcome {
        Ok((label, record)) => {
            if let Some(record) = &record {
                report.ok = record.state != State::Failed && record.state != State::Incomplete;
                if !report.ok {
                    report.error.clone_from(&record.detail);
                }
            }
            report.run = Some(Ran {
                outcome: label.to_owned(),
                record,
            });
            report
        }
        Err(why) => failed(report, why),
    }
}

/// Decides and acts. Returns the outcome label and the record written.
fn start(
    root: &Path,
    dir: &Path,
    job: Job,
    now: u64,
) -> Result<(&'static str, Option<Record>), String> {
    let Some(lock) = take_campaign_lock(dir)? else {
        // Another campaign is running. Queue behind it (and say so) if we owe a run.
        let records = effective_records(dir)?;
        let busy = Busy {
            commit_lock_held: false,
            campaign_lock_held: true,
            t1_owed: false,
        };
        return finish_decision(dir, job, now, decide(job, due(job, now, &records), busy));
    };
    // We hold the lock, so any `running` record is a dead process's.
    let mut records = load(dir)?;
    for record in &mut records {
        if record.state == State::Running {
            *record = effective(record, false);
            save(dir, record)?;
        }
    }
    let owed = due(job, now, &records);
    let busy = Busy {
        commit_lock_held: lock_held(&root.join(LOCK_FILE))?,
        campaign_lock_held: false,
        t1_owed: matches!(due(Job::T1, now, &records), Due::Owed { .. }),
    };
    match decide(job, owed, busy) {
        Decision::Run {
            slot,
            attempt,
            resumes,
        } => {
            let record = run_campaign(root, dir, job, slot, attempt, resumes, now)?;
            drop(lock);
            let label = if record.state == State::Queued {
                "queued"
            } else {
                "ran"
            };
            Ok((label, Some(record)))
        }
        other => {
            drop(lock);
            finish_decision(dir, job, now, other)
        }
    }
}

fn finish_decision(
    dir: &Path,
    job: Job,
    now: u64,
    decision: Decision,
) -> Result<(&'static str, Option<Record>), String> {
    match decision {
        Decision::Nothing(_) => Ok(("nothing", None)),
        Decision::Queue {
            slot,
            attempt,
            resumes,
            blocker,
        } => {
            let id = record_id(job, slot, attempt);
            let prior = load(dir)?.into_iter().find(|r| r.id == id);
            if let Some(started) = prior.as_ref().filter(|r| r.state != State::Queued) {
                // Another process moved it on since this decision was made; leave it be.
                return Ok(("nothing", Some(started.clone())));
            }
            let record = Record {
                blocked_by: Some(blocker),
                state: State::Queued,
                resumes,
                ..prior.unwrap_or_else(|| new_record(job, slot, attempt, now))
            };
            save(dir, &record)?;
            Ok(("queued", Some(record)))
        }
        Decision::Run { .. } => Err("a run needs the campaign lock".to_owned()),
    }
}

pub fn new_record(job: Job, slot: u64, attempt: u32, now: u64) -> Record {
    Record {
        id: record_id(job, slot, attempt),
        job,
        slot,
        attempt,
        state: State::Queued,
        queued_at: now,
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

/// Runs the gate for one slot, with the campaign lock held by the caller.
fn run_campaign(
    root: &Path,
    dir: &Path,
    job: Job,
    slot: u64,
    attempt: u32,
    resumes: Option<String>,
    now: u64,
) -> Result<Record, String> {
    let id = record_id(job, slot, attempt);
    let queued = load(dir)?.into_iter().find(|r| r.id == id);
    let mut record = Record {
        state: State::Running,
        started_at: Some(now),
        resumes,
        detail: None,
        blocked_by: queued.as_ref().and_then(|q| q.blocked_by),
        ..queued.unwrap_or_else(|| new_record(job, slot, attempt, now))
    };
    save(dir, &record)?;
    let mut cmd = Command::new("nice");
    cmd.args(["-n", "19"])
        .arg(root.join("dev"))
        .args(["--json", "gate", crate::gate::tier_name(job.gate_tier())])
        .current_dir(root);
    let finished = match process::run(&mut cmd, RUN_LIMIT) {
        Ok(out) => judge(&out.stdout, out.status.success(), &out.stderr),
        Err(err) => Judged::incomplete(format!("the gate could not run to the end: {err}")),
    };
    record.finished_at = Some(now_unix());
    finished.apply(&mut record, job.seed_target());
    if record.state == State::Queued {
        record.started_at = None;
        record.finished_at = None;
    }
    save(dir, &record)?;
    Ok(record)
}

/// What the gate's output says about a run.
struct Judged {
    state: State,
    tree: Option<String>,
    receipt: Option<String>,
    fresh_seeds: Option<u64>,
    detail: Option<String>,
}

impl Judged {
    const fn incomplete(detail: String) -> Self {
        Self {
            state: State::Incomplete,
            tree: None,
            receipt: None,
            fresh_seeds: None,
            detail: Some(detail),
        }
    }

    fn apply(self, record: &mut Record, target: u64) {
        record.state = self.state;
        record.tree = self.tree;
        record.receipt = self.receipt;
        record.detail = self.detail;
        record.seeds = Seeds {
            target,
            completed: self.fresh_seeds,
            remaining: match (self.state, self.fresh_seeds) {
                (State::Passed, done) => done.map(|_| 0),
                (_, Some(done)) => Some(target.saturating_sub(done)),
                (_, None) => (self.state == State::Incomplete).then_some(target),
            },
        };
    }
}

/// Reads the gate's `--json` report. No receipt means the gate refused or died before it
/// judged anything: the run is incomplete, not failed and never passed.
fn judge(stdout: &str, exited_ok: bool, stderr: &str) -> Judged {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout) else {
        return Judged::incomplete(format!(
            "the gate printed no report: {}",
            stderr.lines().last().unwrap_or("")
        ));
    };
    let Some(receipt) = value
        .get("receipt_path")
        .and_then(serde_json::Value::as_str)
    else {
        let why = value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no receipt");
        // The gate refused before judging anything (for example a dirty checkout). The slot
        // stays owed and queued, with the reason, rather than burning an attempt per retry.
        return Judged {
            state: State::Queued,
            detail: Some(format!("the gate refused to run: {why}")),
            ..Judged::incomplete(String::new())
        };
    };
    let ok = value.get("ok").and_then(serde_json::Value::as_bool) == Some(true) && exited_ok;
    Judged {
        state: if ok { State::Passed } else { State::Failed },
        tree: value
            .pointer("/receipt/tree/id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        receipt: Some(receipt.to_owned()),
        fresh_seeds: value
            .pointer("/receipt/tier_plan/measured/fresh_seeds")
            .and_then(serde_json::Value::as_u64),
        detail: (!ok).then(|| "the gate ran to the end and did not pass".to_owned()),
    }
}

#[cfg(test)]
mod tests;
