//! The sync-order check (PLAN.md §10.17, D-46; W24.2).
//!
//! `SIGKILL` keeps the page cache, so the crash probes cannot see a missing `fsync`. This
//! check looks *below* the adapter instead: [`Recording`] is the [`Os`] seam's test
//! implementation, logging each operating-system call [`FsStorage`] makes while the real
//! journal engine drives it, and [`judge`] holds the resulting trace to the obligations of
//! §7.4 and DUR-7. A port-level wrapper would prove only that the engine *requested* a
//! sync; this trace shows which calls the adapter issued, and in what order.
//!
//! The obligations [`judge`] enforces, each a [`Violation`] when broken:
//! - **DUR-7, directory.** A `mkdir` is followed by an `fsync` of its parent before any
//!   journal is created, written or synced.
//! - **DUR-7, journal.** A journal's creation is followed by an `fsync` of its directory
//!   before any byte is appended, truncated or synced.
//! - **§7.4, one frame in flight.** No append (or truncation) while an earlier one is
//!   unsynced, and the trace never ends with one unsynced: every write is covered by an
//!   `fdatasync` before the next write, so a completion is never reported over bytes that
//!   were not made durable.
//!
//! What this cannot show is that the disk honours `fsync`: power-loss durability is the
//! §10.15 Assumption. The scenarios below run against real files, so the calls recorded
//! are the calls the operating system received.

// check: SYNC-ORDER
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use journal::{FrameKind, Genesis, LogBounds, LogicalLog, RaftBatch, encode, recover};
use types::{
    ByteCount, ByteStorage as _, Config, DataDir as _, Entry, EntryIndex, EntryPayload, HardState,
    InstanceId, LogIndex, NodeId, Term,
};

use super::{FsStorage, JOURNAL, Os, RealOs, create_data_dir_with};

/// One operating-system call, as the recording saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    CreateDir(PathBuf),
    CreateJournal(PathBuf),
    SyncDir(PathBuf),
    Append(usize),
    Truncate(u64),
    SyncData,
}

/// A way the trace breaks an obligation, naming the call index where it happened.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Violation {
    /// A journal was created while a new directory's parent was not yet synced (DUR-7).
    JournalBeforeParentSync { at: usize },
    /// A write, truncation or sync ran before the new directory's parent was synced
    /// (DUR-7).
    WriteBeforeParentSync { at: usize },
    /// A write, truncation or sync ran before the new journal's directory was synced
    /// (DUR-7).
    WriteBeforeDirectorySync { at: usize },
    /// A write or truncation ran while an earlier one was still unsynced (§7.4).
    WriteOverUnsynced { at: usize },
    /// The trace ended with a directory entry unsynced (DUR-7).
    EndedWithUnsyncedEntry,
    /// The trace ended with a write or truncation unsynced (§7.4).
    EndedWithUnsyncedWrite,
}

/// What the trace has written without an `fdatasync` yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unsynced {
    /// Nothing: every write so far is covered.
    Nothing,
    /// An append, or a truncation to a length above zero.
    Written,
    /// A truncation to zero.
    Emptied,
}

/// The directory a path's entry lives in, as the adapter syncs it.
fn parent_of(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// Holds `trace` to the module's obligations.
fn judge(trace: &[Call]) -> Result<(), Violation> {
    // The parent whose sync a `mkdir` still owes, the directory whose sync a journal
    // creation still owes, and whether a write or truncation is unsynced.
    let mut parent_owed: Option<PathBuf> = None;
    let mut directory_owed: Option<PathBuf> = None;
    let mut unsynced = Unsynced::Nothing;
    for (at, call) in trace.iter().enumerate() {
        match call {
            Call::CreateDir(dir) => parent_owed = Some(parent_of(dir)),
            Call::SyncDir(dir) => {
                if parent_owed.as_ref() == Some(dir) {
                    parent_owed = None;
                }
                if directory_owed.as_ref() == Some(dir) {
                    directory_owed = None;
                }
            }
            Call::CreateJournal(path) => {
                if parent_owed.is_some() {
                    return Err(Violation::JournalBeforeParentSync { at });
                }
                directory_owed = Some(parent_of(path));
            }
            Call::Append(_) | Call::Truncate(_) | Call::SyncData => {
                if parent_owed.is_some() {
                    return Err(Violation::WriteBeforeParentSync { at });
                }
                if directory_owed.is_some() {
                    return Err(Violation::WriteBeforeDirectorySync { at });
                }
                match call {
                    Call::Append(_) | Call::Truncate(_) => {
                        // `init` re-run over an incomplete genesis empties the journal and
                        // rewrites it from the start under one fdatasync (§7.7 step 0):
                        // nothing after the new frame can be mistaken for a synced one.
                        let rewrite =
                            unsynced == Unsynced::Emptied && matches!(call, Call::Append(_));
                        if unsynced != Unsynced::Nothing && !rewrite {
                            return Err(Violation::WriteOverUnsynced { at });
                        }
                        unsynced = match call {
                            Call::Truncate(0) => Unsynced::Emptied,
                            _ => Unsynced::Written,
                        };
                    }
                    Call::SyncData => unsynced = Unsynced::Nothing,
                    Call::CreateDir(_) | Call::CreateJournal(_) | Call::SyncDir(_) => {}
                }
            }
        }
    }
    if parent_owed.is_some() || directory_owed.is_some() {
        return Err(Violation::EndedWithUnsyncedEntry);
    }
    if unsynced != Unsynced::Nothing {
        return Err(Violation::EndedWithUnsyncedWrite);
    }
    Ok(())
}

/// The recording operating system: logs each call, then makes it for real.
#[derive(Debug, Default)]
struct Recording {
    calls: Mutex<Vec<Call>>,
}

impl Recording {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn record(&self, call: Call) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(call);
    }
}

impl Os for Recording {
    fn create_dir(&self, dir: &Path) -> io::Result<()> {
        self.record(Call::CreateDir(dir.to_path_buf()));
        RealOs.create_dir(dir)
    }

    fn create_journal(&self, path: &Path) -> io::Result<File> {
        self.record(Call::CreateJournal(path.to_path_buf()));
        RealOs.create_journal(path)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        self.record(Call::SyncDir(dir.to_path_buf()));
        RealOs.sync_dir(dir)
    }

    fn append(&self, journal: &File, bytes: &[u8]) -> io::Result<()> {
        self.record(Call::Append(bytes.len()));
        RealOs.append(journal, bytes)
    }

    fn truncate(&self, journal: &File, len: u64) -> io::Result<()> {
        self.record(Call::Truncate(len));
        RealOs.truncate(journal, len)
    }

    fn sync_data(&self, journal: &File) -> io::Result<()> {
        self.record(Call::SyncData);
        RealOs.sync_data(journal)
    }
}

/// Which durability call a [`Skipping`] system drops.
#[derive(Clone, Copy, Debug)]
enum Dropped {
    SyncData,
    SyncDir,
}

/// A defective operating system, standing in for an adapter that forgets a sync: it
/// reports the dropped call as done without making or recording it.
#[derive(Debug)]
struct Skipping {
    inner: Arc<Recording>,
    dropped: Dropped,
}

impl Os for Skipping {
    fn create_dir(&self, dir: &Path) -> io::Result<()> {
        self.inner.create_dir(dir)
    }

    fn create_journal(&self, path: &Path) -> io::Result<File> {
        self.inner.create_journal(path)
    }

    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        match self.dropped {
            Dropped::SyncDir => Ok(()),
            Dropped::SyncData => self.inner.sync_dir(dir),
        }
    }

    fn append(&self, journal: &File, bytes: &[u8]) -> io::Result<()> {
        self.inner.append(journal, bytes)
    }

    fn truncate(&self, journal: &File, len: u64) -> io::Result<()> {
        self.inner.truncate(journal, len)
    }

    fn sync_data(&self, journal: &File) -> io::Result<()> {
        match self.dropped {
            Dropped::SyncData => Ok(()),
            Dropped::SyncDir => self.inner.sync_data(journal),
        }
    }
}

// ---- scenarios: the real journal engine over the real adapter -------------------------

/// The repository's local cluster config.
const LOCAL: &str = include_str!("../../../config/local.toml");

const BOUNDS: LogBounds = LogBounds::new(ByteCount::new(1 << 20), ByteCount::new(1 << 40));

fn config() -> Config {
    Config::parse(LOCAL, Path::new("config")).unwrap()
}

fn node() -> NodeId {
    NodeId::new(2).unwrap()
}

/// A fresh data-directory path under the cargo target directory, not yet created.
fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let exe = std::env::current_exe().unwrap();
    // <target>/<profile>/deps/<test binary> -> <target>
    let base = exe.ancestors().nth(3).unwrap().join("fs-storage-scratch");
    std::fs::create_dir_all(&base).unwrap();
    base.join(format!(
        "order-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn genesis() -> Genesis {
    Genesis::new(&config(), node(), InstanceId::from_bytes([7; 16])).unwrap()
}

fn genesis_frame() -> Vec<u8> {
    encode(FrameKind::Genesis, 1, &genesis().encode()).unwrap()
}

/// The next batch for `log`: one entry after its last, in term 1.
fn next_batch(log: &LogicalLog) -> RaftBatch {
    let index = log.last_index().get() + 1;
    let entry = Entry {
        index: EntryIndex::new(index).unwrap(),
        term: Term::new(1).unwrap(),
        payload: EntryPayload::Command(vec![u8::try_from(index % 251).unwrap(); 24]),
    };
    let state = HardState {
        term: Term::new(1).unwrap(),
        vote: None,
        commit: LogIndex::new(index - 1).unwrap(),
    };
    RaftBatch::new(vec![entry], Some(state)).unwrap()
}

/// `init`'s sequence from §7.7 step 0 (as `server::init::initialise` runs it), through
/// `os`: create the directory and sync its parent; lock; create the journal unless one is
/// there; sync the directory; empty an incomplete journal; append and sync the genesis.
fn init(dir: &Path, os: &Arc<dyn Os>) {
    let _created = create_data_dir_with(dir, &**os).unwrap();
    let mut storage = FsStorage::open_through(dir, Arc::clone(os)).unwrap();
    storage.lock().unwrap();
    let len = storage.journal_len().ok();
    if len.is_none() {
        storage.create_journal().unwrap();
    }
    storage.sync_dir().unwrap();
    if len.is_some_and(|len| len.get() > 0) {
        storage.truncate(ByteCount::new(0)).unwrap();
    }
    storage.append(&genesis_frame()).unwrap();
    storage.sync().unwrap();
}

/// A node start on an initialised `dir`, as the Ready loop's journal use goes: lock,
/// recover, discard a torn tail and sync the truncation, then `batches` appends, each
/// synced before the next.
fn run(dir: &Path, os: &Arc<dyn Os>, batches: u32) -> bool {
    let mut storage = FsStorage::open_through(dir, Arc::clone(os)).unwrap();
    storage.lock().unwrap();
    let len = storage.journal_len().unwrap();
    let recovered = recover(&storage, len, &config(), node(), BOUNDS).unwrap();
    let tail = recovered.truncate_to();
    let (_genesis, mut log) = recovered.into_parts();
    if let Some(end) = tail {
        storage.truncate(end).unwrap();
        storage.sync().unwrap();
    }
    for _ in 0..batches {
        let frame = log.append(next_batch(&log)).unwrap();
        storage.append(&frame).unwrap();
        storage.sync().unwrap();
    }
    tail.is_some()
}

/// Leaves half of one more frame after the journal's end, as a crash mid-append would.
fn tear_the_tail(dir: &Path) {
    let storage = FsStorage::open_through(dir, Arc::new(RealOs)).unwrap();
    let len = storage.journal_len().unwrap();
    let recovered = recover(&storage, len, &config(), node(), BOUNDS).unwrap();
    let (_genesis, mut log) = recovered.into_parts();
    let frame = log.append(next_batch(&log)).unwrap();
    let mut journal = OpenOptions::new()
        .append(true)
        .open(dir.join(JOURNAL))
        .unwrap();
    journal.write_all(&frame[..frame.len() / 2]).unwrap();
    journal.sync_all().unwrap();
}

/// A recording system, and the same as the `dyn Os` the scenarios take.
fn recording() -> (Arc<Recording>, Arc<dyn Os>) {
    let recording = Recording::new();
    let os: Arc<dyn Os> = recording.clone();
    (recording, os)
}

/// Asserts `trace` satisfies every obligation.
fn assert_ordered(scenario: &str, trace: &[Call]) {
    assert_eq!(
        judge(trace),
        Ok(()),
        "rule: {scenario} issues its directory and data syncs in §7.4/DUR-7 order; \
         trace={trace:?}"
    );
}

#[test]
fn a_fresh_init_syncs_the_parent_then_the_directory_then_the_genesis() {
    let dir = scratch("init");
    let parent = parent_of(&dir);
    let (recording, os) = recording();
    init(&dir, &os);
    let genesis_len = genesis_frame().len();
    assert_eq!(
        recording.calls(),
        [
            Call::CreateDir(dir.clone()),
            Call::SyncDir(parent),
            Call::CreateJournal(dir.join(JOURNAL)),
            Call::SyncDir(dir),
            Call::Append(genesis_len),
            Call::SyncData,
        ],
        "rule: §7.7 step 0 makes the directory entry durable, then the journal's, then the \
         genesis (DUR-7, DUR-1)"
    );
    assert_ordered("a fresh init", &recording.calls());
}

#[test]
fn a_re_run_init_over_an_empty_journal_syncs_the_directory_again() {
    let dir = scratch("rerun-empty");
    let (first, first_os) = recording();
    // The interrupted first run: everything up to and including the journal's creation.
    assert_eq!(
        create_data_dir_with(&dir, &*first_os),
        Ok(crate::DataDirCreated::New)
    );
    let mut storage = FsStorage::open_through(&dir, Arc::clone(&first_os)).unwrap();
    storage.lock().unwrap();
    storage.create_journal().unwrap();
    drop(storage);
    assert_eq!(
        first.calls().len(),
        3,
        "fixture: mkdir, parent sync, create"
    );

    let (second, os) = recording();
    init(&dir, &os);
    let calls = second.calls();
    assert!(
        calls.contains(&Call::SyncDir(dir)),
        "rule: a re-run cannot know whether the first run's directory sync happened, so \
         it syncs the directory before writing (DUR-7); trace={calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|call| matches!(call, Call::CreateJournal(_))),
        "rule: a re-run keeps the journal the first run created; trace={calls:?}"
    );
    assert_ordered("a re-run init over an empty journal", &calls);
}

#[test]
fn a_re_run_init_over_an_incomplete_genesis_truncates_then_syncs_before_appending() {
    let dir = scratch("rerun-partial");
    let (_first, plain) = recording();
    init(&dir, &plain);
    // A crash mid-genesis: only the first half of the frame is in the journal.
    let frame = genesis_frame();
    OpenOptions::new()
        .write(true)
        .open(dir.join(JOURNAL))
        .unwrap()
        .set_len(u64::try_from(frame.len() / 2).unwrap())
        .unwrap();

    let (second, os) = recording();
    init(&dir, &os);
    let calls = second.calls();
    let truncate = calls
        .iter()
        .position(|call| *call == Call::Truncate(0))
        .unwrap_or_else(|| panic!("rule: the incomplete genesis is emptied; trace={calls:?}"));
    let append = calls
        .iter()
        .position(|call| matches!(call, Call::Append(_)))
        .unwrap_or_else(|| panic!("rule: the genesis is written; trace={calls:?}"));
    assert!(
        truncate < append,
        "rule: the journal is emptied before the genesis is appended; trace={calls:?}"
    );
    assert_ordered("a re-run init over an incomplete genesis", &calls);
}

#[test]
fn every_steady_state_batch_is_synced_before_the_next_is_appended() {
    let dir = scratch("steady");
    let (_init, plain) = recording();
    init(&dir, &plain);

    let (recording, os) = recording();
    assert!(!run(&dir, &os, 5), "fixture: a clean journal has no tail");
    let calls = recording.calls();
    assert_eq!(
        calls.len(),
        10,
        "rule: five batches are five appends and five syncs, nothing else; trace={calls:?}"
    );
    for pair in calls.chunks(2) {
        assert!(
            matches!(pair, [Call::Append(_), Call::SyncData]),
            "rule: each append is followed by its own fdatasync (§7.4 step 3); trace={calls:?}"
        );
    }
    assert_ordered("the steady state", &calls);
}

#[test]
fn recovering_a_torn_tail_syncs_the_truncation_before_the_next_append() {
    let dir = scratch("torn");
    let (_init, plain) = recording();
    init(&dir, &plain);
    {
        let (_run, plain) = recording();
        run(&dir, &plain, 2);
    }
    tear_the_tail(&dir);

    let (recording, os) = recording();
    assert!(run(&dir, &os, 2), "fixture: the torn tail is found");
    let calls = recording.calls();
    assert!(
        matches!(
            calls.as_slice(),
            [
                Call::Truncate(_),
                Call::SyncData,
                Call::Append(_),
                Call::SyncData,
                Call::Append(_),
                Call::SyncData
            ]
        ),
        "rule: the torn tail is truncated and the truncation synced before the next \
         append (§7.6); trace={calls:?}"
    );
    assert_ordered("recovering a torn tail", &calls);
}

#[test]
fn the_judge_rejects_a_journal_created_before_the_parent_is_synced() {
    let dir = PathBuf::from("/data/node");
    let trace = [
        Call::CreateDir(dir.clone()),
        Call::CreateJournal(dir.join(JOURNAL)),
        Call::SyncDir(PathBuf::from("/data")),
        Call::SyncDir(dir),
    ];
    assert_eq!(
        judge(&trace),
        Err(Violation::JournalBeforeParentSync { at: 1 }),
        "rule: a new directory's parent is synced before anything is created in it (DUR-7)"
    );
}

#[test]
fn the_judge_rejects_a_write_before_the_directory_entries_are_synced() {
    let dir = PathBuf::from("/data/node");
    let journal = Call::CreateJournal(dir.join(JOURNAL));
    assert_eq!(
        judge(&[Call::CreateDir(dir.clone()), Call::Append(8)]),
        Err(Violation::WriteBeforeParentSync { at: 1 }),
        "rule: nothing is written before the new directory's parent is synced (DUR-7)"
    );
    assert_eq!(
        judge(&[journal.clone(), Call::Append(8), Call::SyncDir(dir)]),
        Err(Violation::WriteBeforeDirectorySync { at: 1 }),
        "rule: nothing is written before the new journal's directory is synced (DUR-7)"
    );
    assert_eq!(
        judge(&[journal, Call::SyncData]),
        Err(Violation::WriteBeforeDirectorySync { at: 1 }),
        "rule: an fdatasync of a journal whose entry is not durable promises nothing (DUR-7)"
    );
}

#[test]
fn the_judge_rejects_a_second_write_over_an_unsynced_one() {
    assert_eq!(
        judge(&[Call::Append(8), Call::Append(8), Call::SyncData]),
        Err(Violation::WriteOverUnsynced { at: 1 }),
        "rule: at most one frame is unsynced (§7.4)"
    );
    assert_eq!(
        judge(&[Call::Truncate(3), Call::Append(8), Call::SyncData]),
        Err(Violation::WriteOverUnsynced { at: 1 }),
        "rule: the truncation is synced before the next append (§7.6)"
    );
}

#[test]
fn the_judge_accepts_an_emptied_journal_rewritten_under_one_fdatasync() {
    assert_eq!(
        judge(&[Call::Truncate(0), Call::Append(8), Call::SyncData]),
        Ok(()),
        "rule: init's re-run empties an incomplete genesis and rewrites it under one sync \
         (§7.7 step 0)"
    );
}

#[test]
fn the_judge_rejects_a_trace_that_ends_with_something_unsynced() {
    let dir = PathBuf::from("/data/node");
    assert_eq!(
        judge(&[Call::Append(8)]),
        Err(Violation::EndedWithUnsyncedWrite),
        "rule: no completion is reported over an unsynced append (§7.4)"
    );
    assert_eq!(
        judge(&[Call::Truncate(0)]),
        Err(Violation::EndedWithUnsyncedWrite),
        "rule: no completion is reported over an unsynced truncation (§7.6)"
    );
    assert_eq!(
        judge(&[Call::CreateDir(dir)]),
        Err(Violation::EndedWithUnsyncedEntry),
        "rule: a created directory's parent is synced before the call returns (DUR-7)"
    );
}

#[test]
fn an_adapter_that_forgets_the_fdatasync_is_caught_in_every_scenario() {
    let dir = scratch("mutant-data");
    let recording = Recording::new();
    let os: Arc<dyn Os> = Arc::new(Skipping {
        inner: Arc::clone(&recording),
        dropped: Dropped::SyncData,
    });
    init(&dir, &os);
    run(&dir, &os, 2);
    let trace = recording.calls();
    assert!(
        matches!(judge(&trace), Err(Violation::WriteOverUnsynced { .. })),
        "rule: a missing fdatasync leaves one append unsynced when the next arrives; \
         trace={trace:?}"
    );
}

#[test]
fn an_adapter_that_forgets_the_directory_sync_is_caught() {
    let dir = scratch("mutant-dir");
    let recording = Recording::new();
    let os: Arc<dyn Os> = Arc::new(Skipping {
        inner: Arc::clone(&recording),
        dropped: Dropped::SyncDir,
    });
    init(&dir, &os);
    let trace = recording.calls();
    assert!(
        matches!(
            judge(&trace),
            Err(Violation::JournalBeforeParentSync { .. })
        ),
        "rule: a missing directory sync is caught before the journal is created (DUR-7); \
         trace={trace:?}"
    );
}
