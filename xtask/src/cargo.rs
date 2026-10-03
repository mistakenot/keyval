//! Invoking cargo from xtask.
//!
//! When `rch` is installed, cargo runs as `rch exec -- cargo …`, so builds are offloaded
//! to remote workers once any are configured. With no workers, rch fails open and builds
//! locally (AGENTS.md "Compiler Checks & Gates"). Set `KEYVAL_NO_RCH=1` to force a
//! plain local `cargo`. Reports record which runner was used.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::process;

/// Set to `1` to run cargo directly even when `rch` is installed.
pub const NO_RCH_ENV: &str = "KEYVAL_NO_RCH";

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
            cmd.env("CARGO_TARGET_DIR", dir);
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
    }
}
