//! Invoking cargo from xtask.
//!
//! When `rch` is installed, cargo runs as `rch exec -- cargo …`, so builds are offloaded
//! to remote workers once any are configured. With no workers, rch fails open and builds
//! locally (AGENTS.md "Compiler Checks & Gates"). Set `KEYVAL_NO_RCH=1` to force a
//! plain local `cargo`. Reports record which runner was used.
//!
//! **One target directory per checkout** (br-gate-shared-target-dir-dtqu). Cargo names a
//! path package's build output relative to its workspace root and judges freshness by
//! mtime, so checkouts at different paths that share one `CARGO_TARGET_DIR` can run each
//! other's binaries, and a receipt would then describe a tree its tests never ran
//! against (P11). The `./dev` launcher therefore builds every checkout into
//! [`checkout_target_dir`] under a shared base, and code that runs cargo in a tree it
//! materialised (a commit candidate) does the same for that tree.

use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::process;

/// Set to `1` to run cargo directly even when `rch` is installed.
pub const NO_RCH_ENV: &str = "KEYVAL_NO_RCH";

/// The shared base of the per-checkout target directories, as the `./dev` launcher
/// exports it.
pub const TARGET_BASE_ENV: &str = "KEYVAL_TARGET_BASE";

/// Cargo's own target-directory variable.
pub const TARGET_DIR_ENV: &str = "CARGO_TARGET_DIR";

/// The shared base under which every checkout has its own target directory: the
/// launcher's [`TARGET_BASE_ENV`], else `CARGO_TARGET_DIR`, else `<root>/target`.
/// Empty values count as unset, as in the launcher's `${VAR:-default}`.
pub fn target_base(root: &Path) -> PathBuf {
    let set = |name: &str| env::var_os(name).filter(|value| !value.is_empty());
    set(TARGET_BASE_ENV)
        .or_else(|| set(TARGET_DIR_ENV))
        .map_or_else(|| root.join("target"), PathBuf::from)
}

/// The file in each per-checkout target directory that names its checkout's real path,
/// one line. The `./dev` launcher writes it on every run, so `./dev prune` can tell when
/// a checkout is gone and its target directory is an orphan (Q9).
pub const CHECKOUT_MARKER: &str = "keyval-checkout.path";

/// The target directory of the checkout at `checkout`: `keyval-checkout-<hash>` under
/// `base`, where `<hash>` is the first 16 hex digits of the SHA-256 of the checkout's
/// real path. Exactly what the `./dev` launcher exports, so every spelling of one
/// checkout shares one directory and different checkouts never do.
pub fn checkout_target_dir(base: &Path, checkout: &Path) -> Result<PathBuf, String> {
    let real = fs::canonicalize(checkout)
        .map_err(|err| format!("cannot resolve {}: {err}", checkout.display()))?;
    Ok(base.join(checkout_dir_name(&real)))
}

/// The target directory name of the checkout whose real path is `real`, which must
/// already be resolved: [`checkout_target_dir`] resolves it, and a marker records it.
pub fn checkout_dir_name(real: &Path) -> String {
    let digest = Sha256::digest(real.as_os_str().as_bytes());
    let hash = digest.iter().take(8).fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    });
    format!("keyval-checkout-{hash}")
}

/// How cargo is launched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Runner {
    /// `rch exec -- cargo …`
    Rch,
    /// `cargo …`
    Local,
}

/// A configured way to run cargo commands.
#[derive(Clone, Debug)]
pub struct Cargo {
    runner: Runner,
    target_dir: Option<PathBuf>,
}

impl Cargo {
    /// `rch` if it is on `PATH` and not disabled through [`NO_RCH_ENV`], else local.
    pub fn detect() -> Self {
        let disabled = env::var_os(NO_RCH_ENV).is_some_and(|v| v == "1");
        let runner = if !disabled && process::find_in_path("rch").is_some() {
            Runner::Rch
        } else {
            Runner::Local
        };
        Self {
            runner,
            target_dir: None,
        }
    }

    /// Plain local cargo.
    #[cfg(test)]
    pub const fn local() -> Self {
        Self {
            runner: Runner::Local,
            target_dir: None,
        }
    }

    /// Builds into `dir` instead of the default target directory.
    #[cfg(test)]
    pub fn with_target_dir(mut self, dir: PathBuf) -> Self {
        self.target_dir = Some(dir);
        self
    }

    /// The runner this invoker uses.
    pub const fn runner(&self) -> Runner {
        self.runner
    }

    /// The target directory its commands build into, when one is set: the override, else
    /// the inherited `CARGO_TARGET_DIR`. `None` means cargo's default for the workspace.
    pub fn target_dir(&self) -> Option<OsString> {
        self.target_dir.as_ref().map_or_else(
            || env::var_os(TARGET_DIR_ENV).filter(|value| !value.is_empty()),
            |dir| Some(dir.clone().into_os_string()),
        )
    }

    /// A `cargo <args>` command that runs in `cwd`.
    pub fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut cmd = match self.runner {
            Runner::Rch => {
                let mut cmd = Command::new("rch");
                cmd.args(["exec", "--", "cargo"]);
                cmd
            }
            Runner::Local => Command::new("cargo"),
        };
        cmd.current_dir(cwd).args(args);
        if let Some(dir) = &self.target_dir {
            cmd.env(TARGET_DIR_ENV, dir);
        }
        cmd
    }

    /// The command line `command` would run, for reports and re-run hints.
    pub fn describe(&self, args: &[&str]) -> String {
        let prefix = match self.runner {
            Runner::Rch => "rch exec -- cargo",
            Runner::Local => "cargo",
        };
        std::iter::once(prefix)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rch_runner_prefixes_the_cargo_command() {
        let cargo = Cargo {
            runner: Runner::Rch,
            target_dir: None,
        };
        let cmd = cargo.command(Path::new("/"), &["test", "-p", "xtask"]);
        let argv: Vec<_> = std::iter::once(cmd.get_program())
            .chain(cmd.get_args())
            .collect();
        assert_eq!(
            argv,
            ["rch", "exec", "--", "cargo", "test", "-p", "xtask"],
            "rule: rch offload wraps the unchanged cargo command line"
        );
        assert_eq!(
            cargo.describe(&["test", "-p", "xtask"]),
            "rch exec -- cargo test -p xtask"
        );
    }

    #[test]
    fn target_dir_override_is_passed_through_the_environment() {
        let cargo = Cargo::local().with_target_dir(PathBuf::from("/tmp/nested"));
        let cmd = cargo.command(Path::new("/"), &["build"]);
        let target = cmd
            .get_envs()
            .find(|(key, _)| *key == "CARGO_TARGET_DIR")
            .and_then(|(_, value)| value);
        assert_eq!(target, Some(std::ffi::OsStr::new("/tmp/nested")));
        assert_eq!(cmd.get_program(), "cargo");
        assert_eq!(
            cargo.target_dir(),
            Some(OsString::from("/tmp/nested")),
            "rule: receipts report the directory the commands build into"
        );
    }

    #[test]
    fn every_checkout_gets_its_own_target_dir() {
        let scratch = crate::test_support::scratch_dir("checkout-target");
        let (a, b) = (scratch.join("a"), scratch.join("b"));
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        std::os::unix::fs::symlink(&a, scratch.join("a-link")).unwrap();
        let base = Path::new("/shared/target");
        let dir = |checkout: &Path| checkout_target_dir(base, checkout).unwrap();
        assert_ne!(
            dir(&a),
            dir(&b),
            "rule: two checkouts never share a target dir (br-gate-shared-target-dir-dtqu)"
        );
        assert_eq!(
            dir(&scratch.join("a-link")),
            dir(&a),
            "rule: the name hashes the real path, so every spelling of a checkout agrees"
        );
        let name = dir(&a).file_name().unwrap().to_string_lossy().into_owned();
        let hash = name.strip_prefix("keyval-checkout-").unwrap_or_default();
        assert!(
            dir(&a).parent() == Some(base)
                && hash.len() == 16
                && hash.chars().all(|c| c.is_ascii_hexdigit()),
            "observed={}",
            dir(&a).display()
        );
        assert!(checkout_target_dir(base, &scratch.join("missing")).is_err());
    }

    /// Runs a copy of the real `./dev` launcher with a fake `cargo` that prints the
    /// environment it was given: root, target base, target dir.
    fn launcher_env(checkout: &Path, fake_bin: &Path, env: &[(&str, &str)]) -> Vec<String> {
        fs::create_dir_all(checkout).unwrap();
        let launcher = checkout.join("dev");
        fs::copy(crate::test_support::workspace_root().join("dev"), &launcher).unwrap();
        let mut cmd = Command::new("sh");
        cmd.arg(&launcher)
            .env("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()))
            .env_remove(TARGET_BASE_ENV)
            .env_remove(TARGET_DIR_ENV);
        for (key, value) in env {
            cmd.env(key, value);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "launcher failed: {out:?}");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_launcher_builds_each_checkout_into_its_own_target_dir() {
        let scratch = crate::test_support::scratch_dir("launcher-target");
        let fake_bin = scratch.join("bin");
        fs::create_dir_all(&fake_bin).unwrap();
        let fake_cargo = fake_bin.join("cargo");
        fs::write(
            &fake_cargo,
            "#!/bin/sh\nprintf '%s\\n' \"$KEYVAL_ROOT\" \"$KEYVAL_TARGET_BASE\" \"$CARGO_TARGET_DIR\"\n",
        )
        .unwrap();
        fs::set_permissions(
            &fake_cargo,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        let shared = scratch.join("shared-target");
        let shared_str = shared.to_string_lossy().into_owned();
        let (a, b) = (scratch.join("checkout-a"), scratch.join("checkout-b"));
        let expected = |checkout: &Path, base: &Path| {
            let real = fs::canonicalize(checkout).unwrap();
            let target = checkout_target_dir(base, checkout).unwrap();
            vec![
                real.display().to_string(),
                base.display().to_string(),
                target.display().to_string(),
            ]
        };

        let from_a = launcher_env(&a, &fake_bin, &[(TARGET_DIR_ENV, &shared_str)]);
        assert_eq!(
            from_a,
            expected(&a, &shared),
            "rule: the launcher and cargo::checkout_target_dir agree"
        );
        let real_a = fs::canonicalize(&a).unwrap();
        assert_eq!(
            fs::read_to_string(Path::new(&from_a[2]).join(CHECKOUT_MARKER)).unwrap(),
            format!("{}\n", real_a.display()),
            "rule: the launcher names the checkout in its target dir, so ./dev prune can \
             tell when it is gone (Q9)"
        );
        assert_eq!(
            Path::new(&from_a[2]).file_name().unwrap().to_string_lossy(),
            checkout_dir_name(&real_a),
            "rule: the marker's path hashes to the directory's own name"
        );
        let from_b = launcher_env(&b, &fake_bin, &[(TARGET_DIR_ENV, &shared_str)]);
        assert_eq!(from_b, expected(&b, &shared));
        assert_ne!(
            from_a[2], from_b[2],
            "rule: checkouts sharing CARGO_TARGET_DIR still build apart"
        );
        // A launcher started from inside another run (a gate's tests, the commit wrapper)
        // inherits both variables; it must derive from the base, not nest.
        let nested = launcher_env(
            &b,
            &fake_bin,
            &[(TARGET_BASE_ENV, &from_a[1]), (TARGET_DIR_ENV, &from_a[2])],
        );
        assert_eq!(
            nested,
            expected(&b, &shared),
            "rule: nesting never compounds"
        );
        let unset = launcher_env(&a, &fake_bin, &[]);
        assert_eq!(
            unset,
            expected(&a, &real_a.join("target")),
            "rule: without CARGO_TARGET_DIR the base is <root>/target"
        );
    }
}
