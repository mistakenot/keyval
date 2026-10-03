//! Helpers shared by xtask's own tests. Test-only: never compiled into `./dev`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::cargo::Cargo;

/// xtask's manifest directory, as `cargo test` reports it to the test process at run
/// time. Never `env!("CARGO_MANIFEST_DIR")`: a compile-time path outlives its checkout,
/// so a test binary built by one checkout would examine that checkout's files when
/// another runs it (br-gate-shared-target-dir-dtqu).
pub fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .expect("CARGO_MANIFEST_DIR is unset; run xtask's tests through `cargo test`")
}

/// The workspace root of the checkout under test.
pub fn workspace_root() -> PathBuf {
    manifest_dir().join("..")
}

/// Local cargo building into `<target>/<name>`.
///
/// A test that runs cargo must not share the outer `cargo test` build directory: that
/// directory stays locked while the tests run, so a nested build would wait forever.
pub fn nested_cargo(name: &str) -> Cargo {
    let exe = std::env::current_exe().unwrap();
    // <target>/debug/deps/xtask-<hash> -> <target>
    let target = exe.ancestors().nth(3).unwrap().join(name);
    Cargo::local().with_target_dir(target)
}

/// A fresh, empty directory under `<target>/keyval-scratch/` for one test's files.
/// Each call gets its own directory, so parallel tests never share one.
pub fn scratch_dir(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let exe = std::env::current_exe().unwrap();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let unique = format!(
        "{name}-{nanos}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let dir = exe
        .ancestors()
        .nth(3)
        .unwrap()
        .join("keyval-scratch")
        .join(unique);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
