//! Helpers shared by xtask's own tests. Test-only: never compiled into `./dev`.

use std::path::{Path, PathBuf};

use crate::cargo::Cargo;

/// The workspace root of this checkout.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
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
