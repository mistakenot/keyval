//! `keyval-server run --config <file> --node <id>`: the start's preflight (PLAN.md §7.7
//! steps 1–4, D-52, DUR-8).
//!
//! An ordinary start never initialises anything. It refuses a missing data directory, a
//! directory without a journal, and a journal without a valid genesis, because a node that
//! lost its data rejoining as a blank voter is Raft amnesia: it could grant a second vote
//! in a term it already voted in, and help elect a leader missing committed entries. It
//! refuses a held lock, a genesis for another node or configuration, and a damaged journal
//! as well. Every refusal modifies nothing; in particular a missing directory is never
//! created.

use std::path::{Path, PathBuf};

use fs_storage::{Failure, FsStorage};
use journal::{GenesisMismatch, LogBounds, Recovered, RecoveryError, recover};
use types::{ByteStorage, Config, NodeId, StorageError};

/// A data directory that passed the preflight: locked by this process, with its journal
/// recovered (§7.7 steps 1–4). The node starts from it (W18.2).
pub struct Prepared {
    /// The data directory, resolved against the config file's directory.
    pub data_dir: PathBuf,
    /// The locked data directory.
    pub storage: FsStorage,
    /// The recovered journal: its verified genesis, its logical log and any torn tail.
    pub recovered: Recovered,
}

/// Why the data directory has no valid genesis (D-52).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Uninitialised {
    /// The directory holds no journal.
    NoJournal,
    /// The journal is empty.
    EmptyJournal,
    /// The journal holds part of a genesis frame and nothing after it: a crash interrupted
    /// `init`, which may re-run.
    IncompleteGenesis,
}

/// Why `run` refused to start. Nothing was modified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunRefusal {
    /// The node is not one of the configuration's voters.
    UnknownNode {
        /// The node asked for.
        node: NodeId,
    },
    /// The data directory does not exist (or is not a directory). It is not created:
    /// initialisation is explicit (`keyval-server init`, D-52).
    MissingDataDir {
        /// The data directory.
        data_dir: PathBuf,
    },
    /// The data directory holds no valid genesis: run `keyval-server init` first.
    Uninitialised {
        /// The data directory.
        data_dir: PathBuf,
        /// What it holds instead.
        why: Uninitialised,
    },
    /// Another process holds the data directory's lock (DUR-8).
    Locked {
        /// The data directory.
        data_dir: PathBuf,
    },
    /// The genesis binds the directory to another node, voter set or semantic
    /// configuration (DUR-8, §7.7 step 2).
    GenesisMismatch {
        /// The data directory.
        data_dir: PathBuf,
        /// What conflicts.
        mismatch: GenesisMismatch,
    },
    /// The journal cannot be recovered: content where a genesis belongs, or damage that
    /// is not a torn tail (DUR-6, §7.7 step 3).
    Damaged {
        /// The data directory.
        data_dir: PathBuf,
        /// What recovery found.
        error: RecoveryError,
    },
    /// A storage call failed.
    Storage {
        /// The data directory.
        data_dir: PathBuf,
        /// The port call, e.g. `open` or `journal_len`.
        op: &'static str,
        /// The port's error.
        error: StorageError,
        /// The operating system's cause, when the adapter kept one.
        cause: Option<String>,
    },
}

/// Why [`preflight`] refused, before the data directory's path is known to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The directory holds no valid genesis.
    Uninitialised(Uninitialised),
    /// Another process holds the lock.
    Locked,
    /// The genesis conflicts with the configuration.
    Mismatch(GenesisMismatch),
    /// The journal cannot be recovered.
    Damaged(RecoveryError),
    /// The port call `op` failed with `error`.
    Storage(&'static str, StorageError),
}

/// The start's preflight for `node` in `config`.
///
/// Opens the data directory, then [`preflight`]s it.
///
/// # Errors
///
/// A [`RunRefusal`] naming what is wrong. Nothing was modified.
pub fn prepare(config: &Config, node: NodeId) -> Result<Prepared, RunRefusal> {
    let Some(node_config) = config.node(node) else {
        return Err(RunRefusal::UnknownNode { node });
    };
    let data_dir = node_config.data_dir().to_path_buf();
    let mut storage = FsStorage::open(&data_dir).map_err(|failure| match failure.error {
        StorageError::Missing => RunRefusal::MissingDataDir {
            data_dir: data_dir.clone(),
        },
        StorageError::Locked | StorageError::ShortRead { .. } | StorageError::Failed => {
            storage_refusal(&data_dir, "open", failure.error, Some(failure))
        }
    })?;
    match preflight(&mut storage, config, node) {
        Ok(recovered) => Ok(Prepared {
            data_dir,
            storage,
            recovered,
        }),
        Err(refused) => Err(in_dir(refused, &data_dir, storage.last_failure())),
    }
}

/// §7.7 steps 1–4 through the byte-storage port.
///
/// Take the lock, then recover the journal: verify the genesis against `config` and
/// `node`, scan the frames, rebuild the log.
///
/// # Errors
///
/// [`Refused`], naming the step. Nothing was modified.
pub fn preflight<S: ByteStorage>(
    storage: &mut S,
    config: &Config,
    node: NodeId,
) -> Result<Recovered, Refused> {
    storage.lock().map_err(|error| match error {
        StorageError::Locked => Refused::Locked,
        StorageError::Missing | StorageError::ShortRead { .. } | StorageError::Failed => {
            Refused::Storage("lock", error)
        }
    })?;
    let len = storage.journal_len().map_err(|error| match error {
        StorageError::Missing => Refused::Uninitialised(Uninitialised::NoJournal),
        StorageError::Locked | StorageError::ShortRead { .. } | StorageError::Failed => {
            Refused::Storage("journal_len", error)
        }
    })?;
    recover(&*storage, len, config, node, LogBounds::of(config)).map_err(|error| match error {
        RecoveryError::Empty => Refused::Uninitialised(Uninitialised::EmptyJournal),
        RecoveryError::IncompleteGenesis => {
            Refused::Uninitialised(Uninitialised::IncompleteGenesis)
        }
        RecoveryError::Mismatch(mismatch) => Refused::Mismatch(mismatch),
        RecoveryError::NotAnInterruptedInit { .. }
        | RecoveryError::ImpossibleHeader { .. }
        | RecoveryError::NotGenesis { .. }
        | RecoveryError::GenesisRecord(_)
        | RecoveryError::Record { .. }
        | RecoveryError::Apply { .. }
        | RecoveryError::Corruption { .. }
        | RecoveryError::TailTooLong { .. } => Refused::Damaged(error),
        RecoveryError::Storage(error) => Refused::Storage("read_at", error),
    })
}

/// The refusal for the data directory `data_dir`, with the adapter's last `failure`.
fn in_dir(refused: Refused, data_dir: &Path, failure: Option<Failure>) -> RunRefusal {
    let data_dir = data_dir.to_path_buf();
    match refused {
        Refused::Uninitialised(why) => RunRefusal::Uninitialised { data_dir, why },
        Refused::Locked => RunRefusal::Locked { data_dir },
        Refused::Mismatch(mismatch) => RunRefusal::GenesisMismatch { data_dir, mismatch },
        Refused::Damaged(error) => RunRefusal::Damaged { data_dir, error },
        Refused::Storage(op, error) => storage_refusal(&data_dir, op, error, failure),
    }
}

fn storage_refusal(
    data_dir: &Path,
    op: &'static str,
    error: StorageError,
    failure: Option<Failure>,
) -> RunRefusal {
    RunRefusal::Storage {
        data_dir: data_dir.to_path_buf(),
        op,
        error,
        cause: failure.map(|failure| failure.cause),
    }
}
