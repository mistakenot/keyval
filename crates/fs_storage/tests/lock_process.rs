//! The two-process `LOCK` test (DUR-8; W12.3), alone in its own test binary (br-b7xl).
//!
//! The test runs this binary again as a second process. A process spawned while any thread
//! of the spawner has a `LOCK` open shares that open file, and so its lock, until it execs:
//! close-on-exec, which std sets on every file it opens, releases it only at the exec. In
//! the library's test binary a sibling test could drop its lock in that window and then
//! find it still held. In this binary no other test runs, so the only lock open when the
//! test spawns is its own. Keep this the only test in this binary.
#![cfg(test)]

mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use fs_storage::{DataDirCreated, FsStorage, create_data_dir};
    use types::{DataDir as _, StorageError};

    /// The environment variable that turns a re-executed test binary into the second
    /// process of [`a_second_process_is_refused_the_lock_until_the_first_exits`].
    const PROBE: &str = "FS_STORAGE_LOCK_PROBE";

    /// What the probe process reports through its exit code.
    const LOCKED: i32 = 3;
    const LOCKED_OK: i32 = 4;
    const PROBE_FAILED: i32 = 5;

    /// A fresh data directory under the cargo target directory (never `/tmp`), created as
    /// init creates one.
    fn data_dir() -> PathBuf {
        let exe = std::env::current_exe().unwrap();
        // <target>/<profile>/deps/<test binary> -> <target>
        let target = exe.ancestors().nth(3).unwrap().to_path_buf();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = target.join("fs-storage-scratch");
        std::fs::create_dir_all(&base).unwrap();
        let dir = base.join(format!("lock-process-{nanos}-{}", std::process::id()));
        assert_eq!(create_data_dir(&dir), Ok(DataDirCreated::New));
        dir
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
        let dir = data_dir();
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
}
