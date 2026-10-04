//! `keyval-server`'s robot-mode output (O-6): the `keyval.server.init/v1` report, the
//! `keyval.server.error/v1` envelope, human-readable messages, and the exit codes.
//!
//! Exit codes: `0` initialised; `1` refused because of the data directory (or, for `run`,
//! not serving yet); `2` a usage or configuration error.

use std::path::Path;

use serde::Serialize;

use crate::init::{InitRefusal, Initialised, Resumed};
use crate::run::{Prepared, RunRefusal, Uninitialised};

/// The init report's schema identifier; its JSON Schema is
/// `schemas/server/init.v1.schema.json`.
pub const INIT_SCHEMA: &str = "keyval.server.init/v1";

/// The error envelope's schema identifier; its JSON Schema is
/// `schemas/server/error.v1.schema.json`.
pub const ERROR_SCHEMA: &str = "keyval.server.error/v1";

/// The exit code of a refusal because of the data directory.
pub const REFUSED_EXIT: u8 = 1;

/// The exit code of a usage or configuration error.
pub const USAGE_EXIT: u8 = 2;

/// The command a report or error is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    /// `keyval-server init`.
    Init,
    /// `keyval-server run`.
    Run,
}

impl Command {
    /// The command's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Run => "run",
        }
    }
}

/// Whether the data directory was created now or found.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Directory {
    /// Created by this `init`.
    Created,
    /// Found, as an interrupted `init` left it.
    Existing,
}

/// What an interrupted `init` had left in the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Replaced {
    /// An empty journal.
    EmptyJournal,
    /// Part of a genesis frame.
    IncompleteGenesis,
}

/// `keyval-server init`'s report: the binding it wrote (DUR-8).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InitReport {
    /// [`INIT_SCHEMA`].
    pub schema: &'static str,
    /// The data directory.
    pub data_dir: String,
    /// Whether the directory was created now.
    pub directory: Directory,
    /// What an interrupted `init` had left, which this one replaced, if anything.
    pub replaced: Option<Replaced>,
    /// The node.
    pub node: u64,
    /// The display cluster ID.
    pub cluster_id: String,
    /// The cluster instance ID, as 32 hex digits.
    pub instance_id: String,
    /// The voters, in configuration order.
    pub voters: Vec<u64>,
    /// The journal's length: the genesis frame.
    pub journal_bytes: u64,
}

impl InitReport {
    /// The report of `initialised`.
    #[must_use]
    pub fn new(initialised: &Initialised) -> Self {
        let genesis = &initialised.genesis;
        Self {
            schema: INIT_SCHEMA,
            data_dir: initialised.data_dir.display().to_string(),
            directory: match initialised.directory {
                fs_storage::DataDirCreated::New => Directory::Created,
                fs_storage::DataDirCreated::Existing => Directory::Existing,
            },
            replaced: initialised.resumed.map(|resumed| match resumed {
                Resumed::EmptyJournal => Replaced::EmptyJournal,
                Resumed::IncompleteGenesis { .. } => Replaced::IncompleteGenesis,
            }),
            node: genesis.node_id().get(),
            cluster_id: genesis.cluster_id().to_string(),
            instance_id: genesis.instance_id().to_string(),
            voters: genesis.voters().iter().map(|voter| voter.get()).collect(),
            journal_bytes: initialised.journal_bytes.get(),
        }
    }

    /// The report as a line for people.
    #[must_use]
    pub fn human(&self) -> String {
        let how = match (self.directory, self.replaced) {
            (Directory::Created, _) => "created".to_owned(),
            (Directory::Existing, None) => "initialised the existing directory".to_owned(),
            (Directory::Existing, Some(Replaced::EmptyJournal)) => {
                "completed an interrupted init (empty journal)".to_owned()
            }
            (Directory::Existing, Some(Replaced::IncompleteGenesis)) => {
                "completed an interrupted init (incomplete genesis replaced)".to_owned()
            }
        };
        format!(
            "initialised {}: node {} of cluster {} (instance {}), {how}; journal {} bytes\n",
            self.data_dir, self.node, self.cluster_id, self.instance_id, self.journal_bytes
        )
    }
}

/// The kind of an error: what to fix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    /// The command line is invalid.
    Usage,
    /// The config file cannot be read or is invalid.
    Config,
    /// The node is not a voter of the configuration.
    UnknownNode,
    /// The data directory cannot be created or opened.
    DataDir,
    /// `run`: the data directory does not exist; it is never created.
    MissingDataDir,
    /// `run`: the data directory holds no valid genesis.
    Uninitialised,
    /// `init`: the data directory is initialised already.
    AlreadyInitialised,
    /// `init`: the journal holds something other than an interrupted `init`.
    NotInitialisable,
    /// Another process holds the data directory's lock.
    Locked,
    /// `run`: the genesis binds the directory to another node or configuration.
    GenesisMismatch,
    /// `run`: the journal is damaged.
    Damaged,
    /// A storage call failed.
    Storage,
    /// `run`: the directory passed the preflight, but this build cannot serve yet (W18.2).
    NotServing,
}

impl Code {
    /// The exit code: [`USAGE_EXIT`] for the command line and configuration,
    /// [`REFUSED_EXIT`] for the data directory.
    #[must_use]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Usage | Self::Config | Self::UnknownNode => USAGE_EXIT,
            Self::DataDir
            | Self::MissingDataDir
            | Self::Uninitialised
            | Self::AlreadyInitialised
            | Self::NotInitialisable
            | Self::Locked
            | Self::GenesisMismatch
            | Self::Damaged
            | Self::Storage
            | Self::NotServing => REFUSED_EXIT,
        }
    }
}

/// The `keyval.server.error/v1` envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ErrorReport {
    /// [`ERROR_SCHEMA`].
    pub schema: &'static str,
    /// The command.
    pub command: Command,
    /// What to fix.
    pub code: Code,
    /// What happened, for people.
    pub message: String,
    /// The data directory, once known.
    pub data_dir: Option<String>,
}

impl ErrorReport {
    /// An error of `command`.
    #[must_use]
    pub fn new(command: Command, code: Code, message: String, data_dir: Option<&Path>) -> Self {
        Self {
            schema: ERROR_SCHEMA,
            command,
            code,
            message,
            data_dir: data_dir.map(|dir| dir.display().to_string()),
        }
    }

    /// `init`'s refusal.
    #[must_use]
    pub fn init(refusal: &InitRefusal) -> Self {
        let (code, message, data_dir) = match refusal {
            InitRefusal::UnknownNode { node } => (
                Code::UnknownNode,
                format!("node {node} is not a voter of the configuration"),
                None,
            ),
            InitRefusal::DataDir(failure) => (
                Code::DataDir,
                format!(
                    "cannot {} {}: {}",
                    failure.op,
                    failure.path.display(),
                    failure.cause
                ),
                Some(failure.path.as_path()),
            ),
            InitRefusal::Locked { data_dir } => (
                Code::Locked,
                "another process holds the data directory's lock".to_owned(),
                Some(data_dir.as_path()),
            ),
            InitRefusal::AlreadyInitialised {
                data_dir,
                instance_id,
            } => (
                Code::AlreadyInitialised,
                format!(
                    "the data directory is initialised already (instance {instance_id}); \
                     re-initialising a directory that holds data is the owner's decision, \
                     never init's"
                ),
                Some(data_dir.as_path()),
            ),
            InitRefusal::NotInitialisable { data_dir, error } => (
                Code::NotInitialisable,
                format!(
                    "the journal holds more than an interrupted init leaves ({error}); it is \
                     never re-initialised"
                ),
                Some(data_dir.as_path()),
            ),
            InitRefusal::Storage {
                data_dir,
                op,
                error,
                cause,
            } => (
                Code::Storage,
                storage_message(op, error, cause.as_deref()),
                Some(data_dir.as_path()),
            ),
        };
        Self::new(Command::Init, code, message, data_dir)
    }

    /// `run`'s refusal.
    #[must_use]
    pub fn run(refusal: &RunRefusal) -> Self {
        let (code, message, data_dir) = match refusal {
            RunRefusal::UnknownNode { node } => (
                Code::UnknownNode,
                format!("node {node} is not a voter of the configuration"),
                None,
            ),
            RunRefusal::MissingDataDir { data_dir } => (
                Code::MissingDataDir,
                "the data directory does not exist; a start never creates it (a node that \
                 lost its data must not rejoin as a blank voter): run `keyval-server init` \
                 to initialise a new node"
                    .to_owned(),
                Some(data_dir.as_path()),
            ),
            RunRefusal::Uninitialised { data_dir, why } => (
                Code::Uninitialised,
                format!(
                    "the data directory is not initialised ({}): run `keyval-server init` \
                     first",
                    match why {
                        Uninitialised::NoJournal => "it holds no journal",
                        Uninitialised::EmptyJournal => "its journal is empty",
                        Uninitialised::IncompleteGenesis => {
                            "its journal holds an incomplete genesis: init was interrupted"
                        }
                    }
                ),
                Some(data_dir.as_path()),
            ),
            RunRefusal::Locked { data_dir } => (
                Code::Locked,
                "another process holds the data directory's lock".to_owned(),
                Some(data_dir.as_path()),
            ),
            RunRefusal::GenesisMismatch { data_dir, mismatch } => (
                Code::GenesisMismatch,
                format!(
                    "the data directory belongs to another node or configuration: \
                     {mismatch}; nothing was modified"
                ),
                Some(data_dir.as_path()),
            ),
            RunRefusal::Damaged { data_dir, error } => (
                Code::Damaged,
                format!(
                    "the journal cannot be recovered: {error}; nothing was modified \
                     (`keyval doctor` reports the details)"
                ),
                Some(data_dir.as_path()),
            ),
            RunRefusal::Storage {
                data_dir,
                op,
                error,
                cause,
            } => (
                Code::Storage,
                storage_message(op, error, cause.as_deref()),
                Some(data_dir.as_path()),
            ),
        };
        Self::new(Command::Run, code, message, data_dir)
    }

    /// `run`'s answer once the preflight passed: this build cannot serve yet (W18.2).
    #[must_use]
    pub fn not_serving(prepared: &Prepared) -> Self {
        let genesis = prepared.recovered.genesis();
        let tail = prepared
            .recovered
            .truncate_to()
            .map_or_else(String::new, |len| {
                format!(", with a torn tail to truncate at byte {}", len.get())
            });
        Self::new(
            Command::Run,
            Code::NotServing,
            format!(
                "node {} of cluster {} (instance {}) recovers from its data directory{tail}, \
                 but this build does not serve yet: the production wiring is W18.2",
                genesis.node_id(),
                genesis.cluster_id(),
                genesis.instance_id()
            ),
            Some(&prepared.data_dir),
        )
    }

    /// The error as a line for people.
    #[must_use]
    pub fn human(&self) -> String {
        let dir = self
            .data_dir
            .as_ref()
            .map_or_else(String::new, |dir| format!("{dir}: "));
        format!(
            "keyval-server {}: {dir}{}\n",
            self.command.name(),
            self.message
        )
    }
}

fn storage_message(op: &str, error: &types::StorageError, cause: Option<&str>) -> String {
    cause.map_or_else(
        || format!("storage call {op} failed: {error:?}"),
        |cause| format!("storage call {op} failed: {cause}"),
    )
}
