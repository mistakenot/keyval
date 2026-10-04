//! `keyval-server init --config <file> --node <id> --instance <hex>` (PLAN.md §7.7 step 0,
//! DUR-7, DUR-8, D-52, D-63).
//!
//! The sequence is §7.7 step 0's, each step durable before the next depends on it:
//! 1. create the data directory and sync its parent ([`fs_storage::create_data_dir`]);
//! 2. take the exclusive lock;
//! 3. create `journal.log` (unless an interrupted `init` left one) and sync the directory;
//! 4. append the genesis frame and sync it.
//!
//! `init` may re-run only over what a crash during `init` can leave: an empty journal, or
//! one holding no complete genesis frame and nothing after it (recovery's
//! [`RecoveryError::permits_init`]). It then truncates the journal to nothing and writes
//! the genesis again. Any other content, an initialised journal included, is never
//! re-initialised and is left exactly as it was (DUR-6; re-initialisation is owner-only).

use std::path::{Path, PathBuf};

use fs_storage::{DataDirCreated, Failure, FsStorage, create_data_dir};
use journal::{FrameKind, Genesis, LogBounds, RecoveryError, encode, recover};
use types::{ByteCount, ByteStorage, Config, InstanceId, NodeId, StorageError, invariant};

/// What `init` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Initialised {
    /// The data directory, resolved against the config file's directory.
    pub data_dir: PathBuf,
    /// Whether the directory was created now, or found as an interrupted `init` left it.
    pub directory: DataDirCreated,
    /// What an interrupted `init` had left in the journal, which this one replaced.
    pub resumed: Option<Resumed>,
    /// The genesis written: the directory's binding (DUR-8).
    pub genesis: Genesis,
    /// The journal's length: exactly the genesis frame.
    pub journal_bytes: ByteCount,
}

/// What an interrupted `init` had left in the journal (§7.7 step 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resumed {
    /// An empty journal: the crash came before the genesis was appended.
    EmptyJournal,
    /// Part of a genesis frame and nothing after it: the crash came during its append.
    IncompleteGenesis {
        /// The bytes it held, discarded.
        bytes: ByteCount,
    },
}

/// Why `init` refused. Nothing it refuses is left changed, except a data directory it
/// created before a later step failed, which a re-run then completes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitRefusal {
    /// The node is not one of the configuration's voters: nothing was created.
    UnknownNode {
        /// The node asked for.
        node: NodeId,
    },
    /// The data directory could not be created or opened: its parent is missing, the path
    /// holds something other than a directory, or the operating system refused.
    DataDir(Failure),
    /// Another process holds the data directory's lock (DUR-8).
    Locked {
        /// The data directory.
        data_dir: PathBuf,
    },
    /// The journal holds a valid genesis: the directory is initialised already. Only the
    /// owner re-initialises a directory that ever held data (D-63).
    AlreadyInitialised {
        /// The data directory.
        data_dir: PathBuf,
        /// The instance ID its genesis carries.
        instance_id: InstanceId,
    },
    /// The journal holds something other than an interrupted `init`: a genesis for
    /// another node or configuration, or content after the genesis frame's length. It is
    /// never re-initialised (§7.7 step 0).
    NotInitialisable {
        /// The data directory.
        data_dir: PathBuf,
        /// What recovery found.
        error: RecoveryError,
    },
    /// A storage call failed; the step that needed it did not happen.
    Storage {
        /// The data directory.
        data_dir: PathBuf,
        /// The port call, e.g. `sync_dir`.
        op: &'static str,
        /// The port's error.
        error: StorageError,
        /// The operating system's cause, when the adapter kept one.
        cause: Option<String>,
    },
}

/// Initialises the data directory of `node` in `config`, stamped with `instance_id`
/// (§7.7 step 0).
///
/// # Errors
///
/// An [`InitRefusal`] naming the step that refused.
pub fn init(
    config: &Config,
    node: NodeId,
    instance_id: InstanceId,
) -> Result<Initialised, InitRefusal> {
    let Some(node_config) = config.node(node) else {
        return Err(InitRefusal::UnknownNode { node });
    };
    if Genesis::new(config, node, instance_id).is_none() {
        return Err(InitRefusal::UnknownNode { node });
    }
    let data_dir = node_config.data_dir().to_path_buf();
    let directory = create_data_dir(&data_dir).map_err(InitRefusal::DataDir)?;
    let mut storage = FsStorage::open(&data_dir).map_err(InitRefusal::DataDir)?;
    match initialise(&mut storage, config, node, instance_id) {
        Ok(written) => Ok(Initialised {
            data_dir,
            directory,
            resumed: written.resumed,
            genesis: written.genesis,
            journal_bytes: written.journal_bytes,
        }),
        Err(refusal) => Err(refusal.in_dir(&data_dir, node, storage.last_failure())),
    }
}

/// What [`initialise`] wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Written {
    /// What an interrupted `init` had left, which this one replaced.
    pub resumed: Option<Resumed>,
    /// The genesis written.
    pub genesis: Genesis,
    /// The journal's length afterwards: exactly the genesis frame.
    pub journal_bytes: ByteCount,
}

/// Why [`initialise`] refused, before the data directory's path is known to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The node is not a voter of the configuration.
    UnknownNode,
    /// Another process holds the lock.
    Locked,
    /// The journal holds a valid genesis carrying this instance ID.
    AlreadyInitialised(InstanceId),
    /// The journal holds something other than an interrupted `init`.
    NotInitialisable(RecoveryError),
    /// The port call `op` failed with `error`.
    Storage(&'static str, StorageError),
}

impl Refused {
    /// The refusal for `node`'s data directory `data_dir`, with the adapter's last
    /// `failure`.
    fn in_dir(self, data_dir: &Path, node: NodeId, failure: Option<Failure>) -> InitRefusal {
        let data_dir = data_dir.to_path_buf();
        match self {
            Self::UnknownNode => InitRefusal::UnknownNode { node },
            Self::Locked => InitRefusal::Locked { data_dir },
            Self::AlreadyInitialised(instance_id) => InitRefusal::AlreadyInitialised {
                data_dir,
                instance_id,
            },
            Self::NotInitialisable(error) => InitRefusal::NotInitialisable { data_dir, error },
            Self::Storage(op, error) => InitRefusal::Storage {
                data_dir,
                op,
                error,
                cause: failure.map(|failure| failure.cause),
            },
        }
    }
}

/// §7.7 step 0 from the lock on, through the byte-storage port.
///
/// Lock; create the journal unless one is there, and sync the directory; replace what an
/// interrupted `init` left; append and sync the genesis frame.
///
/// # Errors
///
/// [`Refused`], naming the step; a storage error stops at the call that failed.
///
/// # Panics
///
/// `SERVER-GENESIS-FRAME` if the genesis does not encode as one frame, or the journal is
/// not exactly that frame afterwards.
pub fn initialise<S: ByteStorage>(
    storage: &mut S,
    config: &Config,
    node: NodeId,
    instance_id: InstanceId,
) -> Result<Written, Refused> {
    let Some(genesis) = Genesis::new(config, node, instance_id) else {
        return Err(Refused::UnknownNode);
    };
    let frame = encode(FrameKind::Genesis, 1, &genesis.encode());
    invariant!(
        frame.is_ok(),
        "SERVER-GENESIS-FRAME",
        "node {node}: the genesis does not encode as a frame: {frame:?}"
    );
    let frame = frame.unwrap_or_default();
    storage.lock().map_err(|error| match error {
        StorageError::Locked => Refused::Locked,
        StorageError::Missing | StorageError::ShortRead { .. } | StorageError::Failed => {
            Refused::Storage("lock", error)
        }
    })?;
    let resumed = match storage.journal_len() {
        Err(StorageError::Missing) => {
            storage
                .create_journal()
                .map_err(|error| Refused::Storage("create_journal", error))?;
            None
        }
        Err(error) => return Err(Refused::Storage("journal_len", error)),
        Ok(len) => match recover(&*storage, len, config, node, LogBounds::of(config)) {
            Ok(recovered) => {
                return Err(Refused::AlreadyInitialised(
                    recovered.genesis().instance_id(),
                ));
            }
            Err(RecoveryError::Empty) => Some(Resumed::EmptyJournal),
            Err(RecoveryError::Storage(error)) => return Err(Refused::Storage("read_at", error)),
            Err(error) if error.permits_init() => Some(Resumed::IncompleteGenesis { bytes: len }),
            Err(error) => return Err(Refused::NotInitialisable(error)),
        },
    };
    // Always: a re-run cannot know whether the first run's directory sync happened.
    storage
        .sync_dir()
        .map_err(|error| Refused::Storage("sync_dir", error))?;
    if let Some(Resumed::IncompleteGenesis { .. }) = resumed {
        storage
            .truncate(ByteCount::new(0))
            .map_err(|error| Refused::Storage("truncate", error))?;
    }
    storage
        .append(&frame)
        .map_err(|error| Refused::Storage("append", error))?;
    storage
        .sync()
        .map_err(|error| Refused::Storage("sync", error))?;
    let journal_bytes = storage
        .journal_len()
        .map_err(|error| Refused::Storage("journal_len", error))?;
    let frame_len = u64::try_from(frame.len()).unwrap_or(u64::MAX);
    invariant!(
        journal_bytes.get() == frame_len,
        "SERVER-GENESIS-FRAME",
        "node {node}: after init the journal holds {} bytes, not the {frame_len}-byte genesis \
         frame",
        journal_bytes.get()
    );
    Ok(Written {
        resumed,
        genesis,
        journal_bytes,
    })
}
