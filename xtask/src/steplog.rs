//! Structured step records for scripted flows: gates, demos, smoke runs, crash probes,
//! lint probes (the W01.6 test logging and diagnostics standard, AGENTS.md "Testing").
//!
//! Every step is logged as one JSON line (`keyval.dev.step/v1`) carrying the flow, the
//! step, what it did, what it expected, what it observed, the outcome, its duration and,
//! for a failure, the exact command that re-runs it. Records go to stderr and to the
//! flow's log file in its artifact directory. A scripted flow stops at its first failed
//! step. There is no "skipped" outcome: a step that cannot run is a failed step, so a
//! skip can never pass silently (P10). The log is bounded (CD-9): oversized fields are
//! truncated, records past the limit are dropped, and both are counted.
//!
//! This is tooling: it lives in xtask and is never compiled into a production crate.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

/// Schema identifier of a step record.
pub const SCHEMA: &str = "keyval.dev.step/v1";

/// Most records one log keeps; later ones are dropped and counted (CD-9).
pub const MAX_RECORDS: usize = 10_000;

/// Most bytes kept from one text field of a record.
pub const MAX_FIELD_BYTES: usize = 4096;

/// How a step ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// It observed what it expected.
    Passed,
    /// It observed something else, or could not run at all.
    Failed,
}

/// One step of a flow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StepRecord {
    /// Schema identifier.
    pub schema: &'static str,
    /// The flow, e.g. `gate fast` or `lint probes`.
    pub flow: String,
    /// The step's name.
    pub step: String,
    /// What the step did (its command or action).
    pub action: String,
    /// What it expected to observe.
    pub expected: String,
    /// What it observed.
    pub observed: String,
    /// How it ended.
    pub outcome: Outcome,
    /// Wall-clock duration.
    pub duration_ms: u64,
    /// The exact command that re-runs this step, if it did not pass.
    pub rerun: Option<String>,
}

/// The step's description before it runs.
#[derive(Clone, Copy, Debug)]
pub struct Step<'a> {
    /// The step's name.
    pub name: &'a str,
    /// What it does.
    pub action: &'a str,
    /// What it expects to observe.
    pub expected: &'a str,
    /// The exact command that re-runs it.
    pub rerun: &'a str,
}

/// A step that did not pass; carries its record for the caller's error message.
#[derive(Debug)]
pub struct StepFailed {
    /// The failed step's record.
    pub record: Box<StepRecord>,
}

impl std::fmt::Display for StepFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let r = &self.record;
        write!(
            f,
            "step `{}` of `{}` {:?}: expected {}; observed {}; rerun: {}",
            r.step,
            r.flow,
            r.outcome,
            r.expected,
            r.observed,
            r.rerun.as_deref().unwrap_or("-")
        )
    }
}

/// A flow's step log.
pub struct StepLog {
    flow: String,
    file: Option<(PathBuf, File)>,
    echo: bool,
    written: usize,
    dropped: usize,
}

impl StepLog {
    /// A log for `flow` that appends JSON lines to `path` (created with its parent
    /// directories) and, if `echo`, to stderr.
    pub fn new(flow: &str, path: Option<&Path>, echo: bool) -> io::Result<Self> {
        let file = match path {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                Some((path.to_path_buf(), File::create(path)?))
            }
            None => None,
        };
        Ok(Self {
            flow: flow.to_owned(),
            file,
            echo,
            written: 0,
            dropped: 0,
        })
    }

    /// Logs a step that the caller ran and timed itself: `observed` is `Ok` if it saw what
    /// `step` expected, `Err` otherwise.
    pub fn record(
        &mut self,
        step: Step<'_>,
        observed: Result<String, String>,
        elapsed: Duration,
    ) -> Result<StepRecord, StepFailed> {
        let (outcome, observed) = match observed {
            Ok(observed) => (Outcome::Passed, observed),
            Err(observed) => (Outcome::Failed, observed),
        };
        self.finish(step, outcome, &observed, elapsed)
    }

    fn finish(
        &mut self,
        step: Step<'_>,
        outcome: Outcome,
        observed: &str,
        elapsed: Duration,
    ) -> Result<StepRecord, StepFailed> {
        let record = StepRecord {
            schema: SCHEMA,
            flow: self.flow.clone(),
            step: bounded(step.name),
            action: bounded(step.action),
            expected: bounded(step.expected),
            observed: bounded(observed),
            outcome,
            duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            rerun: (outcome != Outcome::Passed).then(|| bounded(step.rerun)),
        };
        self.write(&record);
        match outcome {
            Outcome::Passed => Ok(record),
            Outcome::Failed => Err(StepFailed {
                record: Box::new(record),
            }),
        }
    }

    /// Logging must never fail the flow it observes, so write errors are counted with the
    /// dropped records rather than returned.
    fn write(&mut self, record: &StepRecord) {
        if self.written >= MAX_RECORDS {
            self.dropped += 1;
            return;
        }
        let Ok(line) = serde_json::to_string(record) else {
            self.dropped += 1;
            return;
        };
        if self.echo {
            let _ = writeln!(io::stderr().lock(), "{line}");
        }
        if let Some((_, file)) = &mut self.file
            && writeln!(file, "{line}").is_err()
        {
            self.dropped += 1;
            return;
        }
        self.written += 1;
    }

    /// Records written so far.
    pub const fn written(&self) -> usize {
        self.written
    }

    /// Records dropped by the bound or by write errors.
    pub const fn dropped(&self) -> usize {
        self.dropped
    }

    /// The log file, if any.
    pub fn path(&self) -> Option<&Path> {
        self.file.as_ref().map(|(path, _)| path.as_path())
    }
}

/// `text`, cut to at most [`MAX_FIELD_BYTES`] at a character boundary, with the cut
/// counted in a suffix.
fn bounded(text: &str) -> String {
    if text.len() <= MAX_FIELD_BYTES {
        return text.to_owned();
    }
    let mut cut = MAX_FIELD_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let kept = text.get(..cut).unwrap_or_default();
    format!("{kept}[… {} more bytes dropped]", text.len() - cut)
}

#[cfg(test)]
mod tests {
    // check: DEV-STEPLOG
    use super::*;
    use crate::test_support::scratch_dir;

    fn step(name: &str) -> Step<'_> {
        Step {
            name,
            action: "add 1 + 1",
            expected: "2",
            rerun: "./dev check DEV-STEPLOG --case steplog",
        }
    }

    fn lines(path: &Path) -> Vec<serde_json::Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn a_passing_step_is_logged_without_a_rerun_command() {
        let path = scratch_dir("steplog").join("flow.jsonl");
        let mut log = StepLog::new("demo", Some(&path), false).unwrap();
        let record = log
            .record(step("add"), Ok("2".to_owned()), Duration::ZERO)
            .unwrap();
        assert_eq!(record.outcome, Outcome::Passed);
        assert_eq!(record.rerun, None);
        let written = lines(&path);
        assert_eq!(written.len(), 1, "log={written:#?}");
        assert_eq!(written[0]["schema"], SCHEMA);
        assert_eq!(written[0]["flow"], "demo");
    }

    #[test]
    fn a_failing_step_records_expected_observed_and_the_rerun_command() {
        let path = scratch_dir("steplog").join("flow.jsonl");
        let mut log = StepLog::new("demo", Some(&path), false).unwrap();
        let failure = log
            .record(step("add"), Err("3".to_owned()), Duration::ZERO)
            .unwrap_err();
        let record = &failure.record;
        assert_eq!(
            (record.expected.as_str(), record.observed.as_str()),
            ("2", "3")
        );
        assert_eq!(
            record.rerun.as_deref(),
            Some("./dev check DEV-STEPLOG --case steplog")
        );
        let message = failure.to_string();
        assert!(
            message.contains("expected 2; observed 3") && message.contains("rerun: ./dev"),
            "rule: a failure names expected, observed and the re-run command; message={message}"
        );
        assert_eq!(lines(&path)[0]["outcome"], "failed");
    }

    #[test]
    fn a_step_that_cannot_run_is_a_failure() {
        let mut log = StepLog::new("demo", None, false).unwrap();
        let failure = log
            .record(
                step("cluster up"),
                Err("not run: port 7001 in use".to_owned()),
                Duration::ZERO,
            )
            .unwrap_err();
        assert_eq!(
            failure.record.outcome,
            Outcome::Failed,
            "rule: a step that could not run is failed, never skipped (P10)"
        );
        assert!(
            failure.record.rerun.is_some(),
            "record={:?}",
            failure.record
        );
        assert_eq!(log.written(), 1, "rule: it is logged, never silent");
    }

    #[test]
    fn a_caller_timed_step_keeps_its_duration() {
        let mut log = StepLog::new("demo", None, false).unwrap();
        let record = log
            .record(step("add"), Ok("2".to_owned()), Duration::from_millis(1500))
            .unwrap();
        assert_eq!(record.duration_ms, 1500, "record={record:?}");
    }

    #[test]
    fn oversized_fields_are_truncated_and_counted() {
        let huge = "é".repeat(MAX_FIELD_BYTES);
        let kept = bounded(&huge);
        assert!(kept.len() < huge.len(), "len={}", kept.len());
        assert!(
            kept.ends_with(&format!(
                "[… {} more bytes dropped]",
                huge.len() - MAX_FIELD_BYTES
            )),
            "suffix={:?}",
            kept.get(kept.len() - 40..)
        );
        assert_eq!(bounded("short"), "short");
    }

    #[test]
    fn records_past_the_limit_are_dropped_and_counted() {
        let mut log = StepLog::new("demo", None, false).unwrap();
        log.written = MAX_RECORDS;
        let _ = log.record(step("one more"), Ok("2".to_owned()), Duration::ZERO);
        assert_eq!((log.written(), log.dropped()), (MAX_RECORDS, 1));
    }
}
