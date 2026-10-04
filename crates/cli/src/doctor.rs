//! `keyval doctor <data-dir>`: offline verification of a node's data directory (PLAN.md
//! O-7, D-26).
//!
//! 1. The directory is opened read-only ([`ReadOnlyDir`]): nothing is created or written.
//! 2. Its lock is probed first. A live directory (another process holds the lock) is
//!    refused unexamined, so an in-flight append is never read as damage. A free lock is
//!    held while the doctor reads, so no node starts meanwhile. With no LOCK file nothing
//!    keeps a node from starting, so the doctor looks again afterwards, and gives no
//!    verdict on a journal or lock that moved (br-riea).
//! 3. The journal is examined exactly as startup recovery reads it
//!    ([`journal::examine`]): the genesis record, every frame's checksums and sequence,
//!    index contiguity, term monotonicity and hard-state consistency. With `--config` and
//!    `--node`, the genesis binding is verified too (DUR-8).
//! 4. The report says what startup recovery would do (start, discard a described torn
//!    tail, or refuse, with why), as text or as `keyval.cli.doctor/v2` JSON (O-6).

use std::fmt::Write as _;
use std::path::Path;
use std::time::SystemTime;

use fs_storage::{Failure, LockProbe, ReadOnlyDir};
use journal::{
    Binding, Examination, LogBounds, RecoveryError, TailShape, TornTail, Verdict, examine,
};
use serde::Serialize;
use types::{ByteCount, Config, LogIndex, NodeId, StorageError};

/// The JSON report's schema identifier (O-6); its JSON Schema is
/// `schemas/cli/doctor.v2.schema.json`. v2 (br-riea): `refusal.init_allowed` may be null,
/// and the verdict may be `changed`.
pub const SCHEMA: &str = "keyval.cli.doctor/v2";

/// The logical log's bounds without a configuration: the entry cache's fixed bound
/// (`Limit::EntryCacheBytes`, 8 MiB). The doctor never appends, so the hard stop is
/// never reached.
const UNCONFIGURED: LogBounds = LogBounds::new(ByteCount::new(8 << 20), ByteCount::new(u64::MAX));

/// What the doctor concluded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    /// Startup recovery would start: the journal ends with a whole frame.
    Clean,
    /// Startup recovery would discard a torn tail, then start.
    DiscardTail,
    /// Startup recovery would refuse to start.
    Refuse,
    /// Not examined: another process holds the lock, so the node is live.
    Live,
    /// The directory holds no journal: a blank node never starts (D-52).
    NoJournal,
    /// Not examined: the directory or journal cannot be read.
    Unreadable,
    /// Read without a LOCK file, and the journal or the lock moved meanwhile: a node may
    /// have started, so no verdict is given on bytes that moved (br-riea).
    Changed,
}

impl Conclusion {
    /// The process exit code: 0 if startup would start, 1 otherwise. A usage error is 2,
    /// reported by the argument parser.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Clean | Self::DiscardTail => 0,
            Self::Refuse | Self::Live | Self::NoJournal | Self::Unreadable | Self::Changed => 1,
        }
    }
}

/// The lock probe's result, as reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LockState {
    /// The lock was free; the doctor held it while reading.
    Acquired,
    /// There is no LOCK file: no node took the lock since the directory was created, or
    /// the file was removed. Nothing keeps a node from starting while the doctor reads, so it
    /// looks again afterwards (br-riea).
    NoLockFile,
    /// Another process holds the lock.
    Live,
}

/// Whether the genesis binding was verified (DUR-8), as reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingState {
    /// The genesis record matches `--config` and `--node`.
    Verified,
    /// The genesis record does not match `--config` and `--node`, so recovery refuses
    /// ([`RefusalCode::GenesisMismatch`]).
    Mismatch,
    /// Not checked: no configuration was given, or no genesis record was read.
    Unchecked,
}

/// A hard state, as reported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HardStateReport {
    /// The term.
    pub term: u64,
    /// The vote, if one was cast in that term.
    pub vote: Option<u64>,
    /// The persisted commit index.
    pub commit: u64,
}

/// The genesis record, as reported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GenesisReport {
    /// The cluster instance ID, in hex.
    pub instance_id: String,
    /// The display cluster ID.
    pub cluster_id: String,
    /// The node the directory is bound to.
    pub node_id: u64,
    /// The voter set.
    pub voters: Vec<u64>,
    /// The semantic configuration fingerprint, in hex.
    pub fingerprint: String,
}

/// A torn tail's shape, as reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShapeReport {
    /// A frame cut short.
    Incomplete,
    /// A full-length frame failing its checksum.
    ChecksumFailed,
    /// Bytes without an intact header.
    Unreadable,
    /// A stale copy of an earlier frame.
    Stale,
}

/// The torn tail recovery would discard, as reported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TailReport {
    /// Where it starts: the length recovery truncates the journal to.
    pub start: u64,
    /// Its length in bytes.
    pub len: u64,
    /// Its shape.
    pub shape: ShapeReport,
    /// For an incomplete frame, the frame's full length.
    pub frame_len: Option<u64>,
    /// For a stale copy, its sequence.
    pub sequence: Option<u64>,
}

/// A stable code for each recovery refusal ([`RecoveryError`]'s variants).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    /// [`RecoveryError::Empty`].
    Empty,
    /// [`RecoveryError::IncompleteGenesis`].
    IncompleteGenesis,
    /// [`RecoveryError::NotAnInterruptedInit`].
    NotAnInterruptedInit,
    /// [`RecoveryError::ImpossibleHeader`].
    ImpossibleHeader,
    /// [`RecoveryError::NotGenesis`].
    NotGenesis,
    /// [`RecoveryError::GenesisRecord`].
    GenesisRecord,
    /// [`RecoveryError::Mismatch`].
    GenesisMismatch,
    /// [`RecoveryError::Record`].
    Record,
    /// [`RecoveryError::Apply`].
    Apply,
    /// [`RecoveryError::Corruption`].
    Corruption,
    /// [`RecoveryError::TailTooLong`].
    TailTooLong,
    /// [`RecoveryError::Storage`].
    Storage,
}

impl RefusalCode {
    /// The code for `error`.
    #[must_use]
    pub const fn of(error: &RecoveryError) -> Self {
        match error {
            RecoveryError::Empty => Self::Empty,
            RecoveryError::IncompleteGenesis => Self::IncompleteGenesis,
            RecoveryError::NotAnInterruptedInit { .. } => Self::NotAnInterruptedInit,
            RecoveryError::ImpossibleHeader { .. } => Self::ImpossibleHeader,
            RecoveryError::NotGenesis { .. } => Self::NotGenesis,
            RecoveryError::GenesisRecord(_) => Self::GenesisRecord,
            RecoveryError::Mismatch(_) => Self::GenesisMismatch,
            RecoveryError::Record { .. } => Self::Record,
            RecoveryError::Apply { .. } => Self::Apply,
            RecoveryError::Corruption { .. } => Self::Corruption,
            RecoveryError::TailTooLong { .. } => Self::TailTooLong,
            RecoveryError::Storage(_) => Self::Storage,
        }
    }
}

/// Why recovery would refuse, as reported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RefusalReport {
    /// A stable code for the refusal.
    pub code: RefusalCode,
    /// The refusal, in words.
    pub message: String,
    /// Whether `init` may (re-)run on the directory (§7.7 step 0); `None` when that depends
    /// on the genesis frame's length and no configuration was given (br-riea).
    pub init_allowed: Option<bool>,
}

/// The doctor's report: the `keyval.cli.doctor/v1` JSON object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    /// [`SCHEMA`].
    pub schema: &'static str,
    /// The data directory examined.
    pub data_dir: String,
    /// The lock probe's result, if the directory could be opened.
    pub lock: Option<LockState>,
    /// What the doctor concluded.
    pub verdict: Conclusion,
    /// What startup recovery would do, in words.
    pub would_do: String,
    /// Whether the genesis binding was verified.
    pub binding: BindingState,
    /// The journal's length, if it was read.
    pub journal_len: Option<u64>,
    /// Where the frames read end: the durable prefix, or where a refusal was found. `None`
    /// when no logical log was built, never 0 as if read (br-ljgh).
    pub durable_len: Option<u64>,
    /// The frames read and applied, the genesis included.
    pub frames: Option<u64>,
    /// The last index of the logical log; `None` when no logical log was built.
    pub last_index: Option<u64>,
    /// The term of the last entry, if there is one.
    pub last_term: Option<u64>,
    /// The persisted hard state, if the genesis was read.
    pub hard_state: Option<HardStateReport>,
    /// The genesis record, if the first frame is one.
    pub genesis: Option<GenesisReport>,
    /// The torn tail recovery would discard, if any.
    pub tail: Option<TailReport>,
    /// Why recovery would refuse, if it would.
    pub refusal: Option<RefusalReport>,
}

/// A torn tail as reported.
const fn tail_report(tail: &TornTail) -> TailReport {
    let (shape, frame_len, sequence) = match tail.shape() {
        TailShape::Incomplete { frame_len } => (ShapeReport::Incomplete, Some(frame_len), None),
        TailShape::ChecksumFailed => (ShapeReport::ChecksumFailed, None, None),
        TailShape::Unreadable => (ShapeReport::Unreadable, None, None),
        TailShape::Stale { sequence } => (ShapeReport::Stale, None, Some(sequence)),
    };
    TailReport {
        start: tail.start().get(),
        len: tail.len().get(),
        shape,
        frame_len,
        sequence,
    }
}

impl Report {
    /// A report on `dir` that has examined nothing yet.
    fn new(dir: &Path) -> Self {
        Self {
            schema: SCHEMA,
            data_dir: dir.display().to_string(),
            lock: None,
            verdict: Conclusion::Unreadable,
            would_do: String::new(),
            binding: BindingState::Unchecked,
            journal_len: None,
            durable_len: None,
            frames: None,
            last_index: None,
            last_term: None,
            hard_state: None,
            genesis: None,
            tail: None,
            refusal: None,
        }
    }

    /// The report concluding `verdict`, with `would_do` saying what that means.
    fn concluding(mut self, verdict: Conclusion, would_do: String) -> Self {
        self.verdict = verdict;
        self.would_do = would_do;
        self
    }

    /// The report withdrawing its verdict because what it read moved: what was read stays,
    /// the tail and refusal go.
    fn withdrawn(mut self, verdict: Conclusion, would_do: String) -> Self {
        self.tail = None;
        self.refusal = None;
        self.concluding(verdict, would_do)
    }

    /// The report on a directory the doctor could not read.
    fn unreadable(self, failure: Option<Failure>) -> Self {
        let cause = failure.map_or_else(
            || "the data directory cannot be read".to_owned(),
            |failure| {
                format!(
                    "{} {}: {}",
                    failure.op,
                    failure.path.display(),
                    failure.cause
                )
            },
        );
        self.concluding(Conclusion::Unreadable, format!("not examined: {cause}"))
    }

    /// The report filled in from `examination`.
    fn examined(mut self, examination: &Examination) -> Self {
        self.journal_len = Some(examination.journal_len().get());
        self.durable_len = examination.durable_len().map(ByteCount::get);
        self.frames = Some(examination.frames());
        self.last_index = examination.last_index().map(LogIndex::get);
        self.last_term = examination.last_term().map(types::Term::get);
        self.hard_state = examination.hard_state().map(|state| HardStateReport {
            term: state.term.get(),
            vote: state.vote.map(NodeId::get),
            commit: state.commit.get(),
        });
        self.genesis = examination.genesis().map(|genesis| GenesisReport {
            instance_id: genesis.instance_id().to_string(),
            cluster_id: genesis.cluster_id().as_str().to_owned(),
            node_id: genesis.node_id().get(),
            voters: genesis.voters().iter().map(|voter| voter.get()).collect(),
            fingerprint: genesis.fingerprint().to_string(),
        });
        // The examination reports a binding verified only once a genesis record was read
        // and checked; recovery refuses a mismatch, so any other verdict matched.
        let bound = match examination.binding() {
            Binding::Verified => true,
            Binding::Unchecked => false,
        };
        let refusal = match examination.verdict() {
            Verdict::Clean | Verdict::DiscardTail(_) => None,
            Verdict::Refuse(error) => Some(RefusalCode::of(error)),
        };
        self.binding = if !bound {
            BindingState::Unchecked
        } else if refusal == Some(RefusalCode::GenesisMismatch) {
            BindingState::Mismatch
        } else {
            BindingState::Verified
        };
        match examination.verdict() {
            Verdict::Clean => self.concluding(
                Conclusion::Clean,
                "start: the journal ends with a whole frame".to_owned(),
            ),
            Verdict::DiscardTail(tail) => {
                self.tail = Some(tail_report(tail));
                self.concluding(Conclusion::DiscardTail, format!("{tail}, then start"))
            }
            Verdict::Refuse(error) => {
                let init_allowed = examination.init_allowed();
                self.refusal = Some(RefusalReport {
                    code: RefusalCode::of(error),
                    message: error.to_string(),
                    init_allowed,
                });
                let init = match init_allowed {
                    Some(true) => "; `init` may (re-)run on it",
                    Some(false) => "",
                    None => {
                        "; whether `init` may re-run depends on this node's genesis frame length: \
                         run the doctor again with --config and --node"
                    }
                };
                self.concluding(
                    Conclusion::Refuse,
                    format!("refuse to start: {error}{init}"),
                )
            }
        }
    }

    /// The report as text for a person.
    #[must_use]
    pub fn human(&self) -> String {
        let mut out = format!("keyval doctor: {}\n", self.data_dir);
        if let Some(lock) = self.lock {
            let lock = match lock {
                LockState::Acquired => "free (held while reading)",
                LockState::NoLockFile => "no LOCK file",
                LockState::Live => "held by another process (a live node)",
            };
            let _ = writeln!(out, "  lock: {lock}");
        }
        if let (Some(len), Some(frames)) = (self.journal_len, self.frames) {
            // What was never read is left out, never shown as 0 (br-ljgh).
            let mut line = format!("  journal: {len} bytes; {frames} frames read");
            if let Some(durable) = self.durable_len {
                let _ = write!(line, " through byte {durable}");
            }
            if let Some(last) = self.last_index {
                let _ = write!(line, "; last index {last}");
                if let Some(term) = self.last_term {
                    let _ = write!(line, " (term {term})");
                }
            }
            let _ = writeln!(out, "{line}");
        }
        if let Some(state) = &self.hard_state {
            let vote = state
                .vote
                .map_or_else(|| "none".to_owned(), |vote| vote.to_string());
            let _ = writeln!(
                out,
                "  hard state: term {}, vote {vote}, commit {}",
                state.term, state.commit
            );
        }
        if let Some(genesis) = &self.genesis {
            let binding = match self.binding {
                BindingState::Verified => "binding verified against the config",
                BindingState::Mismatch => "binding does NOT match the config",
                BindingState::Unchecked => "binding unchecked (no --config)",
            };
            let _ = writeln!(
                out,
                "  genesis: node {} of cluster {} (instance {}), voters {:?}; {binding}",
                genesis.node_id, genesis.cluster_id, genesis.instance_id, genesis.voters
            );
        }
        let _ = writeln!(out, "  verdict: {}", self.would_do);
        out
    }
}

/// Verifies the data directory `dir` offline, modifying nothing (O-7). With `binding`,
/// the genesis is verified against that configuration and node too.
#[must_use]
pub fn doctor(dir: &Path, binding: Option<(&Config, NodeId)>) -> Report {
    let mut report = Report::new(dir);
    let mut handle = match ReadOnlyDir::open(dir) {
        Ok(handle) => handle,
        Err(failure) => return report.unreadable(Some(failure)),
    };
    let Ok(lock) = handle.probe_lock() else {
        return report.unreadable(handle.last_failure());
    };
    let (lock, live) = match lock {
        LockProbe::Acquired => (LockState::Acquired, false),
        LockProbe::NoLockFile => (LockState::NoLockFile, false),
        LockProbe::Live => (LockState::Live, true),
    };
    report.lock = Some(lock);
    if live {
        return report.concluding(
            Conclusion::Live,
            "not examined: another process holds the data directory's lock (a live node); \
             stop it first, so an append in flight is never read as damage"
                .to_owned(),
        );
    }
    let len = match handle.journal_len() {
        Ok(len) => len,
        Err(StorageError::Missing) => {
            return report.concluding(
                Conclusion::NoJournal,
                "refuse to start: the data directory has no journal, and a blank node never \
                 starts (D-52); `init` may run on it"
                    .to_owned(),
            );
        }
        Err(_) => return report.unreadable(handle.last_failure()),
    };
    let before = journal_stamp(dir);
    let bounds = binding.map_or(UNCONFIGURED, |(config, _)| LogBounds::of(config));
    let examination = examine(&handle, len, binding, bounds);
    if let Verdict::Refuse(RecoveryError::Storage(_)) = examination.verdict() {
        // The journal could not be read: say why, as for any unreadable directory.
        return report.unreadable(handle.last_failure());
    }
    let report = report.examined(&examination);
    match lock {
        LockState::NoLockFile => recheck(report, &mut handle, dir, before),
        LockState::Acquired | LockState::Live => report,
    }
}

/// The journal file's length and modification time, to tell whether it changed.
type Stamp = Option<(u64, SystemTime)>;

/// `dir`'s journal stamp, if it can be read.
fn journal_stamp(dir: &Path) -> Stamp {
    let metadata = std::fs::metadata(dir.join(fs_storage::JOURNAL)).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

/// With no LOCK file, nothing kept a node from starting while the doctor read, so it looks
/// again: a lock taken meanwhile, or a journal that changed, withdraws the verdict, which was
/// given on bytes that may have moved (br-riea).
fn recheck(mut report: Report, handle: &mut ReadOnlyDir, dir: &Path, before: Stamp) -> Report {
    let moved = match handle.probe_lock() {
        Ok(LockProbe::Live) => {
            return report.withdrawn(
                Conclusion::Live,
                "not judged: a node took the data directory's lock while the doctor read it; \
                 stop it, then run the doctor again"
                    .to_owned(),
            );
        }
        Ok(LockProbe::Acquired) => Some("a LOCK file appeared while the doctor read"),
        Ok(LockProbe::NoLockFile) => {
            (journal_stamp(dir) != before).then_some("the journal changed while the doctor read")
        }
        Err(_) => Some("the lock could not be probed again after reading"),
    };
    if let Some(why) = moved {
        return report.withdrawn(
            Conclusion::Changed,
            format!(
                "not judged: {why}; with no LOCK file nothing kept a node from starting, so \
                 stop every node on this directory and run the doctor again"
            ),
        );
    }
    report.would_do.push_str(
        " (there is no LOCK file, so nothing kept a node from starting; the journal did not \
         change while it was read)",
    );
    report
}

#[cfg(test)]
mod doctor_cli {
    // The re-check after a read without a LOCK file (br-riea), driven directly: a race
    // cannot be staged through the binary.
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A fresh data directory under the cargo target directory, holding `journal` and no
    /// LOCK file.
    fn unlocked_dir(name: &str, journal: &[u8]) -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let exe = std::env::current_exe().unwrap();
        let target = exe.ancestors().nth(3).unwrap().to_path_buf();
        let dir = target.join("doctor-recheck-scratch").join(format!(
            "{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(fs_storage::JOURNAL), journal).unwrap();
        dir
    }

    /// What the doctor's re-check concludes on `dir`, read with stamp `before`.
    fn rechecked(dir: &Path, before: Stamp) -> Report {
        let mut handle = ReadOnlyDir::open(dir).unwrap();
        assert_eq!(
            handle.probe_lock(),
            Ok(LockProbe::NoLockFile),
            "fixture: no LOCK file"
        );
        let report = Report::new(dir).concluding(Conclusion::Clean, "start".to_owned());
        recheck(report, &mut handle, dir, before)
    }

    #[test]
    fn a_read_without_a_lock_file_is_judged_only_if_nothing_moved() {
        let dir = unlocked_dir("still", b"journal bytes");
        let still = rechecked(&dir, journal_stamp(&dir));
        assert!(
            still.verdict == Conclusion::Clean && still.would_do.contains("no LOCK file"),
            "rule: an unchanged journal keeps its verdict, flagged as read without a LOCK \
             file (br-riea); report={still:?}"
        );
        let moved = rechecked(&dir, Some((0, SystemTime::UNIX_EPOCH)));
        assert!(
            moved.verdict == Conclusion::Changed && moved.would_do.contains("journal changed"),
            "rule: a journal that changed while it was read is not judged (br-riea); \
             report={moved:?}"
        );
        let mut handle = ReadOnlyDir::open(&dir).unwrap();
        assert_eq!(handle.probe_lock(), Ok(LockProbe::NoLockFile));
        std::fs::write(dir.join(fs_storage::LOCK), b"").unwrap();
        let report = Report::new(&dir).concluding(Conclusion::Clean, "start".to_owned());
        let appeared = recheck(report, &mut handle, &dir, journal_stamp(&dir));
        assert!(
            appeared.verdict == Conclusion::Changed
                && appeared.would_do.contains("LOCK file appeared"),
            "rule: a LOCK file that appeared while reading withdraws the verdict (br-riea); \
             report={appeared:?}"
        );
        drop(handle);
        let live_dir = unlocked_dir("live", b"journal bytes");
        let mut handle = ReadOnlyDir::open(&live_dir).unwrap();
        assert_eq!(handle.probe_lock(), Ok(LockProbe::NoLockFile));
        let mut node = fs_storage::FsStorage::open(&live_dir).unwrap();
        types::DataDir::lock(&mut node).unwrap();
        let report = Report::new(&live_dir).concluding(Conclusion::Clean, "start".to_owned());
        let live = recheck(report, &mut handle, &live_dir, journal_stamp(&live_dir));
        assert_eq!(
            live.verdict,
            Conclusion::Live,
            "rule: a node that took the lock while the doctor read makes the directory live \
             (br-riea); report={live:?}"
        );
    }
}
