//! Real-file crash probes (PLAN.md §10.17, T-14; W24.1).
//!
//! A child process runs the real journal engine and the real file adapter against a real
//! data directory. Test-only failpoints (the `failpoints` feature, on in this build through
//! the crate's own dev-dependency) stop it at one boundary: before a write, mid-append,
//! after a write before sync, after sync before the completion is observed, during
//! directory creation, and during tail truncation. The parent kills the stopped child with
//! `SIGKILL` through its recorded handle, examines the files, and restarts: every synced
//! frame is present, a torn tail is discarded, nothing inside the durable prefix is
//! damaged, recovery never erases synced data, and a second process cannot take the lock
//! while the first holds it.
//!
//! **These are process-crash probes.** `SIGKILL` does not drop the page cache, so bytes
//! written but not synced survive the kill; a missing `fsync` cannot be seen this way
//! (that is the sync-order check, W24.2, and the power-loss assumption of §10.15). A case
//! therefore asserts what a process crash must leave: all acknowledged frames, and either
//! outcome for the one frame in flight, never anything else.
//!
//! The child is this test binary, re-executed on [`child_entry`]. The tests share one
//! lock: a process spawned while another test holds a `LOCK` open shares that open file
//! until it execs (see `lock_process.rs`), so no two tests may overlap.
#![cfg(test)]

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use fs_storage::failpoints::{self, MARKER, Point};
use fs_storage::{DataDirCreated, FsStorage, JOURNAL, create_data_dir};
use journal::{
    FrameKind, Genesis, LogBounds, LogicalLog, RaftBatch, RecoveryError, encode, recover,
};
use types::{
    ByteCount, ByteStorage as _, Config, DataDir as _, Entry, EntryIndex, EntryPayload, HardState,
    InstanceId, JournalRead as _, LogIndex, NodeId, StorageError, Term,
};

/// The repository's local cluster config.
const LOCAL: &str = include_str!("../../../config/local.toml");

/// The bounds the probes recover within: a 1 MiB entry cache and a hard stop far away.
const BOUNDS: LogBounds = LogBounds::new(ByteCount::new(1 << 20), ByteCount::new(1 << 40));

/// The environment variable that turns a re-executed test binary into a probe child. Its
/// value is `mode;dir;point;skip` (`point` empty when nothing is armed).
const CHILD: &str = "FS_STORAGE_CRASH_CHILD";

/// Batches the parent seeds before the child starts, and the child's three appends.
const SEEDED: u64 = 2;

/// Serialises the tests of this binary (see the module comment).
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

fn config() -> Config {
    Config::parse(LOCAL, Path::new("config")).unwrap()
}

fn node() -> NodeId {
    NodeId::new(2).unwrap()
}

/// The next batch for `log`: one entry after its last, in term 1, with a command that
/// names its index, committing the entry before it.
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

/// The genesis frame of the probes' journal.
fn genesis_frame() -> (Genesis, Vec<u8>) {
    let genesis = Genesis::new(&config(), node(), InstanceId::from_bytes([7; 16])).unwrap();
    let frame = encode(FrameKind::Genesis, 1, &genesis.encode()).unwrap();
    (genesis, frame)
}

/// The journal every correct run writes, to `batches` batches: the bytes, and the length
/// after the genesis (`ends[0]`) and after each batch.
struct Reference {
    bytes: Vec<u8>,
    ends: Vec<u64>,
}

impl Reference {
    fn new(batches: u64) -> Self {
        let (genesis, mut bytes) = genesis_frame();
        let mut ends = vec![bytes.len() as u64];
        let mut log = LogicalLog::after_genesis(ends[0], genesis.voters(), BOUNDS);
        for _ in 0..batches {
            let frame = log.append(next_batch(&log)).unwrap();
            bytes.extend_from_slice(&frame);
            ends.push(bytes.len() as u64);
        }
        Self { bytes, ends }
    }
}

/// A fresh path under the cargo target directory (never `/tmp`), not yet created.
fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let exe = std::env::current_exe().unwrap();
    // <target>/<profile>/deps/<test binary> -> <target>
    let target = exe.ancestors().nth(3).unwrap().to_path_buf();
    let base = target.join("fs-storage-scratch");
    std::fs::create_dir_all(&base).unwrap();
    base.join(format!(
        "crash-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A data directory holding the genesis and `batches` batches, all synced, and no lock.
fn seeded(name: &str, batches: u64) -> PathBuf {
    let dir = scratch(name);
    assert_eq!(create_data_dir(&dir), Ok(DataDirCreated::New));
    let reference = Reference::new(batches);
    let mut storage = FsStorage::open(&dir).unwrap();
    storage.lock().unwrap();
    storage.create_journal().unwrap();
    storage.append(&reference.bytes).unwrap();
    storage.sync().unwrap();
    storage.sync_dir().unwrap();
    dir
}

// ---- the child ----------------------------------------------------------------------

/// What a recovered journal holds, as the child reports it.
fn report(storage: &FsStorage) -> Result<(LogicalLog, Option<ByteCount>), RecoveryError> {
    let len = storage.journal_len().unwrap();
    let recovered = recover(storage, len, &config(), node(), BOUNDS)?;
    let tail = recovered.truncate_to();
    let (_genesis, log) = recovered.into_parts();
    Ok((log, tail))
}

fn say(line: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}").unwrap();
    out.flush().unwrap();
}

/// The child's work in `mode` on `dir`. Returns the exit code.
fn run_child(mode: &str, dir: &Path) -> i32 {
    match mode {
        // Initialise a data directory as `init` does (§7.7 step 0).
        "init" => {
            let _created = create_data_dir(dir).unwrap();
            let mut storage = FsStorage::open(dir).unwrap();
            storage.lock().unwrap();
            storage.create_journal().unwrap();
            storage.append(&genesis_frame().1).unwrap();
            storage.sync().unwrap();
            storage.sync_dir().unwrap();
            say("ACKED 0");
        }
        // Recover, lock held, three more appends each synced and then acknowledged.
        "append" => {
            let mut storage = FsStorage::open(dir).unwrap();
            storage.lock().unwrap();
            let (mut log, tail) = report(&storage).unwrap();
            assert_eq!(
                tail, None,
                "fixture: the append probes start from a clean journal"
            );
            for _ in 0..3 {
                let frame = log.append(next_batch(&log)).unwrap();
                storage.append(&frame).unwrap();
                storage.sync().unwrap();
                say(&format!("ACKED {}", log.last_index().get()));
            }
        }
        // A restart: lock, recover, discard a torn tail and sync, then report.
        "recover" => {
            let mut storage = FsStorage::open(dir).unwrap();
            storage.lock().unwrap();
            match report(&storage) {
                Ok((log, tail)) => {
                    if let Some(end) = tail {
                        storage.truncate(end).unwrap();
                        storage.sync().unwrap();
                    }
                    say(&format!(
                        "RECOVERED {} {} {}",
                        log.last_index().get(),
                        storage.journal_len().unwrap().get(),
                        tail.is_some()
                    ));
                }
                Err(error) => say(&format!("REFUSED {error:?}")),
            }
        }
        // Only report whether the lock can be taken.
        "trylock" => {
            let mut storage = FsStorage::open(dir).unwrap();
            match storage.lock() {
                Ok(()) => say("LOCK ok"),
                Err(StorageError::Locked) => say("LOCK locked"),
                Err(error) => say(&format!("LOCK error {error:?}")),
            }
        }
        other => {
            say(&format!("unknown mode {other}"));
            return 2;
        }
    }
    0
}

/// Not a test: the entry the parent re-executes this binary on. Run normally it does
/// nothing.
#[test]
fn child_entry() {
    let Some(spec) = std::env::var_os(CHILD) else {
        return;
    };
    let spec = spec.to_string_lossy().into_owned();
    let mut parts = spec.split(';');
    let (mode, dir, point, skip) = (
        parts.next().unwrap(),
        PathBuf::from(parts.next().unwrap()),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );
    if let Some(point) = Point::parse(point) {
        failpoints::arm(point, skip.parse().unwrap());
    }
    std::process::exit(run_child(mode, &dir));
}

// ---- the parent ---------------------------------------------------------------------

/// A running child, killed (and reaped) when dropped.
struct Probe {
    child: Child,
    lines: BufReader<ChildStdout>,
    /// The last index the child acknowledged, from its `ACKED` lines.
    acked: Option<u64>,
    /// Every other line the child printed.
    output: Vec<String>,
}

impl Probe {
    fn spawn(mode: &str, dir: &Path, armed: Option<(Point, u32)>) -> Self {
        let (point, skip) = armed.map_or(("", 0), |(point, skip)| (point.name(), skip));
        let mut child = Command::new("/proc/self/exe")
            .args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
            .env(CHILD, format!("{mode};{};{point};{skip}", dir.display()))
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            lines,
            acked: None,
            output: Vec::new(),
        }
    }

    /// Reads the child's output until it stops at its failpoint (returning the point) or
    /// exits (returning `None`).
    fn until_failpoint(&mut self) -> Option<String> {
        let mut line = String::new();
        loop {
            line.clear();
            if self.lines.read_line(&mut line).unwrap() == 0 {
                return None;
            }
            // libtest starts the child's first output line on its own "test ... " header.
            let text = line.trim_end();
            let text = text.strip_prefix("test child_entry ... ").unwrap_or(text);
            if let Some(point) = text.strip_prefix(&format!("{MARKER} ")) {
                return Some(point.to_owned());
            }
            if let Some(index) = text.strip_prefix("ACKED ") {
                self.acked = Some(index.parse().unwrap());
            } else {
                self.output.push(text.to_owned());
            }
        }
    }

    /// `SIGKILL`s the child through its handle and checks that is how it ended.
    fn sigkill(&mut self) {
        self.child.kill().unwrap();
        let status = self.child.wait().unwrap();
        assert_eq!(
            status.signal(),
            Some(9),
            "rule: the probe ends its child with SIGKILL; status={status:?}"
        );
    }

    /// Lets the child run to its end and returns its exit code.
    fn finish(&mut self) -> Option<i32> {
        assert_eq!(
            self.until_failpoint(),
            None,
            "rule: an unarmed child never stops at a failpoint; output={:?}",
            self.output
        );
        self.child.wait().unwrap().code()
    }

    /// The first output line starting with `prefix`.
    fn line(&self, prefix: &str) -> Option<&str> {
        self.output
            .iter()
            .map(String::as_str)
            .find(|line| line.starts_with(prefix))
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        // Already waited for, or still running: either way the handle is ours alone.
        let _killed = self.child.kill();
        let _reaped = self.child.wait();
    }
}

/// What the files of a killed run hold.
#[derive(Debug)]
struct Seen {
    /// The journal's bytes.
    bytes: Vec<u8>,
    /// Entries in the recovered durable prefix.
    entries: u64,
    /// The torn tail recovery would discard: where it starts and how long it is.
    tail: Option<(u64, u64)>,
}

/// Recovers `dir` as a restarting node would, without changing it.
fn examine(dir: &Path) -> Result<Seen, RecoveryError> {
    let storage = FsStorage::open(dir).unwrap();
    let len = storage.journal_len().unwrap();
    let bytes = types::ReadLen::new(u32::try_from(len.get()).unwrap())
        .map(|wanted| storage.read_at(ByteCount::new(0), wanted).unwrap())
        .unwrap_or_default();
    let recovered = recover(&storage, len, &config(), node(), BOUNDS)?;
    let end = recovered.truncate_to();
    let entries = recovered.log().last_index().get();
    Ok(Seen {
        tail: end.map(|end| (end.get(), len.get() - end.get())),
        bytes,
        entries,
    })
}

/// Asserts nothing in the durable prefix of `seen` differs from what a correct run wrote.
fn assert_prefix_intact(seen: &Seen, reference: &Reference, case: &str) {
    let end = seen
        .tail
        .map_or(seen.bytes.len() as u64, |(start, _)| start);
    assert_eq!(
        Some(&end),
        reference.ends.get(usize::try_from(seen.entries).unwrap()),
        "rule: the durable prefix ends on a frame boundary of the reference journal; \
         case={case} seen={seen:?}"
    );
    let end = usize::try_from(end).unwrap();
    assert!(
        seen.bytes[..end] == reference.bytes[..end],
        "rule: nothing inside the durable prefix is damaged (DUR-1); case={case} end={end}"
    );
}

/// Asserts the lock state of `dir`: held by the stopped child, free once it is killed.
fn assert_locked(dir: &Path, case: &str) {
    let mut other = FsStorage::open(dir).unwrap();
    assert_eq!(
        other.lock(),
        Err(StorageError::Locked),
        "rule: a second process cannot take the lock while the first holds it (DUR-8); \
         case={case}"
    );
}

fn assert_lock_released(dir: &Path, case: &str) {
    let mut other = FsStorage::open(dir).unwrap();
    assert_eq!(
        other.lock(),
        Ok(()),
        "rule: the kernel releases a killed process's lock; case={case}"
    );
}

/// Runs an `append` child armed at `point` (second append of the child), kills it there,
/// and returns what its files hold, after checking the lock and the durable prefix.
fn crashed_append(point: Point) -> (Seen, Reference, u64) {
    let _serial = serial();
    let case = point.name();
    let dir = seeded(case, SEEDED);
    let mut probe = Probe::spawn("append", &dir, Some((point, 1)));
    assert_eq!(
        probe.until_failpoint().as_deref(),
        Some(case),
        "rule: the child stops at its armed failpoint; case={case} output={:?}",
        probe.output
    );
    assert_locked(&dir, case);
    probe.sigkill();
    assert_lock_released(&dir, case);
    let acked = probe
        .acked
        .expect("the child acknowledged an append before stopping");
    assert_eq!(
        acked,
        SEEDED + 1,
        "fixture: one append acknowledged before the stop"
    );
    let reference = Reference::new(SEEDED + 3);
    let seen = examine(&dir).unwrap();
    assert!(
        seen.entries >= acked,
        "rule: every synced (acknowledged) frame is present (DUR-1); case={case} \
         acked={acked} seen={seen:?}"
    );
    assert_prefix_intact(&seen, &reference, case);
    // The restart: lock, recover, discard any tail. It never erases a synced frame.
    drop(probe);
    let mut restart = Probe::spawn("recover", &dir, None);
    assert_eq!(
        restart.finish(),
        Some(0),
        "restart output={:?}",
        restart.output
    );
    let line = restart
        .line("RECOVERED")
        .expect("the restart reports")
        .to_owned();
    let fields: Vec<&str> = line.split(' ').collect();
    let (entries, len): (u64, u64) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    assert_eq!(
        (entries, Some(&len)),
        (
            seen.entries,
            reference.ends.get(usize::try_from(seen.entries).unwrap())
        ),
        "rule: a restart keeps exactly the durable prefix and ends the journal there \
         (DUR-3); case={case} line={line}"
    );
    (seen, reference, acked)
}

// check: CRASH-PROBES
#[test]
fn a_kill_before_a_write_leaves_exactly_the_synced_frames() {
    let (seen, _, acked) = crashed_append(Point::BeforeAppend);
    assert_eq!(
        (seen.entries, seen.tail),
        (acked, None),
        "rule: nothing was written, so the journal ends on the last acknowledged frame; \
         seen={seen:?}"
    );
}

#[test]
fn a_kill_mid_append_leaves_a_torn_tail_that_recovery_discards() {
    let (seen, reference, acked) = crashed_append(Point::MidAppend);
    let frame = reference.ends[usize::try_from(acked + 1).unwrap()]
        - reference.ends[usize::try_from(acked).unwrap()];
    assert_eq!(
        (seen.entries, seen.tail),
        (
            acked,
            Some((reference.ends[usize::try_from(acked).unwrap()], frame / 2))
        ),
        "rule: half a frame is a torn tail after the durable prefix (DUR-3); seen={seen:?}"
    );
}

#[test]
fn a_kill_after_a_write_before_sync_keeps_every_acknowledged_frame() {
    let (seen, _, acked) = crashed_append(Point::AfterAppend);
    assert_eq!(
        (seen.entries, seen.tail),
        (acked + 1, None),
        "rule: a process crash keeps the unsynced but fully written frame (the page cache \
         survives SIGKILL), and loses none acknowledged; seen={seen:?}"
    );
}

#[test]
fn a_kill_after_sync_before_the_completion_is_observed_keeps_the_frame() {
    let (seen, _, acked) = crashed_append(Point::AfterSync);
    assert_eq!(
        (seen.entries, seen.tail),
        (acked + 1, None),
        "rule: a frame synced but not yet acknowledged is present: the caller may retry it \
         but must not find it gone (DUR-1); seen={seen:?}"
    );
}

/// Seeds a journal with a torn tail (half of the next frame), kills a restart at `point`
/// during the tail's truncation, and returns what the files hold.
fn crashed_truncation(point: Point) -> (Seen, Reference) {
    let _serial = serial();
    let case = point.name();
    let dir = seeded(case, SEEDED);
    let reference = Reference::new(SEEDED + 1);
    let end = usize::try_from(reference.ends[usize::try_from(SEEDED).unwrap()]).unwrap();
    let torn = &reference.bytes[end..end + (reference.bytes.len() - end) / 2];
    {
        let mut storage = FsStorage::open(&dir).unwrap();
        storage.append(torn).unwrap();
        storage.sync().unwrap();
    }
    let before = examine(&dir).unwrap();
    assert_eq!(
        (before.entries, before.tail.map(|(start, _)| start)),
        (SEEDED, Some(end as u64)),
        "fixture: the seeded journal has a torn tail; before={before:?}"
    );
    let mut probe = Probe::spawn("recover", &dir, Some((point, 0)));
    assert_eq!(
        probe.until_failpoint().as_deref(),
        Some(case),
        "output={:?}",
        probe.output
    );
    assert_locked(&dir, case);
    probe.sigkill();
    assert_lock_released(&dir, case);
    let seen = examine(&dir).unwrap();
    assert_eq!(
        seen.entries, SEEDED,
        "rule: a kill during truncation never erases a synced frame (DUR-3); case={case} \
         seen={seen:?}"
    );
    assert_prefix_intact(&seen, &reference, case);
    drop(probe);
    let mut restart = Probe::spawn("recover", &dir, None);
    assert_eq!(
        restart.finish(),
        Some(0),
        "restart output={:?}",
        restart.output
    );
    assert_eq!(
        restart.line("RECOVERED").map(str::to_owned),
        Some(format!("RECOVERED {SEEDED} {end} {}", seen.tail.is_some())),
        "rule: the restart ends the journal on the durable prefix; case={case}"
    );
    assert_eq!(
        examine(&dir).unwrap().tail,
        None,
        "rule: the tail is gone after the restart"
    );
    (seen, reference)
}

#[test]
fn a_kill_before_the_tail_is_truncated_leaves_it_for_the_next_restart() {
    let (seen, _) = crashed_truncation(Point::BeforeTruncate);
    assert!(
        seen.tail.is_some(),
        "rule: nothing was truncated yet, so the tail is still there to discard; seen={seen:?}"
    );
}

#[test]
fn a_kill_after_the_tail_is_truncated_before_its_sync_leaves_the_prefix() {
    let (seen, reference) = crashed_truncation(Point::AfterTruncate);
    assert_eq!(
        (seen.tail, seen.bytes.len() as u64),
        (None, reference.ends[usize::try_from(SEEDED).unwrap()]),
        "rule: the truncation took effect (the page cache survives SIGKILL); seen={seen:?}"
    );
}

#[test]
fn a_kill_after_the_data_directory_is_made_leaves_a_directory_init_can_resume() {
    let _serial = serial();
    let dir = scratch("mkdir");
    let mut probe = Probe::spawn("init", &dir, Some((Point::AfterMkdir, 0)));
    assert_eq!(
        probe.until_failpoint().as_deref(),
        Some("after-mkdir"),
        "output={:?}",
        probe.output
    );
    probe.sigkill();
    assert!(
        dir.is_dir() && !dir.join(JOURNAL).exists(),
        "rule: the directory exists and nothing is in it; dir={dir:?}"
    );
    drop(probe);
    let mut restart = Probe::spawn("init", &dir, None);
    assert_eq!(
        restart.finish(),
        Some(0),
        "restart output={:?}",
        restart.output
    );
    assert_eq!(
        restart.acked,
        Some(0),
        "rule: init re-runs to completion in the directory"
    );
    let seen = examine(&dir).unwrap();
    assert_eq!(
        (seen.entries, seen.tail),
        (0, None),
        "rule: the genesis is whole; seen={seen:?}"
    );
    assert_prefix_intact(&seen, &Reference::new(0), "mkdir");
}

#[test]
fn a_kill_after_the_journal_is_created_leaves_an_empty_journal_recovery_names() {
    let _serial = serial();
    let dir = scratch("create-journal");
    let mut probe = Probe::spawn("init", &dir, Some((Point::AfterJournalCreate, 0)));
    assert_eq!(
        probe.until_failpoint().as_deref(),
        Some("after-journal-create"),
        "output={:?}",
        probe.output
    );
    assert_locked(&dir, "after-journal-create");
    probe.sigkill();
    assert_lock_released(&dir, "after-journal-create");
    assert!(
        matches!(examine(&dir), Err(RecoveryError::Empty)),
        "rule: an empty journal is told apart from corruption, so init may continue (D-52)"
    );
}

#[test]
fn a_kill_mid_genesis_leaves_an_interrupted_init_recovery_names() {
    let _serial = serial();
    let dir = scratch("mid-genesis");
    let mut probe = Probe::spawn("init", &dir, Some((Point::MidAppend, 0)));
    assert_eq!(
        probe.until_failpoint().as_deref(),
        Some("mid-append"),
        "output={:?}",
        probe.output
    );
    probe.sigkill();
    let seen = examine(&dir);
    assert!(
        matches!(seen, Err(RecoveryError::IncompleteGenesis)),
        "rule: half a genesis frame is an interrupted init, not a corrupt journal (§7.7); \
         seen={seen:?}"
    );
    let mut restart = Probe::spawn("recover", &dir, None);
    assert_eq!(
        restart.finish(),
        Some(0),
        "restart output={:?}",
        restart.output
    );
    assert!(
        restart.line("REFUSED IncompleteGenesis").is_some(),
        "rule: a restart refuses the half-initialised node and changes nothing; output={:?}",
        restart.output
    );
}

#[test]
fn a_kill_after_the_genesis_sync_leaves_a_journal_that_starts() {
    let _serial = serial();
    let dir = scratch("genesis-sync");
    let mut probe = Probe::spawn("init", &dir, Some((Point::AfterSync, 0)));
    assert_eq!(
        probe.until_failpoint().as_deref(),
        Some("after-sync"),
        "output={:?}",
        probe.output
    );
    probe.sigkill();
    let seen = examine(&dir).unwrap();
    assert_eq!((seen.entries, seen.tail), (0, None), "seen={seen:?}");
    assert_prefix_intact(&seen, &Reference::new(0), "genesis-sync");
}

#[test]
fn a_second_process_cannot_take_the_lock_while_the_first_is_stopped_holding_it() {
    let _serial = serial();
    let dir = seeded("lock", SEEDED);
    let mut first = Probe::spawn("append", &dir, Some((Point::BeforeAppend, 0)));
    assert_eq!(first.until_failpoint().as_deref(), Some("before-append"));
    let mut second = Probe::spawn("trylock", &dir, None);
    assert_eq!(second.finish(), Some(0));
    assert_eq!(
        second.line("LOCK"),
        Some("LOCK locked"),
        "rule: the second process is refused while the first holds the lock (DUR-8)"
    );
    first.sigkill();
    let mut third = Probe::spawn("trylock", &dir, None);
    assert_eq!(third.finish(), Some(0));
    assert_eq!(
        third.line("LOCK"),
        Some("LOCK ok"),
        "rule: the lock is free once the holder is killed"
    );
}

#[test]
fn every_failpoint_has_a_probe() {
    let source = include_str!("crash_probes.rs");
    for point in Point::ALL {
        let variant = format!("Point::{point:?}");
        assert!(
            source.matches(&variant).count() >= 1,
            "rule: every failpoint is armed by some probe; point={point:?}"
        );
    }
}
