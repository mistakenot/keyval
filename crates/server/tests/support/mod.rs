//! Helpers both test targets share: scratch directories under cargo's per-target temp
//! dir, and configs written into them whose relative data directories resolve there.
#![cfg(test)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use journal::{FrameKind, Genesis, encode};
use types::{Config, InstanceId, NodeId};

const ONE_VOTER: &str = include_str!("../../../../config/fixtures/one-voter.toml");

/// The instance ID the tests stamp.
pub const INSTANCE: &str = "00112233445566778899aabbccddeeff";

pub fn instance() -> InstanceId {
    InstanceId::parse(INSTANCE).unwrap()
}

pub fn node(id: u64) -> NodeId {
    NodeId::new(id).unwrap()
}

/// A fresh scratch directory: the config's directory, holding no data directory yet.
pub fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "server-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    if dir.exists() {
        fs::remove_dir_all(&dir).unwrap();
    }
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A one-voter config (node 1) in `dir`, whose relative data directory `node-1` resolves
/// against `dir`, with `edit` applied to its text. Returns the file's path and the config.
pub fn one_voter(dir: &Path, edit: impl Fn(String) -> String) -> (PathBuf, Config) {
    let text = edit(ONE_VOTER.replace("nodes/one-voter/1", "node-1"));
    write_config(dir, "one-voter.toml", &text)
}

/// Writes `text` to `dir/name` and parses it as a config file in `dir`.
pub fn write_config(dir: &Path, name: &str, text: &str) -> (PathBuf, Config) {
    let path = dir.join(name);
    fs::write(&path, text).unwrap();
    let config = Config::parse(text, dir).unwrap();
    (path, config)
}

/// The data directory of `id` in `config`.
pub fn data_dir(config: &Config, id: u64) -> PathBuf {
    config.node(node(id)).unwrap().data_dir().to_path_buf()
}

/// The journal's bytes, or `None` without one.
pub fn journal_bytes(dir: &Path) -> Option<Vec<u8>> {
    fs::read(dir.join(fs_storage::JOURNAL)).ok()
}

/// The genesis frame `init` writes for `id` in `config`.
pub fn genesis_frame(config: &Config, id: u64) -> Vec<u8> {
    let genesis = Genesis::new(config, node(id), instance()).unwrap();
    encode(FrameKind::Genesis, 1, &genesis.encode()).unwrap()
}
