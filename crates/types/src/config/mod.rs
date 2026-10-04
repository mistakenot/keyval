//! The validated cluster configuration (PLAN.md §7.10, O-5, CD-3): one static file, parsed
//! once into a [`Config`] that cannot hold an inconsistent value.
//!
//! [`Config::parse`] is the only constructor. It reads the file with a strict in-crate
//! reader for exactly the config grammar (D-79), then refuses, with a specific
//! [`ConfigError`] and in this order:
//! 1. a file over [`MAX_CONFIG_BYTES`], or anything outside the grammar;
//! 2. a key defined twice in a table, or a key the config does not have (O-5: a misspelt
//!    key never falls back to a default);
//! 3. a missing cluster ID, protocol version or other required key, a value of the wrong
//!    kind, and any parameter outside its range in the [`ConfigParam`] table (§7.11), including
//!    a heartbeat interval below 1 tick;
//! 4. a voter count other than 1, 3 or 5 (D-45); a node ID that is zero or not below
//!    2^53 (D-77); an address that is not a fixed IP address and port; an empty data
//!    directory; a duplicate node ID, address or data directory;
//! 5. the timing rules: election minimum below 5 heartbeats, election maximum not above
//!    the minimum, a range other than `[t, 2t)` (D-75), a write deadline below the
//!    election maximum, a CLI deadline below the write deadline;
//! 6. the cross-limit rules: the largest command over the canonical-command limit, a
//!    journal hard stop below the margin formula (D-68), in-flight replication bytes over
//!    the per-peer send queue;
//! 7. any constraint of the consensus library's own configuration, for any node
//!    ([`ConsensusSettings::check`], D-78).
//!
//! Every key in §7.10's example is required. The §7.11 local library limits are optional
//! and default to their §7.11 values (D-79). Relative data directories resolve against the
//! config file's directory (§7.7 step 0); the caller passes that directory, since reading
//! the file is an adapter's job.

mod consensus;
mod error;
mod exposure;
mod fingerprint;
mod params;
mod reader;
mod rules;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};

pub use consensus::{ConsensusSettings, ConsensusViolation, ReadMode};
pub use error::{AddressProblem, ConfigError, ConfigSyntax, ConfigTable, ConfigValueKind};
pub use exposure::{AddressRole, ExposureWarning};
pub use fingerprint::{SEMANTIC_ENCODING, SemanticFingerprint};
pub use params::{
    COMMAND_ENVELOPE_BOUND, COMMAND_FORMAT_VERSION, ConfigParam, ConfigParamClass,
    ENTRY_ENVELOPE_BOUND, FRAME_FIELDS, FRAME_OVERHEAD_BYTES, JOURNAL_PROTOCOL_ALLOWANCE,
    MAX_COMMAND_BYTES, MAX_CONFIG_BYTES, MAX_FRAME_PAYLOAD_BYTES, MAX_REQUEST_BODY_BYTES,
    MAX_SCAN_PAGE_BYTES, PER_ENTRY_OVERHEAD_BOUND, PROTOCOL_VERSION,
};

use crate::{ByteCount, ClusterId, ItemCount, KeyValueLimits, LogIndex, NodeId, Tick};
use reader::{Document, Raw, Table};
use rules::Values;

/// The top-level key holding the cluster's display name.
const CLUSTER_ID_KEY: &str = "cluster_id";

/// The keys of a `[[nodes]]` table, all required.
const NODE_KEYS: [&str; 4] = ["id", "client_address", "peer_address", "data_dir"];

/// One voter of the cluster.
///
/// Invariants: the ID is nonzero and below 2^53; both addresses are IP addresses with a
/// nonzero port; the data directory is resolved against the config file's directory and
/// lexically normalised. Within a [`Config`], no ID, address or data directory repeats.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeConfig {
    /// The node ID.
    id: NodeId,
    /// The client API address.
    client_address: SocketAddr,
    /// The peer (Raft) address.
    peer_address: SocketAddr,
    /// The data directory.
    data_dir: PathBuf,
}

impl NodeConfig {
    /// The node ID.
    #[must_use]
    pub const fn id(&self) -> NodeId {
        self.id
    }

    /// The client API address.
    #[must_use]
    pub const fn client_address(&self) -> SocketAddr {
        self.client_address
    }

    /// The peer (Raft) address.
    #[must_use]
    pub const fn peer_address(&self) -> SocketAddr {
        self.peer_address
    }

    /// The data directory, resolved against the config file's directory.
    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
}

/// Where each of a node's values was written, for duplicate errors.
#[derive(Clone, Copy, Debug)]
struct NodeLines {
    /// The `id` line.
    id: usize,
    /// The `client_address` line.
    client_address: usize,
    /// The `peer_address` line.
    peer_address: usize,
    /// The `data_dir` line.
    data_dir: usize,
}

/// The validated cluster configuration: one type for production and simulation (§7.10).
///
/// Invariants: every rule listed in the [module documentation](self) holds. In particular
/// every parameter lies in its [`ConfigParam`] range, the voters number 1, 3 or 5 with distinct
/// IDs, addresses and data directories, and every voter's [`ConsensusSettings`] pass the
/// consensus library's own checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The cluster's display name (§7.7 step 0).
    cluster_id: ClusterId,
    /// Every numeric parameter.
    values: Values,
    /// The key and value limits in force (D-80): `max_key_bytes` and `max_value_bytes`.
    key_value_limits: KeyValueLimits,
    /// The commit-index admission ceiling (DM-7): `max_log_entries` as a log index.
    max_log_entries: LogIndex,
    /// The voters, by ID.
    nodes: BTreeMap<NodeId, NodeConfig>,
}

impl Config {
    /// Parses a config file's text. Relative data directories resolve against
    /// `config_dir`, the directory holding the file (§7.7 step 0).
    ///
    /// # Errors
    ///
    /// The first rule the file breaks, as a specific [`ConfigError`], in the order given
    /// in the [module documentation](self).
    pub fn parse(text: &str, config_dir: &Path) -> Result<Self, ConfigError> {
        let bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
        if bytes > MAX_CONFIG_BYTES {
            return Err(ConfigError::FileTooLarge { bytes });
        }
        let document = reader::read(text)?;
        check_known_keys(&document)?;
        let cluster_id = read_cluster_id(&document.top)?;
        let values = read_values(&document.top)?;
        let key_value_limits = key_value_limits(&document.top, &values)?;
        let max_log_entries = history_ceiling(&document.top, &values)?;
        let nodes = read_nodes(&document, config_dir)?;
        rules::timing(&values)?;
        rules::limits(&values)?;
        let config = Self {
            cluster_id,
            values,
            key_value_limits,
            max_log_entries,
            nodes,
        };
        for &node in config.nodes.keys() {
            config
                .settings(node)
                .check()
                .map_err(|violation| ConfigError::Consensus { node, violation })?;
        }
        Ok(config)
    }

    /// The cluster's display name (§7.7 step 0).
    #[must_use]
    pub const fn cluster_id(&self) -> &ClusterId {
        &self.cluster_id
    }

    /// The value of a numeric parameter.
    #[must_use]
    pub const fn get(&self, param: ConfigParam) -> u64 {
        self.values.get(param)
    }

    /// The peer protocol version: [`PROTOCOL_VERSION`].
    #[must_use]
    pub const fn protocol_version(&self) -> u64 {
        self.values.protocol_version
    }

    /// The production tick length in milliseconds.
    #[must_use]
    pub const fn tick_ms(&self) -> u64 {
        self.values.tick_ms
    }

    /// The heartbeat interval.
    #[must_use]
    pub const fn heartbeat_interval(&self) -> Tick {
        Tick::new(self.values.heartbeat_ticks)
    }

    /// The election timeout's inclusive minimum (raft-rs `election_tick`).
    #[must_use]
    pub const fn election_min(&self) -> Tick {
        Tick::new(self.values.election_min_ticks)
    }

    /// The election timeout's exclusive maximum: twice the minimum (D-75).
    #[must_use]
    pub const fn election_max_exclusive(&self) -> Tick {
        Tick::new(self.values.election_max_ticks_exclusive)
    }

    /// The server write deadline, from admission.
    #[must_use]
    pub const fn write_deadline(&self) -> Tick {
        Tick::new(self.values.write_deadline_ticks)
    }

    /// The CLI's overall deadline in milliseconds.
    #[must_use]
    pub const fn cli_deadline_ms(&self) -> u64 {
        self.values.cli_deadline_ms
    }

    /// The key limit (DM-1).
    #[must_use]
    pub const fn max_key_bytes(&self) -> ByteCount {
        ByteCount::new(self.values.max_key_bytes)
    }

    /// The value limit (DM-2).
    #[must_use]
    pub const fn max_value_bytes(&self) -> ByteCount {
        ByteCount::new(self.values.max_value_bytes)
    }

    /// The key and value limits in force, for [`Key::parse`](crate::Key::parse) and
    /// [`Value::parse`](crate::Value::parse) at every boundary (D-80).
    #[must_use]
    pub const fn key_value_limits(&self) -> KeyValueLimits {
        self.key_value_limits
    }

    /// The live-key limit (DM-6).
    #[must_use]
    pub const fn max_live_keys(&self) -> ItemCount {
        ItemCount::new(self.values.max_live_keys)
    }

    /// The live key and value byte limit (DM-6).
    #[must_use]
    pub const fn max_live_bytes(&self) -> ByteCount {
        ByteCount::new(self.values.max_live_bytes)
    }

    /// The commit-index admission ceiling (DM-7): the leader refuses new mutations once its
    /// commit index reaches it.
    #[must_use]
    pub const fn max_log_entries(&self) -> LogIndex {
        self.max_log_entries
    }

    /// The first-execution command byte ceiling (DM-7).
    #[must_use]
    pub const fn max_history_bytes(&self) -> ByteCount {
        ByteCount::new(self.values.max_history_bytes)
    }

    /// The local journal hard stop (DM-7).
    #[must_use]
    pub const fn max_journal_bytes(&self) -> ByteCount {
        ByteCount::new(self.values.max_journal_bytes)
    }

    /// The largest replication batch.
    #[must_use]
    pub const fn replication_batch_bytes(&self) -> ByteCount {
        ByteCount::new(self.values.replication_batch_bytes)
    }

    /// Replication batches in flight per peer.
    #[must_use]
    pub const fn replication_inflight_batches(&self) -> ItemCount {
        ItemCount::new(self.values.replication_inflight_batches)
    }

    /// The per-peer send queue's message bound.
    #[must_use]
    pub const fn peer_send_queue_messages(&self) -> ItemCount {
        ItemCount::new(self.values.peer_send_queue_messages)
    }

    /// The per-peer send queue's byte bound.
    #[must_use]
    pub const fn peer_send_queue_bytes(&self) -> ByteCount {
        ByteCount::new(self.values.peer_send_queue_bytes)
    }

    /// Raft's uncommitted proposal bytes.
    #[must_use]
    pub const fn max_uncommitted_bytes(&self) -> ByteCount {
        ByteCount::new(self.values.max_uncommitted_bytes)
    }

    /// Committed entry bytes per Ready.
    #[must_use]
    pub const fn max_committed_bytes_per_ready(&self) -> ByteCount {
        ByteCount::new(self.values.max_committed_bytes_per_ready)
    }

    /// The voters, in ID order.
    pub fn nodes(&self) -> impl Iterator<Item = &NodeConfig> {
        self.nodes.values()
    }

    /// The voter with this ID, if any.
    #[must_use]
    pub fn node(&self, id: NodeId) -> Option<&NodeConfig> {
        self.nodes.get(&id)
    }

    /// The canonical voter set: the voters' IDs in ascending order (§7.10).
    pub fn voters(&self) -> impl Iterator<Item = NodeId> {
        self.nodes.keys().copied()
    }

    /// The number of voters: 1, 3 or 5.
    #[must_use]
    pub fn voter_count(&self) -> usize {
        self.nodes.len()
    }

    /// The consensus library's settings for voter `node`, or `None` if it is not a voter.
    #[must_use]
    pub fn consensus(&self, node: NodeId) -> Option<ConsensusSettings> {
        self.nodes.contains_key(&node).then(|| self.settings(node))
    }

    /// The consensus library's settings for `node` (§7.10: pre-vote and check-quorum on,
    /// `election_tick` = the election minimum, the range `[min, 2 × min)`).
    const fn settings(&self, node: NodeId) -> ConsensusSettings {
        let values = &self.values;
        ConsensusSettings {
            node_id: node,
            heartbeat_tick: Tick::new(values.heartbeat_ticks),
            election_tick: Tick::new(values.election_min_ticks),
            min_election_tick: Tick::new(values.election_min_ticks),
            max_election_tick: Tick::new(values.election_max_ticks_exclusive),
            max_size_per_msg: ByteCount::new(values.replication_batch_bytes),
            max_inflight_msgs: ItemCount::new(values.replication_inflight_batches),
            max_uncommitted_size: ByteCount::new(values.max_uncommitted_bytes),
            max_committed_size_per_ready: ByteCount::new(values.max_committed_bytes_per_ready),
            check_quorum: true,
            pre_vote: true,
            read_mode: ReadMode::Safe,
        }
    }
}

/// Refuses any key the config does not have.
fn check_known_keys(document: &Document) -> Result<(), ConfigError> {
    let unknown = |entry: &reader::Entry, table| ConfigError::UnknownKey {
        line: entry.line,
        key: entry.key.clone(),
        table,
    };
    for entry in &document.top.entries {
        if entry.key != CLUSTER_ID_KEY && ConfigParam::from_key(&entry.key).is_none() {
            return Err(unknown(entry, ConfigTable::Top));
        }
    }
    for (line, node) in &document.nodes {
        for entry in &node.entries {
            if !NODE_KEYS.contains(&entry.key.as_str()) {
                return Err(unknown(entry, ConfigTable::Node { line: *line }));
            }
        }
    }
    Ok(())
}

/// The integer at `key`, with its line, if present.
fn integer(table: &Table, key: &'static str) -> Result<Option<(u64, usize)>, ConfigError> {
    table.get(key).map_or(Ok(None), |entry| match &entry.value {
        Raw::Integer(value) => Ok(Some((*value, entry.line))),
        Raw::String(_) => Err(ConfigError::WrongType {
            line: entry.line,
            key,
            expected: ConfigValueKind::Integer,
        }),
    })
}

/// The string at `key`, with its line; refuses a missing key or an integer.
fn required_string<'a>(
    table: &'a Table,
    key: &'static str,
    at: ConfigTable,
) -> Result<(&'a str, usize), ConfigError> {
    let entry = table
        .get(key)
        .ok_or(ConfigError::MissingKey { key, table: at })?;
    match &entry.value {
        Raw::String(text) => Ok((text.as_str(), entry.line)),
        Raw::Integer(_) => Err(ConfigError::WrongType {
            line: entry.line,
            key,
            expected: ConfigValueKind::String,
        }),
    }
}

/// The cluster's display name.
fn read_cluster_id(top: &Table) -> Result<ClusterId, ConfigError> {
    let (text, line) = required_string(top, CLUSTER_ID_KEY, ConfigTable::Top)?;
    ClusterId::parse(text.to_owned()).map_err(|error| ConfigError::ClusterId { line, error })
}

/// One numeric parameter: its value, in range, or its default if it is optional.
fn read_param(top: &Table, param: ConfigParam) -> Result<u64, ConfigError> {
    match integer(top, param.key())? {
        Some((value, _)) if param.allows(value) => Ok(value),
        Some((value, line)) => Err(ConfigError::OutOfRange { line, param, value }),
        None => param
            .default_value()
            .ok_or_else(|| ConfigError::MissingKey {
                key: param.key(),
                table: ConfigTable::Top,
            }),
    }
}

/// Every numeric parameter, read in [`ConfigParam::ALL`]'s order.
fn read_values(top: &Table) -> Result<Values, ConfigError> {
    let read = |param| read_param(top, param);
    Ok(Values {
        protocol_version: read(ConfigParam::ProtocolVersion)?,
        tick_ms: read(ConfigParam::TickMs)?,
        heartbeat_ticks: read(ConfigParam::HeartbeatTicks)?,
        election_min_ticks: read(ConfigParam::ElectionMinTicks)?,
        election_max_ticks_exclusive: read(ConfigParam::ElectionMaxTicksExclusive)?,
        write_deadline_ticks: read(ConfigParam::WriteDeadlineTicks)?,
        cli_deadline_ms: read(ConfigParam::CliDeadlineMs)?,
        max_key_bytes: read(ConfigParam::MaxKeyBytes)?,
        max_value_bytes: read(ConfigParam::MaxValueBytes)?,
        max_live_keys: read(ConfigParam::MaxLiveKeys)?,
        max_live_bytes: read(ConfigParam::MaxLiveBytes)?,
        max_log_entries: read(ConfigParam::MaxLogEntries)?,
        max_history_bytes: read(ConfigParam::MaxHistoryBytes)?,
        max_journal_bytes: read(ConfigParam::MaxJournalBytes)?,
        replication_batch_bytes: read(ConfigParam::ReplicationBatchBytes)?,
        replication_inflight_batches: read(ConfigParam::ReplicationInflightBatches)?,
        peer_send_queue_messages: read(ConfigParam::PeerSendQueueMessages)?,
        peer_send_queue_bytes: read(ConfigParam::PeerSendQueueBytes)?,
        max_uncommitted_bytes: read(ConfigParam::MaxUncommittedBytes)?,
        max_committed_bytes_per_ready: read(ConfigParam::MaxCommittedBytesPerReady)?,
    })
}

/// The key and value limits from `max_key_bytes` and `max_value_bytes`. Their
/// [`ConfigParam`] ranges are exactly [`KeyValueLimits`]'s, so this refuses nothing that
/// [`read_values`] admitted; the error names the parameter if they ever drift apart.
fn key_value_limits(top: &Table, values: &Values) -> Result<KeyValueLimits, ConfigError> {
    let key = usize::try_from(values.max_key_bytes).unwrap_or(usize::MAX);
    let value = usize::try_from(values.max_value_bytes).unwrap_or(usize::MAX);
    KeyValueLimits::new(key, value).ok_or_else(|| {
        let param = KeyValueLimits::new(key, 0)
            .map_or(ConfigParam::MaxKeyBytes, |_| ConfigParam::MaxValueBytes);
        ConfigError::OutOfRange {
            line: top.get(param.key()).map_or(0, |entry| entry.line),
            param,
            value: values.get(param),
        }
    })
}

/// The commit-index admission ceiling as a [`LogIndex`]. The `max_log_entries` range lies
/// far inside a log index's, so this refuses nothing that [`read_values`] admitted; the
/// error names the parameter if they ever drift apart.
fn history_ceiling(top: &Table, values: &Values) -> Result<LogIndex, ConfigError> {
    LogIndex::new(values.max_log_entries).map_err(|_| {
        let param = ConfigParam::MaxLogEntries;
        ConfigError::OutOfRange {
            line: top.get(param.key()).map_or(0, |entry| entry.line),
            param,
            value: values.max_log_entries,
        }
    })
}

/// A client or peer address: an IP address and a nonzero port.
fn read_address(
    node: &Table,
    key: &'static str,
    at: ConfigTable,
) -> Result<(SocketAddr, usize), ConfigError> {
    let (text, line) = required_string(node, key, at)?;
    let address: SocketAddr = text.parse().map_err(|_| ConfigError::Address {
        line,
        key,
        problem: AddressProblem::NotIpAndPort,
    })?;
    if address.port() == 0 {
        return Err(ConfigError::Address {
            line,
            key,
            problem: AddressProblem::PortZero,
        });
    }
    Ok((address, line))
}

/// One `[[nodes]]` table whose header is on `header`.
fn read_node(
    header: usize,
    node: &Table,
    config_dir: &Path,
) -> Result<(NodeConfig, NodeLines), ConfigError> {
    let at = ConfigTable::Node { line: header };
    let (raw_id, id_line) = integer(node, "id")?.ok_or(ConfigError::MissingKey {
        key: "id",
        table: at,
    })?;
    let id = NodeId::new(raw_id).map_err(|error| ConfigError::NodeId {
        line: id_line,
        error,
    })?;
    let (client_address, client_line) = read_address(node, "client_address", at)?;
    let (peer_address, peer_line) = read_address(node, "peer_address", at)?;
    let (data_dir, data_dir_line) = required_string(node, "data_dir", at)?;
    if data_dir.is_empty() {
        return Err(ConfigError::EmptyDataDir {
            line: data_dir_line,
        });
    }
    let config = NodeConfig {
        id,
        client_address,
        peer_address,
        data_dir: normalise(&config_dir.join(data_dir)),
    };
    let lines = NodeLines {
        id: id_line,
        client_address: client_line,
        peer_address: peer_line,
        data_dir: data_dir_line,
    };
    Ok((config, lines))
}

/// Every voter, checking the voter count and that no ID, address or data directory
/// repeats.
fn read_nodes(
    document: &Document,
    config_dir: &Path,
) -> Result<BTreeMap<NodeId, NodeConfig>, ConfigError> {
    let count = document.nodes.len();
    if ![1, 3, 5].contains(&count) {
        return Err(ConfigError::VoterCount { count });
    }
    let mut parsed = Vec::with_capacity(count);
    for (header, node) in &document.nodes {
        parsed.push(read_node(*header, node, config_dir)?);
    }
    let mut ids = BTreeMap::new();
    for (node, lines) in &parsed {
        if let Some(&first_line) = ids.get(&node.id) {
            return Err(ConfigError::DuplicateNodeId {
                id: node.id,
                line: lines.id,
                first_line,
            });
        }
        ids.insert(node.id, lines.id);
    }
    let mut addresses = BTreeMap::new();
    for (node, lines) in &parsed {
        let uses = [
            (node.client_address, lines.client_address),
            (node.peer_address, lines.peer_address),
        ];
        for (address, line) in uses {
            if let Some(&first_line) = addresses.get(&address) {
                return Err(ConfigError::DuplicateAddress {
                    address,
                    line,
                    first_line,
                });
            }
            addresses.insert(address, line);
        }
    }
    let mut data_dirs = BTreeMap::new();
    for (node, lines) in &parsed {
        if let Some(&first_line) = data_dirs.get(&node.data_dir) {
            return Err(ConfigError::DuplicateDataDir {
                path: node.data_dir.clone(),
                line: lines.data_dir,
                first_line,
            });
        }
        data_dirs.insert(node.data_dir.clone(), lines.data_dir);
    }
    Ok(parsed
        .into_iter()
        .map(|(node, _)| (node.id, node))
        .collect())
}

/// What a `..` does to the path normalised so far.
enum Parent {
    /// It cancels the last directory name.
    Pop,
    /// It stays at the root: `/..` is `/`.
    Stay,
    /// Nothing to cancel in a relative path: the `..` is kept.
    Keep,
}

/// `path` with `.` removed and each `..` folded into the component before it, lexically:
/// no filesystem access, so symbolic links are not followed.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let parent = match out.components().next_back() {
                    Some(Component::Normal(_)) => Parent::Pop,
                    Some(Component::RootDir) => Parent::Stay,
                    Some(Component::Prefix(_) | Component::CurDir | Component::ParentDir)
                    | None => Parent::Keep,
                };
                match parent {
                    Parent::Pop => {
                        out.pop();
                    }
                    Parent::Stay => {}
                    Parent::Keep => out.push(Component::ParentDir),
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                out.push(component);
            }
        }
    }
    if out.as_os_str().is_empty() {
        out.push(Component::CurDir);
    }
    out
}
