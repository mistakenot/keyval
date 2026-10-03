//! Locating the workspace root and reading its pinned toolchain.

use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Environment variable through which `./dev` passes the workspace root.
pub const ROOT_ENV: &str = "KEYVAL_ROOT";

/// Why the workspace root or its toolchain pin could not be determined.
#[derive(Debug)]
pub enum WorkspaceError {
    /// No ancestor of the current directory looks like the keyval workspace.
    RootNotFound(PathBuf),
    /// `rust-toolchain.toml` could not be read.
    PinUnreadable(PathBuf, io::Error),
    /// `rust-toolchain.toml` is not valid TOML or lacks `toolchain.channel`.
    PinInvalid(PathBuf, String),
}

impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootNotFound(start) => write!(
                f,
                "no keyval workspace (Cargo.toml + xtask/Cargo.toml) at or above {}; \
                 run through ./dev or set {ROOT_ENV}",
                start.display()
            ),
            Self::PinUnreadable(path, err) => write!(f, "cannot read {}: {err}", path.display()),
            Self::PinInvalid(path, why) => write!(f, "invalid {}: {why}", path.display()),
        }
    }
}

/// The workspace root: `$KEYVAL_ROOT` when set (the `./dev` launcher sets it),
/// otherwise the nearest ancestor of the current directory holding both `Cargo.toml`
/// and `xtask/Cargo.toml`.
pub fn root() -> Result<PathBuf, WorkspaceError> {
    if let Some(root) = env::var_os(ROOT_ENV) {
        return Ok(PathBuf::from(root));
    }
    let start = env::current_dir().map_err(|_| WorkspaceError::RootNotFound(PathBuf::new()))?;
    start
        .ancestors()
        .find(|dir| dir.join("Cargo.toml").is_file() && dir.join("xtask/Cargo.toml").is_file())
        .map(Path::to_path_buf)
        .ok_or(WorkspaceError::RootNotFound(start))
}

#[derive(Deserialize)]
struct ToolchainFile {
    toolchain: ToolchainSection,
}

#[derive(Deserialize)]
struct ToolchainSection {
    channel: String,
}

/// The `toolchain.channel` pinned in `<root>/rust-toolchain.toml`, e.g. `1.99.0`.
pub fn pinned_toolchain(root: &Path) -> Result<String, WorkspaceError> {
    let path = root.join("rust-toolchain.toml");
    let text = fs::read_to_string(&path)
        .map_err(|err| WorkspaceError::PinUnreadable(path.clone(), err))?;
    parse_pin(&text).map_err(|why| WorkspaceError::PinInvalid(path, why))
}

fn parse_pin(text: &str) -> Result<String, String> {
    let file: ToolchainFile = toml::from_str(text).map_err(|err| err.message().to_owned())?;
    Ok(file.toolchain.channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pin_reads_the_channel() {
        let text = "[toolchain]\nchannel = \"1.99.0\"\ncomponents = [\"clippy\"]\n";
        assert_eq!(parse_pin(text).as_deref(), Ok("1.99.0"), "input={text:?}");
    }

    #[test]
    fn parse_pin_rejects_a_file_without_a_channel() {
        let text = "[toolchain]\ncomponents = [\"clippy\"]\n";
        let observed = parse_pin(text);
        assert!(observed.is_err(), "input={text:?} observed={observed:?}");
    }

    #[test]
    fn the_repository_pin_is_an_exact_stable_release() {
        let root = crate::test_support::workspace_root();
        let pin = pinned_toolchain(&root).unwrap();
        let parts: Vec<&str> = pin.split('.').collect();
        assert!(
            parts.len() == 3 && parts.iter().all(|p| p.parse::<u32>().is_ok()),
            "rule: rust-toolchain.toml pins an exact stable version (AGENTS.md); pin={pin:?}"
        );
    }
}
