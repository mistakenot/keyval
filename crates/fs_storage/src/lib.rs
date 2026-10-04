//! keyval's real-file byte-storage adapter (PLAN.md §7.2, §7.6, §7.7; W12.3).
//!
//! One data directory holds one append-only journal file, [`JOURNAL`], and a [`LOCK`]
//! file giving one process exclusive ownership of the directory (DUR-8). [`FsStorage`]
//! implements the core's [`ByteStorage`] port over them, and it only translates (CD-1):
//! - appends go to the end of `journal.log` (append mode), and reads are positional and
//!   bounded ([`ReadLen`]). A read past the end is [`StorageError::ShortRead`], never fewer
//!   bytes without an error;
//! - [`ByteStorage::create_journal`] never replaces an existing journal (`create_new`), and
//!   its directory entry becomes durable at the next [`ByteStorage::sync_dir`] (DUR-7);
//! - [`ByteStorage::truncate`] shortens the file and refuses to lengthen it;
//! - [`ByteStorage::sync`] is `fdatasync`, which also makes a truncation's new length
//!   durable, and [`ByteStorage::sync_dir`] is `fsync` of the data directory;
//! - [`DataDir::lock`] takes an exclusive advisory lock (`flock`) on `LOCK` and holds it
//!   until the handle is dropped. `flock` belongs to the open file, so a second handle is
//!   refused whether it is in another process or in this one.
//!
//! [`create_data_dir`] is §7.7 step 0's first step: create the directory and sync its
//! parent, so its entry is durable before anything is written into it (DUR-7).
//!
//! A [`StorageError`] carries no cause, so the adapter keeps the last one it saw
//! ([`FsStorage::last_failure`]) for the composition root to log. Each call returns only
//! the errors its contract names: [`StorageError::Missing`] means a missing data
//! directory, parent or journal, never a path that could not be examined.
//!
//! **A failed write or sync latches the handle** (§7.11; br-zswx). After the operating
//! system reports a failed append, truncate, sync or directory sync, what the journal
//! holds durably is unknown: a failed `fdatasync` may already have dropped the dirty
//! pages it reported, so a later one can succeed without them. Every later append,
//! truncate, sync, directory sync and journal creation on that handle is therefore
//! [`StorageError::Failed`] ([`FsStorage::latched`]), so no completion can claim
//! durability the failure undid. A refusal that changes nothing (no journal yet, a
//! lengthening truncate, an existing journal) does not latch.
//!
//! [`ReadOnlyDir`] is the doctor's handle (O-7, W23.1): it opens the journal for reading
//! only, never creates or writes anything, and only probes the lock
//! ([`ReadOnlyDir::probe_lock`]), holding it while it reads so that no node starts
//! meanwhile. It has no [`ByteStorage`] implementation, so it cannot write.
//!
//! The adapter-agnostic contract suite (`crates/types/tests/storage_contract`) runs
//! against it unmodified (`tests/contract.rs`, check FS-STORAGE); crash behaviour is
//! tested by W24's crash probes.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, ErrorKind, Write as _};
use std::os::unix::fs::FileExt as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use types::{ByteCount, ByteStorage, DataDir, JournalRead, ReadLen, StorageError};

/// The journal's file name in the data directory (§7.6).
pub const JOURNAL: &str = "journal.log";

/// The lock file's name in the data directory (§7.6, DUR-8).
pub const LOCK: &str = "LOCK";

/// Why a file operation failed: what the adapter tried, on which path, and the operating
/// system's answer. [`StorageError`] carries none of this; a root logs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// The operation, e.g. `append` or `sync_dir`.
    pub op: &'static str,
    /// The file or directory it concerned.
    pub path: PathBuf,
    /// The cause, as the operating system or the adapter reported it.
    pub cause: String,
    /// The error the port reported.
    pub error: StorageError,
}

impl Failure {
    /// A failure of `op` on `path` caused by the operating system's `err`: always
    /// [`StorageError::Failed`]. Only the calls whose contract names
    /// [`StorageError::Missing`] use [`Failure::io_or_missing`] (br-zswx).
    fn io(op: &'static str, path: &Path, err: &io::Error) -> Self {
        Self {
            op,
            path: path.to_path_buf(),
            cause: err.to_string(),
            error: StorageError::Failed,
        }
    }

    /// As [`Failure::io`], except that a path that does not exist, or that runs through
    /// something other than a directory, is [`StorageError::Missing`]. Every other error
    /// (permission denied, a symlink loop, an I/O error) is still
    /// [`StorageError::Failed`]: a directory that cannot be examined is not missing.
    fn io_or_missing(op: &'static str, path: &Path, err: &io::Error) -> Self {
        let missing = matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory);
        Self {
            error: if missing {
                StorageError::Missing
            } else {
                StorageError::Failed
            },
            ..Self::io(op, path, err)
        }
    }
}

/// Creates the data directory `dir` and syncs its parent, so the new entry is durable
/// before anything depends on it (§7.7 step 0, DUR-7). An existing directory is refused:
/// initialisation never reuses one.
///
/// # Errors
///
/// A [`Failure`] whose error is [`StorageError::Missing`] when the parent does not exist,
/// and [`StorageError::Failed`] when `dir` exists or either step fails.
pub fn create_data_dir(dir: &Path) -> Result<(), Failure> {
    fs::create_dir(dir).map_err(|err| Failure::io_or_missing("create_data_dir", dir, &err))?;
    let parent = dir
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    sync_directory(parent).map_err(|err| Failure::io("sync_parent_dir", parent, &err))
}

/// `fsync` of a directory: makes the entries created in it durable.
fn sync_directory(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// One process's handle on a data directory: the journal, once it exists, and the lock,
/// once taken. Dropping the handle closes both and so releases the lock.
#[derive(Debug)]
pub struct FsStorage {
    /// The data directory.
    dir: PathBuf,
    /// `journal.log`, open for reading and appending, once it exists.
    journal: Option<File>,
    /// `LOCK`, locked exclusively, once [`DataDir::lock`] succeeded.
    lock: Option<File>,
    /// The failed write or sync that latched the handle, if one did (br-zswx).
    latched: Option<Failure>,
    /// The cause of the last failure, for the root's log.
    last_failure: Mutex<Option<Failure>>,
}

impl FsStorage {
    /// Opens the data directory `dir` as a starting process does: the journal, if it
    /// exists, is opened; nothing is created and the lock is not taken.
    ///
    /// # Errors
    ///
    /// A [`Failure`] whose error is [`StorageError::Missing`] if `dir` does not exist or
    /// is not a directory, or [`StorageError::Failed`] if it cannot be examined (for
    /// example, permission denied) or the journal cannot be opened.
    pub fn open(dir: &Path) -> Result<Self, Failure> {
        Self::open_journal(dir, true)
    }

    /// Opens `dir`'s journal, if it exists, for reading, and for appending when
    /// `writable`. Nothing is created and the lock is not taken.
    fn open_journal(dir: &Path, writable: bool) -> Result<Self, Failure> {
        let metadata =
            fs::metadata(dir).map_err(|err| Failure::io_or_missing("open", dir, &err))?;
        if !metadata.is_dir() {
            return Err(Failure {
                op: "open",
                path: dir.to_path_buf(),
                cause: "not a directory".to_owned(),
                error: StorageError::Missing,
            });
        }
        let path = dir.join(JOURNAL);
        let journal = match OpenOptions::new().read(true).append(writable).open(&path) {
            Ok(file) => Some(file),
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => return Err(Failure::io("open_journal", &path, &err)),
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            journal,
            lock: None,
            latched: None,
            last_failure: Mutex::new(None),
        })
    }

    /// The data directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The cause of the most recent failed call, if any failed.
    #[must_use]
    pub fn last_failure(&self) -> Option<Failure> {
        self.last_failure.lock().ok().and_then(|slot| slot.clone())
    }

    /// Records `failure` as the last one and returns the port's error.
    fn fail(&self, failure: Failure) -> StorageError {
        let error = failure.error;
        if let Ok(mut slot) = self.last_failure.lock() {
            *slot = Some(failure);
        }
        error
    }

    /// The failed write or sync that latched this handle, if one did: from then on every
    /// append, truncate, sync, directory sync and journal creation is
    /// [`StorageError::Failed`] (br-zswx).
    #[must_use]
    pub const fn latched(&self) -> Option<&Failure> {
        self.latched.as_ref()
    }

    /// Refuses `op` once the handle has latched: after a failed write or sync, nothing
    /// since the last good sync is known durable, so no later call may report success.
    fn refuse_if_latched(&self, op: &'static str) -> Result<(), StorageError> {
        self.latched.as_ref().map_or(Ok(()), |first| {
            Err(self.fail(Failure {
                op,
                path: first.path.clone(),
                cause: format!(
                    "refused: an earlier {} failed ({}), so what the journal holds durably \
                     is unknown",
                    first.op, first.cause
                ),
                error: StorageError::Failed,
            }))
        })
    }

    /// Records the failed write or sync `failure` and latches the handle on it; the first
    /// such failure is the one kept.
    fn latch(&mut self, failure: Failure) -> StorageError {
        if self.latched.is_none() {
            self.latched = Some(failure.clone());
        }
        self.fail(failure)
    }

    /// Records the failed write or sync `op` of the journal and latches the handle.
    fn latch_journal(&mut self, op: &'static str, err: &io::Error) -> StorageError {
        let failure = Failure::io(op, &self.dir.join(JOURNAL), err);
        self.latch(failure)
    }

    /// Records an I/O error of `op` on the journal.
    fn journal_error(&self, op: &'static str, err: &io::Error) -> StorageError {
        self.fail(Failure::io(op, &self.dir.join(JOURNAL), err))
    }

    /// The open journal, or [`StorageError::Missing`] without one.
    fn journal(&self, op: &'static str) -> Result<&File, StorageError> {
        self.journal.as_ref().ok_or_else(|| {
            self.fail(Failure {
                op,
                path: self.dir.join(JOURNAL),
                cause: "the data directory has no journal".to_owned(),
                error: StorageError::Missing,
            })
        })
    }

    /// The journal's length in bytes.
    fn len_of(&self, op: &'static str) -> Result<u64, StorageError> {
        let file = self.journal(op)?;
        file.metadata()
            .map(|metadata| metadata.len())
            .map_err(|err| self.journal_error(op, &err))
    }
}

impl JournalRead for FsStorage {
    fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
        let end = self.len_of("read_at")?;
        let wanted = u64::from(len.get());
        let available = end.saturating_sub(offset.get());
        if available < wanted {
            // `available` is below `wanted`, which is at most ReadLen::MAX.
            let got = u32::try_from(available).unwrap_or(u32::MAX);
            return Err(StorageError::ShortRead { wanted: len, got });
        }
        let mut bytes = vec![0; usize::try_from(wanted).map_err(|_| StorageError::Failed)?];
        self.journal("read_at")?
            .read_exact_at(&mut bytes, offset.get())
            .map_err(|err| self.journal_error("read_at", &err))?;
        Ok(bytes)
    }
}

impl DataDir for FsStorage {
    fn lock(&mut self) -> Result<(), StorageError> {
        if self.lock.is_some() {
            return Ok(());
        }
        let path = self.dir.join(LOCK);
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|err| self.fail(Failure::io("lock", &path, &err)))?;
        match file.try_lock() {
            Ok(()) => {
                self.lock = Some(file);
                Ok(())
            }
            Err(TryLockError::WouldBlock) => Err(self.fail(Failure {
                op: "lock",
                path,
                cause: "another process holds the data directory's lock".to_owned(),
                error: StorageError::Locked,
            })),
            Err(TryLockError::Error(err)) => Err(self.fail(Failure::io("lock", &path, &err))),
        }
    }
}

/// What probing a data directory's lock found ([`ReadOnlyDir::probe_lock`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockProbe {
    /// There is no `LOCK` file: no process ever took the directory's lock.
    NoLockFile,
    /// The lock was free, and the probing handle now holds it until it is dropped, so no
    /// node starts while the doctor reads.
    Acquired,
    /// Another process holds the lock: the directory is live, and its journal may be
    /// mid-append (O-7).
    Live,
}

/// A data directory opened for examination only (the doctor, O-7).
///
/// The journal is opened for reading, nothing is created or written, and the lock is only
/// probed. It implements [`JournalRead`] and no [`ByteStorage`], so it cannot write.
#[derive(Debug)]
pub struct ReadOnlyDir(FsStorage);

impl ReadOnlyDir {
    /// Opens the data directory `dir` for reading: its journal, if it exists, read-only.
    ///
    /// # Errors
    ///
    /// A [`Failure`] whose error is [`StorageError::Missing`] if `dir` does not exist or
    /// is not a directory, or [`StorageError::Failed`] if it cannot be examined or the
    /// journal cannot be opened.
    pub fn open(dir: &Path) -> Result<Self, Failure> {
        FsStorage::open_journal(dir, false).map(Self)
    }

    /// The data directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        self.0.dir()
    }

    /// The cause of the most recent failed call, if any failed.
    #[must_use]
    pub fn last_failure(&self) -> Option<Failure> {
        self.0.last_failure()
    }

    /// The journal's length.
    ///
    /// # Errors
    ///
    /// [`StorageError::Missing`] without a journal; [`StorageError::Failed`] if its length
    /// cannot be read.
    pub fn journal_len(&self) -> Result<ByteCount, StorageError> {
        self.0.journal_len()
    }

    /// Probes the directory's lock without creating anything: an existing `LOCK` file is
    /// opened read-only and locked if it is free, and held until this handle is dropped.
    ///
    /// # Errors
    ///
    /// [`StorageError::Failed`] if `LOCK` exists but cannot be opened or locked.
    pub fn probe_lock(&mut self) -> Result<LockProbe, StorageError> {
        if self.0.lock.is_some() {
            return Ok(LockProbe::Acquired);
        }
        let path = self.0.dir.join(LOCK);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(LockProbe::NoLockFile),
            Err(err) => return Err(self.0.fail(Failure::io("probe_lock", &path, &err))),
        };
        match file.try_lock() {
            Ok(()) => {
                self.0.lock = Some(file);
                Ok(LockProbe::Acquired)
            }
            Err(TryLockError::WouldBlock) => Ok(LockProbe::Live),
            Err(TryLockError::Error(err)) => {
                Err(self.0.fail(Failure::io("probe_lock", &path, &err)))
            }
        }
    }
}

impl JournalRead for ReadOnlyDir {
    fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
        self.0.read_at(offset, len)
    }
}

impl ByteStorage for FsStorage {
    fn journal_len(&self) -> Result<ByteCount, StorageError> {
        self.len_of("journal_len").map(ByteCount::new)
    }

    fn create_journal(&mut self) -> Result<(), StorageError> {
        self.refuse_if_latched("create_journal")?;
        let path = self.dir.join(JOURNAL);
        let created = OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .open(&path);
        match created {
            Ok(file) => {
                self.journal = Some(file);
                Ok(())
            }
            // An existing journal is refused, never replaced; nothing changed, so no latch.
            Err(err) => Err(self.fail(Failure::io("create_journal", &path, &err))),
        }
    }

    fn sync_dir(&mut self) -> Result<(), StorageError> {
        self.refuse_if_latched("sync_dir")?;
        sync_directory(&self.dir).map_err(|err| {
            let failure = Failure::io("sync_dir", &self.dir, &err);
            self.latch(failure)
        })
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), StorageError> {
        self.refuse_if_latched("append")?;
        let result = self.journal("append")?.write_all(bytes);
        result.map_err(|err| self.latch_journal("append", &err))
    }

    fn truncate(&mut self, len: ByteCount) -> Result<(), StorageError> {
        self.refuse_if_latched("truncate")?;
        let end = self.len_of("truncate")?;
        if len.get() > end {
            return Err(self.fail(Failure {
                op: "truncate",
                path: self.dir.join(JOURNAL),
                cause: format!("{} bytes is past the journal's end at {end}", len.get()),
                error: StorageError::Failed,
            }));
        }
        let result = self.journal("truncate")?.set_len(len.get());
        result.map_err(|err| self.latch_journal("truncate", &err))
    }

    fn sync(&mut self) -> Result<(), StorageError> {
        self.refuse_if_latched("sync")?;
        let result = self.journal("sync")?.sync_data();
        result.map_err(|err| self.latch_journal("sync", &err))
    }
}

#[cfg(test)]
mod tests {
    // check: FS-STORAGE
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// The environment variable that turns a re-executed test binary into the second
    /// process of [`a_second_process_is_refused_the_lock_until_the_first_exits`].
    const PROBE: &str = "FS_STORAGE_LOCK_PROBE";

    /// What the probe process reports through its exit code.
    const LOCKED: i32 = 3;
    const LOCKED_OK: i32 = 4;
    const PROBE_FAILED: i32 = 5;

    /// A fresh path under the cargo target directory (never `/tmp`), not yet created.
    fn scratch(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let exe = std::env::current_exe().unwrap();
        // <target>/<profile>/deps/<test binary> -> <target>
        let target = exe.ancestors().nth(3).unwrap().to_path_buf();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let unique = format!(
            "{name}-{nanos}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let base = target.join("fs-storage-scratch");
        fs::create_dir_all(&base).unwrap();
        base.join(unique)
    }

    /// A fresh data directory, created as init creates one.
    fn data_dir(name: &str) -> PathBuf {
        let dir = scratch(name);
        create_data_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn create_data_dir_makes_the_directory_and_refuses_an_existing_or_orphaned_one() {
        let dir = scratch("create");
        assert_eq!(create_data_dir(&dir), Ok(()));
        assert!(
            dir.is_dir(),
            "rule: init creates the data directory; dir={dir:?}"
        );
        let again = create_data_dir(&dir).unwrap_err();
        assert_eq!(
            (again.op, again.error),
            ("create_data_dir", StorageError::Failed),
            "rule: init never reuses an existing directory (§7.7 step 0); failure={again:?}"
        );
        let orphan = dir.join("no-such-parent").join("data");
        let missing = create_data_dir(&orphan).unwrap_err();
        assert_eq!(
            missing.error,
            StorageError::Missing,
            "rule: a missing parent is Missing, never created on the way; failure={missing:?}"
        );
    }

    #[test]
    fn opening_a_missing_directory_is_missing() {
        let dir = scratch("absent");
        let failure = FsStorage::open(&dir).unwrap_err();
        assert_eq!(
            (failure.op, failure.error),
            ("open", StorageError::Missing),
            "rule: an ordinary start refuses a missing data directory (§7.7); \
             failure={failure:?}"
        );
    }

    /// Re-executes this test binary as a second process that tries to lock `dir`, and
    /// returns its exit code.
    fn probe_lock(dir: &Path) -> Option<i32> {
        // This very test binary, by a fixed path (Linux), as a second process.
        let out = Command::new("/proc/self/exe")
            .args([
                "--exact",
                "tests::a_second_process_is_refused_the_lock_until_the_first_exits",
                "--test-threads=1",
            ])
            .env(PROBE, dir)
            .output()
            .unwrap();
        out.status.code()
    }

    #[test]
    fn a_second_process_is_refused_the_lock_until_the_first_exits() {
        if let Some(dir) = std::env::var_os(PROBE) {
            // The second process: report what locking the directory did, and exit.
            let code = match FsStorage::open(Path::new(&dir)).map(|mut s| s.lock()) {
                Ok(Err(StorageError::Locked)) => LOCKED,
                Ok(Ok(())) => LOCKED_OK,
                Ok(Err(_)) | Err(_) => PROBE_FAILED,
            };
            std::process::exit(code);
        }
        let dir = data_dir("lock-process");
        let mut first = FsStorage::open(&dir).unwrap();
        assert_eq!(first.lock(), Ok(()));
        assert_eq!(
            probe_lock(&dir),
            Some(LOCKED),
            "rule: two processes can never use one data directory (DUR-8, W12): a second \
             process is refused while the first holds the lock"
        );
        drop(first);
        assert_eq!(
            probe_lock(&dir),
            Some(LOCKED_OK),
            "rule: the lock is released when its holder exits"
        );
    }

    #[test]
    fn locking_again_through_the_same_handle_keeps_the_lock() {
        let dir = data_dir("relock");
        let mut storage = FsStorage::open(&dir).unwrap();
        assert_eq!(storage.lock(), Ok(()));
        assert_eq!(
            storage.lock(),
            Ok(()),
            "rule: the holder's own second lock call is not another process"
        );
        let mut other = FsStorage::open(&dir).unwrap();
        assert_eq!(other.lock(), Err(StorageError::Locked));
        assert_eq!(
            other.last_failure().map(|f| (f.op, f.error)),
            Some(("lock", StorageError::Locked))
        );
    }

    #[test]
    fn failures_keep_their_cause_for_the_roots_log() {
        let dir = data_dir("causes");
        let mut storage = FsStorage::open(&dir).unwrap();
        assert_eq!(storage.last_failure(), None);
        assert_eq!(storage.append(b"x"), Err(StorageError::Missing));
        let missing = storage.last_failure().unwrap();
        assert_eq!(
            (missing.op, missing.path.clone()),
            ("append", dir.join(JOURNAL)),
            "failure={missing:?}"
        );
        assert_eq!(storage.create_journal(), Ok(()));
        assert_eq!(storage.create_journal(), Err(StorageError::Failed));
        let exists = storage.last_failure().unwrap();
        assert!(
            exists.op == "create_journal" && !exists.cause.is_empty(),
            "rule: a refused create keeps the operating system's reason; failure={exists:?}"
        );
        assert_eq!(
            storage.truncate(ByteCount::new(1)),
            Err(StorageError::Failed)
        );
        let past = storage.last_failure().unwrap();
        assert!(
            past.op == "truncate" && past.cause.contains("past the journal's end"),
            "failure={past:?}"
        );
    }

    #[test]
    fn the_journal_and_lock_live_in_the_data_directory_under_their_plan_names() {
        let dir = data_dir("names");
        let mut storage = FsStorage::open(&dir).unwrap();
        assert_eq!(storage.lock(), Ok(()));
        assert_eq!(storage.create_journal(), Ok(()));
        assert_eq!(storage.sync_dir(), Ok(()));
        assert!(
            dir.join("journal.log").is_file() && dir.join("LOCK").is_file(),
            "rule: one journal.log plus a LOCK file per data directory (§7.6)"
        );
        assert_eq!(storage.dir(), dir.as_path());
    }

    /// A data directory whose journal is `/dev/full`, so every write to it fails with
    /// ENOSPC from the real operating system, and the handle on it.
    fn full_disk(name: &str) -> (PathBuf, FsStorage) {
        let dir = data_dir(name);
        assert!(
            Path::new("/dev/full").exists(),
            "fixture: /dev/full gives these tests a real ENOSPC (Linux)"
        );
        std::os::unix::fs::symlink("/dev/full", dir.join(JOURNAL)).unwrap();
        let storage = FsStorage::open(&dir).unwrap();
        (dir, storage)
    }

    /// One mutating call on a handle, for [`assert_latched_on`].
    type Call = fn(&mut FsStorage) -> Result<(), StorageError>;

    /// Asserts that every mutating call on `storage` is refused by its latch, naming
    /// `first`, the failed call that latched it.
    fn assert_latched_on(storage: &mut FsStorage, first: &str) {
        assert_eq!(storage.latched().map(|f| f.op), Some(first));
        let calls: [(&str, Call); 5] = [
            ("sync", |s| s.sync()),
            ("sync_dir", |s| s.sync_dir()),
            ("append", |s| s.append(b"frame")),
            ("truncate", |s| s.truncate(ByteCount::new(0))),
            ("create_journal", |s| s.create_journal()),
        ];
        for (op, call) in calls {
            assert_eq!(
                call(storage),
                Err(StorageError::Failed),
                "rule: after a failed {first}, {op} never reports success (br-zswx, §7.11)"
            );
            let refused = storage.last_failure().unwrap();
            assert!(
                refused.op == op
                    && refused
                        .cause
                        .contains(&format!("an earlier {first} failed")),
                "rule: the refusal names the failure that latched the handle; failure={refused:?}"
            );
        }
        assert_eq!(
            storage.latched().map(|f| f.op),
            Some(first),
            "rule: the first failure is the one kept"
        );
    }

    #[test]
    fn a_failed_append_latches_every_later_write_and_sync() {
        let (dir, mut storage) = full_disk("latch-append");
        assert_eq!(storage.latched(), None);
        assert_eq!(storage.append(b"frame"), Err(StorageError::Failed));
        let failed = storage.last_failure().unwrap();
        assert_eq!(
            (failed.op, failed.path.clone()),
            ("append", dir.join(JOURNAL)),
            "rule: a real ENOSPC is Failed with its cause; failure={failed:?}"
        );
        assert_latched_on(&mut storage, "append");
        assert_eq!(
            FsStorage::open(&dir).unwrap().sync_dir(),
            Ok(()),
            "fixture sanity: the directory itself syncs, so the refusal above is the latch"
        );
    }

    #[test]
    fn a_failed_truncate_latches_the_handle() {
        // ftruncate on a character device is EINVAL.
        let (_dir, mut storage) = full_disk("latch-truncate");
        assert_eq!(
            storage.truncate(ByteCount::new(0)),
            Err(StorageError::Failed)
        );
        assert_latched_on(&mut storage, "truncate");
    }

    #[test]
    fn refusals_that_change_nothing_do_not_latch() {
        let dir = data_dir("no-latch");
        let mut storage = FsStorage::open(&dir).unwrap();
        assert_eq!(storage.append(b"x"), Err(StorageError::Missing));
        assert_eq!(storage.create_journal(), Ok(()));
        assert_eq!(storage.create_journal(), Err(StorageError::Failed));
        assert_eq!(
            storage.truncate(ByteCount::new(1)),
            Err(StorageError::Failed)
        );
        assert_eq!(
            storage.latched(),
            None,
            "rule: no journal, an existing journal and a lengthening truncate change \
             nothing, so they leave the handle usable"
        );
        assert_eq!(storage.sync_dir(), Ok(()));
        assert_eq!(storage.append(b"frame"), Ok(()));
        assert_eq!(storage.sync(), Ok(()));
        assert_eq!(storage.journal_len(), Ok(ByteCount::new(5)));
    }

    #[test]
    fn a_vanished_directory_fails_lock_and_sync_dir_and_is_never_missing() {
        let dir = data_dir("vanished");
        let mut storage = FsStorage::open(&dir).unwrap();
        fs::rename(&dir, dir.with_extension("moved")).unwrap();
        assert_eq!(
            storage.lock(),
            Err(StorageError::Failed),
            "rule: lock returns only Locked or Failed (DataDir::lock); failure={:?}",
            storage.last_failure()
        );
        assert_eq!(
            storage.sync_dir(),
            Err(StorageError::Failed),
            "rule: sync_dir returns only Failed (ByteStorage::sync_dir); failure={:?}",
            storage.last_failure()
        );
        assert_eq!(storage.latched().map(|f| f.op), Some("sync_dir"));
    }

    #[test]
    fn an_unexaminable_path_is_failed_and_only_a_missing_one_is_missing() {
        // A symlink to itself: stat is ELOOP, whoever runs the test.
        let looped = scratch("loop");
        std::os::unix::fs::symlink(&looped, &looped).unwrap();
        let open = |dir: &Path| {
            FsStorage::open(dir)
                .map(|_| ())
                .map_err(|f| (f.op, f.error))
        };
        let examine = |dir: &Path| ReadOnlyDir::open(dir).map(|_| ()).map_err(|f| f.error);
        assert_eq!(
            (open(&looped), examine(&looped)),
            (
                Err(("open", StorageError::Failed)),
                Err(StorageError::Failed)
            ),
            "rule: a directory that cannot be examined is Failed, not a missing one (§7.7)"
        );
        // A path through a regular file is ENOTDIR: nothing is there, so Missing.
        let file = scratch("file");
        fs::write(&file, b"").unwrap();
        let below = file.join("data");
        assert_eq!(
            (open(&below), examine(&below), open(&file)),
            (
                Err(("open", StorageError::Missing)),
                Err(StorageError::Missing),
                Err(("open", StorageError::Missing))
            ),
            "rule: a path that is not a directory, or runs through a file, is Missing"
        );
        assert_eq!(
            create_data_dir(&below).map_err(|f| f.error),
            Err(StorageError::Missing),
            "rule: a parent that is not a directory is a missing parent"
        );
    }

    #[test]
    fn an_unopenable_lock_file_is_failed() {
        let dir = data_dir("lock-loop");
        std::os::unix::fs::symlink(dir.join(LOCK), dir.join(LOCK)).unwrap();
        let mut storage = FsStorage::open(&dir).unwrap();
        assert_eq!(
            storage.lock(),
            Err(StorageError::Failed),
            "rule: a LOCK that cannot be opened (ELOOP) is Failed, never Locked or Missing"
        );
        let failure = storage.last_failure().unwrap();
        assert!(
            failure.op == "lock" && !failure.cause.is_empty(),
            "failure={failure:?}"
        );
    }
}

#[cfg(test)]
mod doctor_handle {
    // check: FS-DOCTOR
    use super::*;
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A fresh path under the cargo target directory (never `/tmp`), not yet created.
    fn scratch(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let exe = std::env::current_exe().unwrap();
        let target = exe.ancestors().nth(3).unwrap().to_path_buf();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let unique = format!(
            "{name}-{nanos}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let base = target.join("fs-storage-doctor-scratch");
        fs::create_dir_all(&base).unwrap();
        base.join(unique)
    }

    /// Every file in `dir` with its bytes: what "unchanged" means for the doctor.
    fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }

    /// A data directory a node has used: a LOCK file and a journal of `bytes`.
    fn used_dir(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = scratch(name);
        create_data_dir(&dir).unwrap();
        let mut node = FsStorage::open(&dir).unwrap();
        node.lock().unwrap();
        node.create_journal().unwrap();
        node.sync_dir().unwrap();
        node.append(bytes).unwrap();
        node.sync().unwrap();
        dir
    }

    #[test]
    fn examining_a_directory_creates_and_changes_nothing() {
        let dir = used_dir("unchanged", b"genesis and frames");
        let before = snapshot(&dir);
        {
            let mut doctor = ReadOnlyDir::open(&dir).unwrap();
            assert_eq!(doctor.probe_lock(), Ok(LockProbe::Acquired));
            assert_eq!(
                doctor.read_at(ByteCount::new(0), ReadLen::new(7).unwrap()),
                Ok(b"genesis".to_vec())
            );
            assert_eq!(doctor.journal_len(), Ok(ByteCount::new(18)));
        }
        assert_eq!(
            snapshot(&dir),
            before,
            "rule: the doctor's handle leaves every file byte for byte as it was (O-7)"
        );
        let blank = scratch("blank");
        create_data_dir(&blank).unwrap();
        let mut doctor = ReadOnlyDir::open(&blank).unwrap();
        assert_eq!(
            (doctor.journal_len(), doctor.probe_lock()),
            (Err(StorageError::Missing), Ok(LockProbe::NoLockFile)),
            "rule: a blank directory has no journal and no lock"
        );
        drop(doctor);
        assert!(
            snapshot(&blank).is_empty(),
            "rule: probing never creates LOCK, unlike DataDir::lock"
        );
    }

    #[test]
    fn a_live_directory_is_reported_live_and_an_examined_one_cannot_start() {
        let dir = used_dir("live", b"frames");
        let mut node = FsStorage::open(&dir).unwrap();
        node.lock().unwrap();
        let mut doctor = ReadOnlyDir::open(&dir).unwrap();
        assert_eq!(
            doctor.probe_lock(),
            Ok(LockProbe::Live),
            "rule: a locked directory is live; the doctor refuses it (O-7)"
        );
        drop(node);
        assert_eq!(
            doctor.probe_lock(),
            Ok(LockProbe::Acquired),
            "rule: once the node exits the lock is free"
        );
        let mut starting = FsStorage::open(&dir).unwrap();
        assert_eq!(
            starting.lock(),
            Err(StorageError::Locked),
            "rule: while the doctor reads, no node can start on the directory"
        );
        drop(doctor);
        assert_eq!(
            starting.lock(),
            Ok(()),
            "rule: the doctor's hold ends with its handle"
        );
    }

    #[test]
    fn reads_are_bounded_and_never_silently_short() {
        let dir = used_dir("reads", b"0123456789");
        let doctor = ReadOnlyDir::open(&dir).unwrap();
        assert_eq!(
            doctor.read_at(ByteCount::new(6), ReadLen::new(8).unwrap()),
            Err(StorageError::ShortRead {
                wanted: ReadLen::new(8).unwrap(),
                got: 4
            }),
            "rule: a read past the end is ShortRead, as through the node's own handle"
        );
        assert_eq!(
            ReadOnlyDir::open(&dir.join("missing"))
                .map(|_| ())
                .map_err(|failure| failure.error),
            Err(StorageError::Missing),
            "rule: a missing directory is Missing"
        );
    }

    #[test]
    fn a_journal_the_process_cannot_write_is_still_examined() {
        let dir = used_dir("read-only-file", b"frames");
        let journal = dir.join(JOURNAL);
        let mut permissions = fs::metadata(&journal).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&journal, permissions).unwrap();
        let writable = FsStorage::open(&dir)
            .map(|_| ())
            .map_err(|failure| failure.error);
        let examined = ReadOnlyDir::open(&dir).and_then(|doctor| {
            doctor.journal_len().map_err(|error| Failure {
                op: "journal_len",
                path: journal.clone(),
                cause: String::new(),
                error,
            })
        });
        fs::set_permissions(&journal, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            examined.map_err(|failure| failure.error),
            Ok(ByteCount::new(6)),
            "rule: the doctor opens the journal read-only, so it needs no write access"
        );
        if running_as_root() {
            return;
        }
        assert_eq!(
            writable,
            Err(StorageError::Failed),
            "fixture sanity: the node's own handle needs write access"
        );
    }

    /// Whether this process runs as root, which ignores file permissions.
    fn running_as_root() -> bool {
        fs::metadata("/proc/self")
            .is_ok_and(|metadata| std::os::unix::fs::MetadataExt::uid(&metadata) == 0)
    }
}
