//! Checker K-14, replay identity (PLAN.md §10.5, §10.7; T-3, T-11, P4): the same binary
//! running the same seed in fresh OS processes gives byte-identical canonical traces.
//!
//! [`fresh_processes`] runs every seed in several fresh processes, started by a launcher
//! the caller supplies, and each process writes its run's canonical trace to a file. Each
//! process's trace is compared with the first process's trace for the same seed: the
//! event counts and the complete canonical bytes, never a hash or the final state alone.
//! A difference is reported with its first differing line, the last event step both
//! traces share before it, and both trace files, which are kept. Any difference fails
//! (RISK-6).
//!
//! A replay that would compare nothing is refused, never passed (P10): no seeds, a
//! repeated seed, fewer than two processes, a trace with no events, or a trace marked
//! incomplete ([`INCOMPLETE_MARKER`](crate::trace::INCOMPLETE_MARKER)). Each process runs
//! with its stdout discarded and its stderr kept in a file beside its trace, of which a
//! report keeps only the tail. A process still running at its timeout is killed and fails
//! the replay. The timeout supervises processes; it never reaches a run (P4).
//!
//! K-14 lives in `sim`, an explicit, stated exemption from T-12 (§10.5): it compares
//! canonical traces byte for byte and consults no expected-result oracle.
//!
//! Until the five-node slice (W17) gives K-14 a cluster to replay, the replayed run is
//! [`machinery`]: the scheduler, the entropy streams and the trace working together,
//! without Raft. Five nodes' election timers fire on delays drawn from each node
//! incarnation's own stream. The delays sit on a coarse grid, so timers often fire at the
//! same virtual time and the seeded tie priority orders them. The faults stream restarts
//! nodes, and a timer armed by an earlier incarnation still fires after the restart, as
//! a stale event that is ignored.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use types::{Entropy as _, EntropyStream, Incarnation, NodeId};

use crate::entropy::{MasterSeed, NodeStreams, Subsystem};
use crate::scheduler::{
    EventBudget, NodeIncarnation, ScheduleError, Scheduler, Shortfall, Step, VirtualTime,
};
use crate::trace::{Comparison, Name, ObservationBound, Trace, compare, marks_incomplete};

/// The machinery run's nodes: the five voters (§1).
const NODES: u64 = 5;

/// The delay grid. Every delay is a whole number of grid steps, so events often fall at
/// the same virtual time.
const GRID_MICROS: u64 = 100;

/// A timer's delay: 1 to `TIMER_STEPS` grid steps.
const TIMER_STEPS: u64 = 8;

/// One live timeout in `RESTART_ODDS` restarts its node.
const RESTART_ODDS: u64 = 16;

/// A restart lands 1 to `RESTART_STEPS` grid steps after the timeout that caused it.
const RESTART_STEPS: u64 = 4;

/// No timer is armed for this virtual time or later, so the run drains and completes.
const HORIZON_MICROS: u64 = 20_000;

/// The most stderr bytes a failed process's report keeps (CD-9).
const STDERR_TAIL: usize = 4096;

/// How long one process may run before it is killed and the replay fails.
pub const PROCESS_TIMEOUT: Duration = Duration::from_secs(300);

/// How often a running process is polled for its exit.
const POLL: Duration = Duration::from_millis(5);

/// What happens in the machinery run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Happening {
    /// The election timer that node `slot` armed in `incarnation` fired.
    Timeout {
        /// The node's slot in the run.
        slot: usize,
        /// The incarnation that armed it.
        incarnation: Incarnation,
    },
    /// Node `slot` restarts as its next incarnation.
    Restart {
        /// The node's slot in the run.
        slot: usize,
    },
}

/// A node of the machinery run.
#[derive(Debug)]
struct Node {
    id: NodeId,
    incarnation: Incarnation,
    streams: NodeStreams,
    restarting: bool,
}

impl Node {
    const fn on(&self) -> NodeIncarnation {
        NodeIncarnation {
            node: self.id,
            incarnation: self.incarnation,
        }
    }
}

/// A finished machinery run.
#[derive(Clone, Debug)]
pub struct MachineryRun {
    /// Its canonical trace.
    pub trace: Trace,
    /// Events dispatched.
    pub events: u64,
    /// Node restarts.
    pub restarts: u64,
    /// Timeouts armed by an earlier incarnation, fired after its node restarted.
    pub stale_timeouts: u64,
    /// Events dispatched at the same virtual time as the event before them, so ordered by
    /// the seeded tie priority.
    pub same_time: u64,
}

/// Why a run has no complete trace. It is never a pass (P10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunError {
    /// One of the run's constants was rejected by its domain type.
    Setup(&'static str),
    /// The scheduler refused an event.
    Schedule(ScheduleError),
    /// The run hit a bound before it finished.
    Incomplete {
        /// Events dispatched.
        events: u64,
        /// Events still queued.
        pending: usize,
        /// Which bound it hit.
        reason: Shortfall,
    },
    /// A required observation overflowed its bound.
    Unobserved,
}

impl From<ScheduleError> for RunError {
    fn from(err: ScheduleError) -> Self {
        Self::Schedule(err)
    }
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Setup(what) => write!(f, "the run's constant {what} is invalid"),
            Self::Schedule(err) => write!(f, "{err}"),
            Self::Incomplete {
                events,
                pending,
                reason,
            } => write!(
                f,
                "the run is incomplete ({reason:?}) after {events} events with {pending} pending"
            ),
            Self::Unobserved => write!(f, "a required observation overflowed its bound"),
        }
    }
}

impl Error for RunError {}

/// Runs the machinery scenario for `seed`.
///
/// # Errors
///
/// [`RunError`] if the run cannot finish within its bounds. It then has no complete
/// trace.
pub fn machinery(seed: MasterSeed) -> Result<MachineryRun, RunError> {
    run(seed, |_, delay| delay)
}

/// [`machinery`], with every timer delay passed through `skew`. Every real run passes
/// the identity; only K-14's negative control skews a draw.
fn run(
    seed: MasterSeed,
    mut skew: impl FnMut(NodeIncarnation, u64) -> u64,
) -> Result<MachineryRun, RunError> {
    let timeout = name("timeout")?;
    let restart = name("restart")?;
    let mut scheduler = Scheduler::new(seed.stream(Subsystem::Scheduler), EventBudget::DEFAULT);
    let mut faults = seed.stream(Subsystem::Faults);
    let mut trace = Trace::new(ObservationBound::DEFAULT);
    let mut nodes = Vec::new();
    for raw in 1..=NODES {
        let id = NodeId::new(raw).map_err(|_| RunError::Setup("node ID"))?;
        nodes.push(Node {
            id,
            incarnation: Incarnation::FIRST,
            streams: seed.node(id, Incarnation::FIRST),
            restarting: false,
        });
    }
    for (slot, node) in nodes.iter_mut().enumerate() {
        arm(&mut scheduler, slot, node, &mut skew)?;
    }
    let (mut restarts, mut stale_timeouts, mut same_time) = (0, 0, 0);
    let mut last = None;
    let events = loop {
        let event = match scheduler.step() {
            Step::Event(event) => event,
            Step::Complete { events } => break events,
            Step::Incomplete {
                events,
                pending,
                reason,
            } => {
                return Err(RunError::Incomplete {
                    events,
                    pending,
                    reason,
                });
            }
        };
        if last == Some(event.time) {
            same_time += 1;
        }
        last = Some(event.time);
        match event.payload {
            Happening::Timeout { slot, incarnation } => {
                trace.event(&event, &timeout);
                let node = &mut nodes[slot];
                if incarnation != node.incarnation {
                    stale_timeouts += 1;
                    continue;
                }
                if !node.restarting && faults.next_u64().is_multiple_of(RESTART_ODDS) {
                    node.restarting = true;
                    let delay = (1 + faults.next_u64() % RESTART_STEPS) * GRID_MICROS;
                    let at = VirtualTime::from_micros(scheduler.now().as_micros() + delay);
                    scheduler.schedule(at, Some(node.on()), Happening::Restart { slot })?;
                }
                arm(&mut scheduler, slot, node, &mut skew)?;
            }
            Happening::Restart { slot } => {
                trace.event(&event, &restart);
                let node = &mut nodes[slot];
                node.incarnation = node
                    .incarnation
                    .checked_next()
                    .ok_or(RunError::Setup("incarnation"))?;
                node.streams = seed.node(node.id, node.incarnation);
                node.restarting = false;
                restarts += 1;
                arm(&mut scheduler, slot, node, &mut skew)?;
            }
        }
    };
    trace.metric(name("events")?, events);
    trace.metric(name("restarts")?, restarts);
    trace.metric(name("stale_timeouts")?, stale_timeouts);
    trace.metric(name("same_time")?, same_time);
    if !trace.is_complete() {
        return Err(RunError::Unobserved);
    }
    Ok(MachineryRun {
        trace,
        events,
        restarts,
        stale_timeouts,
        same_time,
    })
}

/// Arms `node`'s election timer on a delay drawn from its incarnation's stream, unless it
/// would fire at or after the horizon.
fn arm(
    scheduler: &mut Scheduler<Happening>,
    slot: usize,
    node: &mut Node,
    skew: &mut impl FnMut(NodeIncarnation, u64) -> u64,
) -> Result<(), RunError> {
    let steps = 1 + node.streams.next_u64(EntropyStream::ElectionTimeout) % TIMER_STEPS;
    let at = scheduler
        .now()
        .as_micros()
        .saturating_add(skew(node.on(), steps * GRID_MICROS));
    if at < HORIZON_MICROS {
        let timeout = Happening::Timeout {
            slot,
            incarnation: node.incarnation,
        };
        scheduler.schedule(VirtualTime::from_micros(at), Some(node.on()), timeout)?;
    }
    Ok(())
}

fn name(text: &'static str) -> Result<Name, RunError> {
    Name::new(text).ok_or(RunError::Setup(text))
}

/// The file the process numbered `process` writes `seed`'s canonical trace to, under
/// `out`.
#[must_use]
pub fn trace_path(out: &Path, seed: MasterSeed, process: u32) -> PathBuf {
    out.join(format!("p{process}"))
        .join(format!("seed-{}.trace", seed.get()))
}

/// A replay: every seed run by every process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Seeds run.
    pub seeds: usize,
    /// Fresh processes per seed.
    pub processes: u32,
    /// Canonical bytes compared.
    pub bytes: usize,
    /// Every trace that differs from its seed's first-process trace.
    pub divergences: Vec<Divergence>,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "K-14: {} seed(s) x {} fresh process(es), {} canonical bytes compared: ",
            self.seeds, self.processes, self.bytes
        )?;
        if self.divergences.is_empty() {
            return write!(f, "byte-identical");
        }
        write!(f, "{} divergence(s)", self.divergences.len())?;
        self.divergences
            .iter()
            .try_for_each(|divergence| write!(f, "\n  {divergence}"))
    }
}

/// A trace that differs from its seed's first-process trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergence {
    /// The seed.
    pub seed: MasterSeed,
    /// The process whose trace differs from process 0's.
    pub process: u32,
    /// Events in process 0's trace and in this one.
    pub events: (usize, usize),
    /// The first differing line (1-based).
    pub line: usize,
    /// The last event line both traces share before the difference; `None` if they
    /// differ before their first event.
    pub step: Option<String>,
    /// Process 0's line and this one's; `None` where a trace ended.
    pub lines: (Option<String>, Option<String>),
    /// Both trace files, kept for inspection.
    pub traces: (PathBuf, PathBuf),
}

impl fmt::Display for Divergence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |line: Option<&str>| {
            line.map_or_else(|| "<end of trace>".to_owned(), |line| format!("{line:?}"))
        };
        write!(
            f,
            "seed {}: process {} differs from process 0 at line {} (after `{}`): {} vs {}; \
             {} vs {} events; traces kept: {} and {}",
            self.seed.get(),
            self.process,
            self.line,
            self.step.as_deref().unwrap_or("the start"),
            show(self.lines.0.as_deref()),
            show(self.lines.1.as_deref()),
            self.events.0,
            self.events.1,
            self.traces.0.display(),
            self.traces.1.display()
        )
    }
}

/// Why a replay could not compare its traces. It is never a pass (P10).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayError {
    /// No seeds, or fewer than two processes: nothing would be compared.
    Vacuous {
        /// Seeds asked for.
        seeds: usize,
        /// Processes asked for.
        processes: u32,
    },
    /// A seed is listed twice; its processes would share one trace file.
    RepeatedSeed {
        /// The seed.
        seed: MasterSeed,
    },
    /// A trace directory, or a trace left by an earlier replay, could not be prepared.
    Prepare {
        /// The path.
        path: PathBuf,
        /// The OS error.
        error: String,
    },
    /// A process could not be started.
    Spawn {
        /// Its seed.
        seed: MasterSeed,
        /// Its number.
        process: u32,
        /// The OS error.
        error: String,
    },
    /// A process failed, so its run has no complete trace.
    Failed {
        /// Its seed.
        seed: MasterSeed,
        /// Its number.
        process: u32,
        /// Its exit status.
        status: String,
        /// The tail of its stderr.
        stderr: String,
    },
    /// A process was still running at its timeout, and was killed.
    TimedOut {
        /// Its seed.
        seed: MasterSeed,
        /// Its number.
        process: u32,
        /// The timeout.
        after: Duration,
    },
    /// A process's trace could not be read, or was never written.
    Unreadable {
        /// The trace file.
        path: PathBuf,
        /// The OS error.
        error: String,
    },
    /// A process's trace has no events: there is nothing to compare.
    NoEvents {
        /// Its seed.
        seed: MasterSeed,
        /// Its number.
        process: u32,
        /// The trace file.
        path: PathBuf,
    },
    /// A process's trace is marked incomplete: it is not the run's full evidence.
    Incomplete {
        /// Its seed.
        seed: MasterSeed,
        /// Its number.
        process: u32,
        /// The trace file.
        path: PathBuf,
    },
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vacuous { seeds, processes } => write!(
                f,
                "a replay needs at least one seed and two processes, not {seeds} and {processes}"
            ),
            Self::RepeatedSeed { seed } => {
                write!(f, "seed {} is listed twice in one replay", seed.get())
            }
            Self::Prepare { path, error } => write!(f, "prepare {}: {error}", path.display()),
            Self::Spawn {
                seed,
                process,
                error,
            } => write!(
                f,
                "process {process} for seed {} could not start: {error}",
                seed.get()
            ),
            Self::Failed {
                seed,
                process,
                status,
                stderr,
            } => write!(
                f,
                "process {process} for seed {} failed ({status}): {stderr}",
                seed.get()
            ),
            Self::TimedOut {
                seed,
                process,
                after,
            } => write!(
                f,
                "process {process} for seed {} was still running after {after:?}, and was killed",
                seed.get()
            ),
            Self::Unreadable { path, error } => {
                write!(f, "read trace {}: {error}", path.display())
            }
            Self::NoEvents {
                seed,
                process,
                path,
            } => write!(
                f,
                "process {process}'s trace for seed {} has no events ({})",
                seed.get(),
                path.display()
            ),
            Self::Incomplete {
                seed,
                process,
                path,
            } => write!(
                f,
                "process {process}'s trace for seed {} is marked incomplete ({})",
                seed.get(),
                path.display()
            ),
        }
    }
}

impl Error for ReplayError {}

/// Runs every seed in `processes` fresh processes and compares their traces (K-14).
///
/// The processes run one after another, each killed if still running after `timeout`.
/// `launch(seed, path)` builds the command that runs `seed` and writes its canonical trace
/// to `path`; its stdin and stdout are null and its stderr goes to a file beside the trace.
/// Traces go under `out` at [`trace_path`]; a trace left there by an earlier replay is
/// removed first, so a process that writes nothing is caught.
///
/// # Errors
///
/// [`ReplayError`] if nothing would be compared (no seeds, a repeated seed, one process,
/// a trace without events or marked incomplete), or a process fails, times out or leaves
/// no trace.
pub fn fresh_processes(
    seeds: &[MasterSeed],
    processes: u32,
    out: &Path,
    timeout: Duration,
    launch: impl Fn(MasterSeed, &Path) -> Command,
) -> Result<Report, ReplayError> {
    if seeds.is_empty() || processes < 2 {
        return Err(ReplayError::Vacuous {
            seeds: seeds.len(),
            processes,
        });
    }
    let mut distinct = BTreeSet::new();
    if let Some(&seed) = seeds.iter().find(|seed| !distinct.insert(seed.get())) {
        return Err(ReplayError::RepeatedSeed { seed });
    }
    for process in 0..processes {
        let dir = out.join(format!("p{process}"));
        fs::create_dir_all(&dir).map_err(|err| prepare(&dir, &err))?;
        for &seed in seeds {
            let path = trace_path(out, seed, process);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(prepare(&path, &err)),
            }
            run_process(launch(seed, &path), seed, process, &path, timeout)?;
        }
    }
    let mut bytes = 0;
    let mut divergences = Vec::new();
    for &seed in seeds {
        let first_path = trace_path(out, seed, 0);
        let first = read(&first_path)?;
        check_trace(seed, 0, &first_path, &first)?;
        bytes += first.len();
        for process in 1..processes {
            let path = trace_path(out, seed, process);
            let other = read(&path)?;
            check_trace(seed, process, &path, &other)?;
            bytes += other.len();
            match compare(&first, &other) {
                Comparison::Identical => {}
                Comparison::Differs { line, left, right } => divergences.push(Divergence {
                    seed,
                    process,
                    events: (events(&first), events(&other)),
                    line,
                    step: step_before(&first, line),
                    lines: (left, right),
                    traces: (first_path.clone(), path),
                }),
            }
        }
    }
    Ok(Report {
        seeds: seeds.len(),
        processes,
        bytes,
        divergences,
    })
}

/// Runs one process to its exit within `timeout`: stdin and stdout null, stderr in a
/// file beside `trace` (`.stderr`), of which a failure report keeps only the tail (CD-9).
fn run_process(
    mut command: Command,
    seed: MasterSeed,
    process: u32,
    trace: &Path,
    timeout: Duration,
) -> Result<(), ReplayError> {
    let stderr_path = trace.with_extension("stderr");
    let stderr = File::create(&stderr_path).map_err(|err| prepare(&stderr_path, &err))?;
    let spawn_error = |err: io::Error| ReplayError::Spawn {
        seed,
        process,
        error: err.to_string(),
    };
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .map_err(spawn_error)?;
    let deadline = Instant::now().checked_add(timeout);
    let status: ExitStatus = loop {
        match child.try_wait().map_err(spawn_error)? {
            Some(status) => break status,
            None if deadline.is_some_and(|deadline| Instant::now() >= deadline) => {
                // The kill can only fail if the process has just exited; wait reaps it.
                let _ = child.kill();
                let _ = child.wait();
                return Err(ReplayError::TimedOut {
                    seed,
                    process,
                    after: timeout,
                });
            }
            None => thread::sleep(POLL),
        }
    };
    if status.success() {
        return Ok(());
    }
    Err(ReplayError::Failed {
        seed,
        process,
        status: status.to_string(),
        stderr: tail_of(&stderr_path),
    })
}

/// The last [`STDERR_TAIL`] bytes of the file at `path`, read without loading the rest.
fn tail_of(path: &Path) -> String {
    let limit = u64::try_from(STDERR_TAIL).unwrap_or(u64::MAX);
    let read = || -> io::Result<Vec<u8>> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(limit)))?;
        let mut bytes = Vec::new();
        file.take(limit).read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    read().map_or_else(
        |err| format!("<stderr unreadable: {err}>"),
        |bytes| tail(&bytes),
    )
}

/// Refuses a trace that would make the comparison vacuous: one marked incomplete, or one
/// without events (P10).
fn check_trace(
    seed: MasterSeed,
    process: u32,
    path: &Path,
    canonical: &str,
) -> Result<(), ReplayError> {
    let path = path.to_path_buf();
    if marks_incomplete(canonical) {
        return Err(ReplayError::Incomplete {
            seed,
            process,
            path,
        });
    }
    if events(canonical) == 0 {
        return Err(ReplayError::NoEvents {
            seed,
            process,
            path,
        });
    }
    Ok(())
}

fn prepare(path: &Path, err: &io::Error) -> ReplayError {
    ReplayError::Prepare {
        path: path.to_path_buf(),
        error: err.to_string(),
    }
}

fn read(path: &Path) -> Result<String, ReplayError> {
    fs::read_to_string(path).map_err(|err| ReplayError::Unreadable {
        path: path.to_path_buf(),
        error: err.to_string(),
    })
}

/// The number of event lines in a canonical trace.
fn events(canonical: &str) -> usize {
    canonical
        .lines()
        .filter(|line| line.starts_with("E "))
        .count()
}

/// The last event line before line `line` (1-based).
fn step_before(canonical: &str, line: usize) -> Option<String> {
    canonical
        .lines()
        .take(line.saturating_sub(1))
        .filter(|text| text.starts_with("E "))
        .last()
        .map(str::to_owned)
}

/// The last [`STDERR_TAIL`] bytes of `stderr`, trimmed.
fn tail(stderr: &[u8]) -> String {
    let start = stderr.len().saturating_sub(STDERR_TAIL);
    String::from_utf8_lossy(stderr.get(start..).unwrap_or_default())
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    //! Each fresh process is this test binary run again through `/proc/self/exe` (Linux),
    //! a fixed path, with `--exact` selecting the test that launched it and a cleared
    //! environment holding only the child's instructions. The same test is then the child:
    //! it runs its seed and writes the trace.
    // check: K-14
    use super::*;
    use std::env;

    const SEED: &str = "KEYVAL_K14_SEED";
    const OUT: &str = "KEYVAL_K14_OUT";
    const COUNTER: &str = "KEYVAL_K14_COUNTER";
    const MODE: &str = "KEYVAL_K14_MODE";

    /// If this process is a child, does what its mode says (by default: runs its seed and
    /// writes the trace) and returns `true`.
    fn child() -> bool {
        let (Some(seed), Some(out)) = (env::var_os(SEED), env::var_os(OUT)) else {
            return false;
        };
        match env::var(MODE).unwrap_or_default().as_str() {
            "fail" => panic!("rule: this child fails on purpose"),
            "noisy-fail" => {
                eprint!("{}", "x".repeat(10 * STDERR_TAIL));
                panic!("rule: this child fails on purpose");
            }
            "sleep" => thread::sleep(Duration::from_secs(60)),
            "empty" => {
                fs::write(out, "").unwrap();
                return true;
            }
            "incomplete" => {
                fs::write(out, incomplete_trace()).unwrap();
                return true;
            }
            _ => {}
        }
        let seed = MasterSeed::new(seed.to_str().unwrap().parse().unwrap());
        let skew = env::var_os(COUNTER).map_or(0, |path| bump(Path::new(&path)));
        let run = run(seed, counter_skew(skew)).unwrap();
        fs::write(out, run.trace.canonical()).unwrap();
        true
    }

    /// A real run's trace, with events, whose last step then overflows its observation
    /// bound, so the run is incomplete.
    fn incomplete_trace() -> String {
        let mut trace = machinery(MasterSeed::new(3)).unwrap().trace;
        let verdict = crate::trace::Record::Verdict {
            checker: Name::new("K-14").unwrap(),
            passed: true,
        };
        while trace.record(&verdict).is_ok() {}
        assert!(!trace.is_complete(), "test setup: the last step overflowed");
        trace.canonical()
    }

    /// Reads the counter file (absent: 0), stores the next value and returns the one read.
    fn bump(path: &Path) -> u64 {
        let value = fs::read_to_string(path).map_or(0, |text| text.trim().parse().unwrap());
        fs::write(path, (value + 1).to_string()).unwrap();
        value
    }

    /// Delays node 3's first timer by `counter` grid steps: zero changes nothing.
    fn counter_skew(counter: u64) -> impl FnMut(NodeIncarnation, u64) -> u64 {
        let mut first = true;
        move |on, delay| {
            if first && on.node.get() == 3 {
                first = false;
                delay + counter * GRID_MICROS
            } else {
                delay
            }
        }
    }

    fn launcher(test: &'static str) -> impl Fn(MasterSeed, &Path) -> Command {
        move |seed, path| {
            let mut command = Command::new("/proc/self/exe");
            command
                .args(["--exact", test, "--test-threads=1", "--nocapture"])
                .env_clear()
                .env(SEED, seed.get().to_string())
                .env(OUT, path);
            command
        }
    }

    /// `<target>/<profile>/k14-replay/<case>`: next to this test binary's `deps/`.
    fn out_dir(case: &str) -> PathBuf {
        let exe = env::current_exe().unwrap();
        let profile = exe.parent().and_then(Path::parent).unwrap();
        profile.join("k14-replay").join(case)
    }

    #[test]
    fn fresh_processes_give_byte_identical_traces() {
        const TEST: &str = "replay::tests::fresh_processes_give_byte_identical_traces";
        if child() {
            return;
        }
        // T0: 2 seeds x 2 fresh processes (§10.7).
        let seeds = [MasterSeed::new(0), MasterSeed::new(42)];
        let out = out_dir("identical");
        let report = fresh_processes(&seeds, 2, &out, PROCESS_TIMEOUT, launcher(TEST)).unwrap();
        assert!(
            report.divergences.is_empty(),
            "rule: the same seed and binary give byte-identical traces in fresh processes \
             (K-14, T-11); {report}"
        );
        for seed in seeds {
            let expected = machinery(seed).unwrap().trace.canonical();
            for process in 0..2 {
                let path = trace_path(&out, seed, process);
                assert_eq!(
                    fs::read_to_string(&path).unwrap(),
                    expected,
                    "rule: each process wrote the full run, not an empty or partial trace; \
                     path={}",
                    path.display()
                );
            }
        }
        assert_eq!(
            (report.seeds, report.processes),
            (2, 2),
            "rule: T0 is 2 seeds x 2 processes"
        );
    }

    #[test]
    fn a_changing_counter_is_caught_at_its_first_differing_event() {
        const TEST: &str =
            "replay::tests::a_changing_counter_is_caught_at_its_first_differing_event";
        if child() {
            return;
        }
        // Each process reads and bumps a counter: process 0 sees 0, process 1 sees 1 and
        // delays one timer by a grid step. It differs every time, never "might differ".
        let seed = MasterSeed::new(7);
        let out = out_dir("counter");
        fs::create_dir_all(&out).unwrap();
        let counter = out.join("counter");
        match fs::remove_file(&counter) {
            Ok(()) => {}
            Err(err) => assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}"),
        }
        let launch = launcher(TEST);
        let report = fresh_processes(&[seed], 2, &out, PROCESS_TIMEOUT, |seed, path| {
            let mut command = launch(seed, path);
            command.env(COUNTER, &counter);
            command
        })
        .unwrap();
        let unskewed = run(seed, counter_skew(0)).unwrap().trace.canonical();
        let skewed = run(seed, counter_skew(1)).unwrap().trace.canonical();
        let Comparison::Differs { line, left, right } = compare(&unskewed, &skewed) else {
            panic!("rule: the control's skew changes the trace; seed={seed:?}");
        };
        let expected = Divergence {
            seed,
            process: 1,
            events: (events(&unskewed), events(&skewed)),
            line,
            step: step_before(&unskewed, line),
            lines: (left, right),
            traces: (trace_path(&out, seed, 0), trace_path(&out, seed, 1)),
        };
        assert_eq!(
            report.divergences,
            [expected],
            "rule: K-14 reports the first differing event and where both traces are kept; \
             {report}"
        );
        assert_eq!(
            fs::read_to_string(&report.divergences[0].traces.0).unwrap(),
            unskewed
        );
        assert_eq!(
            fs::read_to_string(&report.divergences[0].traces.1).unwrap(),
            skewed,
            "rule: both traces are kept for inspection"
        );
        assert!(
            report.divergences[0].step.is_some(),
            "rule: the control diverges mid-run, after a shared event; {report}"
        );
    }

    #[test]
    fn a_failing_process_fails_the_replay() {
        const TEST: &str = "replay::tests::a_failing_process_fails_the_replay";
        if child() {
            return;
        }
        let launch = launcher(TEST);
        let result = fresh_processes(
            &[MasterSeed::new(1)],
            2,
            &out_dir("failing"),
            PROCESS_TIMEOUT,
            |seed, path| {
                let mut command = launch(seed, path);
                command.env(MODE, "noisy-fail");
                command
            },
        );
        assert!(
            matches!(
                result,
                Err(ReplayError::Failed { process: 0, ref stderr, .. })
                    if stderr.contains("this child fails on purpose")
                        && stderr.len() <= STDERR_TAIL
            ),
            "rule: a failed process is an error, never a pass (P10), and its report keeps \
             only the tail of 10x the bound of stderr (CD-9); {result:?}"
        );
    }

    #[test]
    fn a_trace_without_events_fails_the_replay() {
        const TEST: &str = "replay::tests::a_trace_without_events_fails_the_replay";
        if child() {
            return;
        }
        let launch = launcher(TEST);
        let out = out_dir("eventless");
        let result = fresh_processes(
            &[MasterSeed::new(1)],
            2,
            &out,
            PROCESS_TIMEOUT,
            |seed, path| {
                let mut command = launch(seed, path);
                command.env(MODE, "empty");
                command
            },
        );
        assert_eq!(
            result,
            Err(ReplayError::NoEvents {
                seed: MasterSeed::new(1),
                process: 0,
                path: trace_path(&out, MasterSeed::new(1), 0),
            }),
            "rule: two empty traces are byte-identical, but compare nothing, so they fail (P10)"
        );
    }

    #[test]
    fn an_incomplete_trace_fails_the_replay_even_when_identical() {
        const TEST: &str =
            "replay::tests::an_incomplete_trace_fails_the_replay_even_when_identical";
        if child() {
            return;
        }
        let launch = launcher(TEST);
        let out = out_dir("incomplete");
        let result = fresh_processes(
            &[MasterSeed::new(1)],
            2,
            &out,
            PROCESS_TIMEOUT,
            |seed, path| {
                let mut command = launch(seed, path);
                command.env(MODE, "incomplete");
                command
            },
        );
        assert_eq!(
            result,
            Err(ReplayError::Incomplete {
                seed: MasterSeed::new(1),
                process: 0,
                path: trace_path(&out, MasterSeed::new(1), 0),
            }),
            "rule: identical incomplete traces are not replay identity (P10)"
        );
        let written = fs::read_to_string(trace_path(&out, MasterSeed::new(1), 1)).unwrap();
        assert!(
            events(&written) > 0 && written == incomplete_trace(),
            "test setup: both processes wrote the same incomplete trace, with events"
        );
    }

    #[test]
    fn a_process_past_its_timeout_is_killed_and_fails_the_replay() {
        const TEST: &str =
            "replay::tests::a_process_past_its_timeout_is_killed_and_fails_the_replay";
        if child() {
            return;
        }
        let launch = launcher(TEST);
        let timeout = Duration::from_millis(200);
        let started = Instant::now();
        let result = fresh_processes(
            &[MasterSeed::new(1)],
            2,
            &out_dir("timeout"),
            timeout,
            |seed, path| {
                let mut command = launch(seed, path);
                command.env(MODE, "sleep");
                command
            },
        );
        assert_eq!(
            result,
            Err(ReplayError::TimedOut {
                seed: MasterSeed::new(1),
                process: 0,
                after: timeout,
            }),
            "rule: a process that never finishes fails the replay; it is never awaited forever"
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "rule: the sleeping child was killed, not awaited; elapsed={:?}",
            started.elapsed()
        );
    }

    #[test]
    fn repeated_seeds_are_refused_before_anything_runs() {
        let never = |_: MasterSeed, _: &Path| -> Command { panic!("rule: nothing is launched") };
        let seed = MasterSeed::new(4);
        assert_eq!(
            fresh_processes(
                &[seed, MasterSeed::new(5), seed],
                2,
                &out_dir("repeated"),
                PROCESS_TIMEOUT,
                never
            ),
            Err(ReplayError::RepeatedSeed { seed }),
            "rule: a repeated seed's processes would share one trace file and be counted \
             twice"
        );
    }

    #[test]
    fn a_process_that_writes_no_trace_fails_the_replay() {
        let out = out_dir("silent");
        // Stale traces from an earlier replay are removed first, so they cannot pass.
        for process in 0..2 {
            let path = trace_path(&out, MasterSeed::new(1), process);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "E 0 0 - - stale\n").unwrap();
        }
        // The test binary selecting no test exits successfully and writes nothing.
        let result = fresh_processes(
            &[MasterSeed::new(1)],
            2,
            &out,
            PROCESS_TIMEOUT,
            launcher("replay::tests::no_such_test"),
        );
        assert_eq!(
            result,
            Err(ReplayError::Unreadable {
                path: trace_path(&out, MasterSeed::new(1), 0),
                error: "No such file or directory (os error 2)".to_owned(),
            }),
            "rule: a missing trace is an error, never a pass (P10)"
        );
    }

    #[test]
    fn a_replay_needs_a_seed_and_two_processes() {
        let never = |_: MasterSeed, _: &Path| -> Command { panic!("rule: nothing is launched") };
        assert_eq!(
            fresh_processes(&[], 2, &out_dir("vacuous"), PROCESS_TIMEOUT, never),
            Err(ReplayError::Vacuous {
                seeds: 0,
                processes: 2
            })
        );
        assert_eq!(
            fresh_processes(
                &[MasterSeed::new(1)],
                1,
                &out_dir("vacuous"),
                PROCESS_TIMEOUT,
                never
            ),
            Err(ReplayError::Vacuous {
                seeds: 1,
                processes: 1
            }),
            "rule: one process has nothing to compare with (P10)"
        );
    }

    #[test]
    fn the_machinery_run_exercises_ties_restarts_and_stale_timers() {
        for seed in [0, 42, 7] {
            let run = machinery(MasterSeed::new(seed)).unwrap();
            let text = run.trace.canonical();
            assert!(
                run.events > 100 && run.same_time > 0 && run.restarts > 0 && run.stale_timeouts > 0,
                "rule: K-14's run is not vacuous: many events, seeded ties, restarts and \
                 stale timers; seed={seed} events={} same_time={} restarts={} stale={}",
                run.events,
                run.same_time,
                run.restarts,
                run.stale_timeouts
            );
            assert_eq!(
                u64::try_from(events(&text)).unwrap(),
                run.events,
                "rule: every dispatched event is traced"
            );
            assert!(
                text.ends_with(&format!(
                    "M events {}\nM restarts {}\nM same_time {}\nM stale_timeouts {}\n",
                    run.events, run.restarts, run.same_time, run.stale_timeouts
                )),
                "rule: the run's metrics close the trace in name order; seed={seed}"
            );
        }
    }

    #[test]
    fn the_machinery_run_depends_only_on_its_seed() {
        let a = machinery(MasterSeed::new(5)).unwrap().trace;
        let b = machinery(MasterSeed::new(5)).unwrap().trace;
        let c = machinery(MasterSeed::new(6)).unwrap().trace;
        assert_eq!(
            a.canonical(),
            b.canonical(),
            "rule: same seed, same trace (P4)"
        );
        assert_ne!(a.hash(), c.hash(), "rule: the seed drives the run");
        assert_eq!(
            run(MasterSeed::new(5), counter_skew(0))
                .unwrap()
                .trace
                .canonical(),
            a.canonical(),
            "rule: the control's skew is the identity at zero"
        );
    }

    #[test]
    fn the_first_divergence_names_the_event_step_before_it() {
        let trace = "R - packet\nE 0 10 - n1/0 timeout\nR 0 role\nE 1 10 0 n2/0 timeout\n";
        assert_eq!(step_before(trace, 1), None, "rule: before the first event");
        assert_eq!(step_before(trace, 2), None);
        assert_eq!(
            step_before(trace, 3).as_deref(),
            Some("E 0 10 - n1/0 timeout"),
            "rule: a differing record names the step it belongs to"
        );
        assert_eq!(
            step_before(trace, 4).as_deref(),
            Some("E 0 10 - n1/0 timeout"),
            "rule: a differing event names the step before it"
        );
        assert_eq!(events(trace), 2);
        let long = vec![b'x'; STDERR_TAIL + 10];
        assert_eq!(
            tail(&long).len(),
            STDERR_TAIL,
            "rule: stderr is bounded (CD-9)"
        );
    }
}
