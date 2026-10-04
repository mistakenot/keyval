//! `keyval doctor <data-dir>`: offline verification of a node's data directory (PLAN.md
//! O-7, D-26).
//!
//! 1. The directory is opened read-only ([`ReadOnlyDir`]): nothing is created or written.
//! 2. Its lock is probed first. A live directory (another process holds the lock) is
//!    refused unexamined, so an in-flight append is never read as damage. A free lock is
//!    held while the doctor reads, so no node starts meanwhile.
//! 3. The journal is examined exactly as startup recovery reads it
//!    ([`journal::examine`]): the genesis record, every frame's checksums and sequence,
//!    index contiguity, term monotonicity and hard-state consistency. With `--config` and
//!    `--node`, the genesis binding is verified too (DUR-8).
//! 4. The report says what startup recovery would do (start, discard a described torn
//!    tail, or refuse, with why), as text or as `keyval.cli.doctor/v1` JSON (O-6).

use std::fmt::Write as _;
use std::path::Path;

use fs_storage::{Failure, LockProbe, ReadOnlyDir};
use journal::{
    Binding, Examination, LogBounds, RecoveryError, TailShape, TornTail, Verdict, examine,
};
use serde::Serialize;
use types::{ByteCount, Config, NodeId, StorageError};

/// The JSON report's schema identifier (O-6); its JSON Schema is
/// `schemas/cli/doctor.v1.schema.json`.
pub const SCHEMA: &str = "keyval.cli.doctor/v1";

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
}

impl Conclusion {
    /// The process exit code: 0 if startup would start, 1 otherwise. A usage error is 2,
    /// reported by the argument parser.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Clean | Self::DiscardTail => 0,
            Self::Refuse | Self::Live | Self::NoJournal | Self::Unreadable => 1,
        }
    }
}

/// The lock probe's result, as reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LockState {
    /// The lock was free; the doctor held it while reading.
    Acquired,
    /// There is no LOCK file: no process ever took the directory's lock.
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
    /// Whether `init` may (re-)run on the directory (§7.7 step 0).
    pub init_allowed: bool,
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
    /// Where the frames read end: the durable prefix, or where a refusal was found.
    pub durable_len: Option<u64>,
    /// The frames read and applied, the genesis included.
    pub frames: Option<u64>,
    /// The last index of the logical log.
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
        self.durable_len = Some(examination.durable_len().get());
        self.frames = Some(examination.frames());
        self.last_index = Some(examination.last_index().get());
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
        // Recovery checks the binding as soon as it has decoded the genesis record, and
        // refuses on a mismatch; a genesis record past any other point has matched.
        let bound = match examination.binding() {
            Binding::Verified => examination.genesis().is_some(),
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
                self.refusal = Some(RefusalReport {
                    code: RefusalCode::of(error),
                    message: error.to_string(),
                    init_allowed: error.permits_init(),
                });
                let init = if error.permits_init() {
                    "; `init` may (re-)run on it"
                } else {
                    ""
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
        if let (Some(len), Some(durable), Some(frames)) =
            (self.journal_len, self.durable_len, self.frames)
        {
            let _ = writeln!(
                out,
                "  journal: {len} bytes; {frames} frames read through byte {durable}; last index \
                 {}{}",
                self.last_index.unwrap_or_default(),
                self.last_term
                    .map_or_else(String::new, |term| format!(" (term {term})"))
            );
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
    let bounds = binding.map_or(UNCONFIGURED, |(config, _)| LogBounds::of(config));
    let examination = examine(&handle, len, binding, bounds);
    report.examined(&examination)
}
