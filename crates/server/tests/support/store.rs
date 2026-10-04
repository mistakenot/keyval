//! The library tests' own helpers: the three-voter config, recovery, and a recording
//! byte-storage port. This target never spawns a process, so the data-directory locks it
//! takes can never leak into a child (br-o8co, br-b7xl): the binary's tests are a target
//! of their own.
#![cfg(test)]

use std::path::{Path, PathBuf};

use fs_storage::FsStorage;
use journal::{LogBounds, Recovered, recover};
use types::{ByteCount, ByteStorage, Config, DataDir, JournalRead, ReadLen, StorageError};

use crate::support::{node, write_config};

const THREE_VOTERS: &str = include_str!("../../../../config/fixtures/three-voters.toml");

/// The three-voter config in `dir`, whose node 1 has the data directory `node-1`, like
/// `one_voter`'s node 1: the same directory under another voter set.
pub fn three_voters(dir: &Path) -> (PathBuf, Config) {
    let text = THREE_VOTERS
        .replace("nodes/three-voters/c", "node-1")
        .replace("nodes/three-voters/", "node-");
    write_config(dir, "three-voters.toml", &text)
}

/// Recovers `dir`'s journal for `id` in `config`, as a start would.
pub fn recovered(config: &Config, id: u64, dir: &Path) -> Recovered {
    let storage = FsStorage::open(dir).unwrap();
    let len = storage.journal_len().unwrap();
    recover(&storage, len, config, node(id), LogBounds::of(config)).unwrap()
}

/// A byte-storage port over a real data directory that records each call that changes
/// something, in order, and can fail one of them.
pub struct Recording {
    pub inner: FsStorage,
    pub calls: Vec<&'static str>,
    pub fail: Option<&'static str>,
}

impl Recording {
    pub fn open(dir: &Path, fail: Option<&'static str>) -> Self {
        Self {
            inner: FsStorage::open(dir).unwrap(),
            calls: Vec::new(),
            fail,
        }
    }

    fn call(&mut self, op: &'static str) -> Result<(), StorageError> {
        self.calls.push(op);
        if self.fail == Some(op) {
            Err(StorageError::Failed)
        } else {
            Ok(())
        }
    }
}

impl JournalRead for Recording {
    fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
        self.inner.read_at(offset, len)
    }
}

impl DataDir for Recording {
    fn lock(&mut self) -> Result<(), StorageError> {
        self.call("lock")?;
        self.inner.lock()
    }
}

impl ByteStorage for Recording {
    fn journal_len(&self) -> Result<ByteCount, StorageError> {
        self.inner.journal_len()
    }

    fn create_journal(&mut self) -> Result<(), StorageError> {
        self.call("create_journal")?;
        self.inner.create_journal()
    }

    fn sync_dir(&mut self) -> Result<(), StorageError> {
        self.call("sync_dir")?;
        self.inner.sync_dir()
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), StorageError> {
        self.call("append")?;
        self.inner.append(bytes)
    }

    fn truncate(&mut self, len: ByteCount) -> Result<(), StorageError> {
        self.call("truncate")?;
        self.inner.truncate(len)
    }

    fn sync(&mut self) -> Result<(), StorageError> {
        self.call("sync")?;
        self.inner.sync()
    }
}
