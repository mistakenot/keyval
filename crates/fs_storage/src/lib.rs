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
//! ([`FsStorage::last_failure`]) for the composition root to log.
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
    /// A failure of `op` on `path` caused by `err`: `NotFound` is
    /// [`StorageError::Missing`], anything else [`StorageError::Failed`].
    fn io(op: &'static str, path: &Path, err: &io::Error) -> Self {
        let error = if err.kind() == ErrorKind::NotFound {
            StorageError::Missing
        } else {
            StorageError::Failed
        };
        Self {
            op,
            path: path.to_path_buf(),
            cause: err.to_string(),
            error,
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
    fs::create_dir(dir).map_err(|err| {
        let mut failure = Failure::io("create_data_dir", dir, &err);
        if err.kind() == ErrorKind::AlreadyExists {
            failure.error = StorageError::Failed;
        }
        failure
    })?;
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
    /// The cause of the last failure, for the root's log.
    last_failure: Mutex<Option<Failure>>,
}

impl FsStorage {
    /// Opens the data directory `dir` as a starting process does: the journal, if it
    /// exists, is opened; nothing is created and the lock is not taken.
    ///
    /// # Errors
    ///
    /// A [`Failure`] whose error is [`StorageError::Missing`] if `dir` is not a directory,
    /// or [`StorageError::Failed`] if the journal cannot be opened.
    pub fn open(dir: &Path) -> Result<Self, Failure> {
        if !dir.is_dir() {
            return Err(Failure {
                op: "open",
                path: dir.to_path_buf(),
                cause: "not a directory".to_owned(),
                error: StorageError::Missing,
            });
        }
        let path = dir.join(JOURNAL);
        let journal = match OpenOptions::new().read(true).append(true).open(&path) {
            Ok(file) => Some(file),
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => return Err(Failure::io("open_journal", &path, &err)),
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            journal,
            lock: None,
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
            Err(TryLockError::Error(err)) => Err(self.fail(Failure {
                error: StorageError::Failed,
                ..Failure::io("lock", &path, &err)
            })),
        }
    }
}

impl ByteStorage for FsStorage {
    fn journal_len(&self) -> Result<ByteCount, StorageError> {
        self.len_of("journal_len").map(ByteCount::new)
    }

    fn create_journal(&mut self) -> Result<(), StorageError> {
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
            Err(err) => Err(self.fail(Failure {
                // An existing journal is refused, never replaced.
                error: StorageError::Failed,
                ..Failure::io("create_journal", &path, &err)
            })),
        }
    }

    fn sync_dir(&mut self) -> Result<(), StorageError> {
        sync_directory(&self.dir).map_err(|err| self.fail(Failure::io("sync_dir", &self.dir, &err)))
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), StorageError> {
        let result = self.journal("append")?.write_all(bytes);
        result.map_err(|err| self.journal_error("append", &err))
    }

    fn truncate(&mut self, len: ByteCount) -> Result<(), StorageError> {
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
        result.map_err(|err| self.journal_error("truncate", &err))
    }

    fn sync(&mut self) -> Result<(), StorageError> {
        let result = self.journal("sync")?.sync_data();
        result.map_err(|err| self.journal_error("sync", &err))
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
}
