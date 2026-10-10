//! The config file's numeric parameters, each with its allowed range, and the fixed
//! format limits the cross-limit rules check them against (PLAN.md §7.10, §7.11, D-79).
//!
//! This is the one table of parameter bounds: the parser enforces it, and W08.3's limits
//! registry extends it, so no bound lives in two places. Semantic and queue maxima are the
//! §7.11 defaults, the values the plan sizes and measures; a configuration may lower them
//! (test profiles do) but never raise them.

use core::fmt;

use crate::{Key, Value};

/// One kibibyte.
const KIB: u64 = 1024;
/// One mebibyte.
const MIB: u64 = 1024 * KIB;
/// One gibibyte.
const GIB: u64 = 1024 * MIB;

/// The peer protocol version this build speaks (§8.2). A config naming any other version
/// is refused.
pub const PROTOCOL_VERSION: u64 = 1;

/// The canonical command encoding's version (§7.5). The command codec (W09) writes it,
/// and the semantic-config fingerprint covers it (§7.10, DUR-8).
pub const COMMAND_FORMAT_VERSION: u64 = 1;

/// The SCAN page's cap on decoded key and value bytes (1 MiB, §7.11): a fixed semantic
/// constant, covered by the semantic-config fingerprint. The item cap is
/// [`PageSize::MAX`](crate::PageSize::MAX).
pub const MAX_SCAN_PAGE_BYTES: u64 = MIB;

/// The largest config file, in bytes (CD-9). A five-node file is under 2 KiB, so this bounds
/// every allocation the reader makes without constraining any real configuration.
pub const MAX_CONFIG_BYTES: u64 = 64 * KIB;

/// The canonical-command limit (66 KiB, §7.11): no encoded command is larger.
pub const MAX_COMMAND_BYTES: u64 = 66 * KIB;

/// The bound on a canonical command's envelope: everything but the key and value bytes.
///
/// `max_key_bytes + max_value_bytes + COMMAND_ENVELOPE_BOUND` must fit
/// [`MAX_COMMAND_BYTES`] (§7.10); the command codec (W09) proves its envelope fits.
pub const COMMAND_ENVELOPE_BOUND: u64 = KIB;

/// The journal-frame payload limit (1 MiB, §7.6, §7.11).
pub const MAX_FRAME_PAYLOAD_BYTES: u64 = MIB;

/// The HTTP request-body limit (512 KiB, §7.11).
pub const MAX_REQUEST_BODY_BYTES: u64 = 512 * KIB;

/// The fixed fields of a journal frame (§7.6), in order, with their sizes in bytes: every
/// field except the variable payload.
pub const FRAME_FIELDS: [(&str, u64); 9] = [
    ("magic", 4),
    ("format version", 2),
    ("kind", 1),
    ("reserved", 1),
    ("payload length", 4),
    ("frame sequence", 8),
    ("header checksum", 4),
    ("checksum", 4),
    ("trailing length", 4),
];

/// A journal frame's overhead in bytes: the sum of [`FRAME_FIELDS`] (32 B).
pub const FRAME_OVERHEAD_BYTES: u64 = frame_overhead();

/// The bound on one log entry's envelope inside a `RaftBatch` payload: index, term, entry
/// type and length prefixes. The journal codec (W11) proves its envelope fits.
pub const ENTRY_ENVELOPE_BOUND: u64 = 32;

/// The per-entry journal overhead bound of §7.10's margin formula: a frame of its own plus
/// the entry's envelope (64 B, D-68).
pub const PER_ENTRY_OVERHEAD_BOUND: u64 = FRAME_OVERHEAD_BYTES + ENTRY_ENVELOPE_BOUND;

/// `journal_protocol_allowance` (64 MiB, §7.11): the named constant in §7.10's journal
/// margin formula, covering protocol frames and truncation slack.
pub const JOURNAL_PROTOCOL_ALLOWANCE: u64 = 64 * MIB;

/// Bytes of protobuf framing one Raft entry adds beyond its data (D-81, §7.11): the
/// default and floor of `raft_entry_overhead_bound`.
pub const RAFT_ENTRY_OVERHEAD_DEFAULT: u64 = 32;

/// Bytes of envelope and message header one peer message adds beyond its entries (D-81,
/// §7.11): the default and floor of `peer_message_overhead_bound`.
pub const PEER_MESSAGE_OVERHEAD_DEFAULT: u64 = 256;

/// The sum of [`FRAME_FIELDS`].
const fn frame_overhead() -> u64 {
    let mut total = 0;
    let mut field = 0;
    while field < FRAME_FIELDS.len() {
        total += FRAME_FIELDS[field].1;
        field += 1;
    }
    total
}

/// How a parameter takes part in the cluster's identity (DUR-8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigParamClass {
    /// The peer protocol: identical on every node and carried in every peer envelope.
    Protocol,
    /// Changes results, so it is identical on every node and covered by the
    /// semantic-config fingerprint (DUR-8, §7.10).
    Semantic,
    /// Controls progress and give-up points only, never commitment, versions or history
    /// retention (§7.10).
    Timing,
    /// A node-local bound on journal bytes, batching or queues: it affects admission and
    /// latency only, and is not fingerprinted (§7.11).
    Local,
}

/// A numeric key of the config file, with its allowed range and, for the optional local
/// limits, its default (§7.10, §7.11, D-79).
///
/// Invariant: `minimum() <= maximum()`, and an optional parameter's default lies in its range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConfigParam {
    /// `protocol_version`: exactly [`PROTOCOL_VERSION`].
    ProtocolVersion,
    /// `tick_ms`: the production tick length in milliseconds, 1 to 1,000.
    TickMs,
    /// `heartbeat_ticks`: the heartbeat interval, 1 to 1,000 ticks.
    HeartbeatTicks,
    /// `election_min_ticks`: the election timeout's inclusive minimum, 1 to 10,000 ticks;
    /// it is also raft-rs's `election_tick`.
    ElectionMinTicks,
    /// `election_max_ticks_exclusive`: the election timeout's exclusive maximum, 1 to
    /// 20,000 ticks.
    ElectionMaxTicksExclusive,
    /// `write_deadline_ticks`: the server write deadline from admission, 1 to 1,000,000
    /// ticks.
    WriteDeadlineTicks,
    /// `cli_deadline_ms`: the CLI's overall deadline, 1 ms to 24 hours.
    CliDeadlineMs,
    /// `max_key_bytes`: the key limit, 1 to [`Key::MAX_BYTES`] (DM-1).
    MaxKeyBytes,
    /// `max_value_bytes`: the value limit, 0 to [`Value::MAX_BYTES`] (DM-2).
    MaxValueBytes,
    /// `max_live_keys`: live keys, 1 to 10,000 (DM-6).
    MaxLiveKeys,
    /// `max_live_bytes`: live key and value bytes, 1 B to 64 MiB (DM-6).
    MaxLiveBytes,
    /// `max_log_entries`: the commit-index admission ceiling, 1 to 1,000,000 (DM-7).
    MaxLogEntries,
    /// `max_history_bytes`: first-execution command bytes, 1 B to 3 GiB (DM-7).
    MaxHistoryBytes,
    /// `max_journal_bytes`: the local journal hard stop, 1 B to 1 TiB (DM-7).
    MaxJournalBytes,
    /// `replication_batch_bytes`: the largest replication batch (raft-rs
    /// `max_size_per_msg`), 1 B to 256 KiB; default 256 KiB.
    ReplicationBatchBytes,
    /// `replication_inflight_batches`: batches in flight per peer (raft-rs
    /// `max_inflight_msgs`), 1 to 16; default 16.
    ReplicationInflightBatches,
    /// `peer_send_queue_messages`: the per-peer send queue's message bound, 1 to 64;
    /// default 64.
    PeerSendQueueMessages,
    /// `peer_send_queue_bytes`: the per-peer send queue's byte bound, 1 B to 8 MiB;
    /// default 8 MiB.
    PeerSendQueueBytes,
    /// `max_uncommitted_bytes`: Raft's uncommitted proposal bytes (raft-rs
    /// `max_uncommitted_size`), 1 B to 4 MiB; default 4 MiB.
    MaxUncommittedBytes,
    /// `max_committed_bytes_per_ready`: committed entry bytes per Ready (raft-rs
    /// `max_committed_size_per_ready`), 1 B to 1 MiB; default 1 MiB.
    MaxCommittedBytesPerReady,
    /// `raft_entry_overhead_bound`: the bytes one entry's protobuf framing adds beyond
    /// its data (D-81), 32 B to 64 KiB; default 32 B. Raising it only widens the
    /// send-queue margin.
    RaftEntryOverheadBound,
    /// `peer_message_overhead_bound`: the bytes one peer message's envelope and header
    /// add beyond its entries (D-81), 256 B to 64 KiB; default 256 B. Raising it only
    /// widens the send-queue margin.
    PeerMessageOverheadBound,
}

impl ConfigParam {
    /// Every parameter, in the order the parser reads and checks them.
    pub const ALL: [Self; 22] = [
        Self::ProtocolVersion,
        Self::TickMs,
        Self::HeartbeatTicks,
        Self::ElectionMinTicks,
        Self::ElectionMaxTicksExclusive,
        Self::WriteDeadlineTicks,
        Self::CliDeadlineMs,
        Self::MaxKeyBytes,
        Self::MaxValueBytes,
        Self::MaxLiveKeys,
        Self::MaxLiveBytes,
        Self::MaxLogEntries,
        Self::MaxHistoryBytes,
        Self::MaxJournalBytes,
        Self::ReplicationBatchBytes,
        Self::ReplicationInflightBatches,
        Self::PeerSendQueueMessages,
        Self::PeerSendQueueBytes,
        Self::MaxUncommittedBytes,
        Self::MaxCommittedBytesPerReady,
        Self::RaftEntryOverheadBound,
        Self::PeerMessageOverheadBound,
    ];

    /// The parameter's key in the config file.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::ProtocolVersion => "protocol_version",
            Self::TickMs => "tick_ms",
            Self::HeartbeatTicks => "heartbeat_ticks",
            Self::ElectionMinTicks => "election_min_ticks",
            Self::ElectionMaxTicksExclusive => "election_max_ticks_exclusive",
            Self::WriteDeadlineTicks => "write_deadline_ticks",
            Self::CliDeadlineMs => "cli_deadline_ms",
            Self::MaxKeyBytes => "max_key_bytes",
            Self::MaxValueBytes => "max_value_bytes",
            Self::MaxLiveKeys => "max_live_keys",
            Self::MaxLiveBytes => "max_live_bytes",
            Self::MaxLogEntries => "max_log_entries",
            Self::MaxHistoryBytes => "max_history_bytes",
            Self::MaxJournalBytes => "max_journal_bytes",
            Self::ReplicationBatchBytes => "replication_batch_bytes",
            Self::ReplicationInflightBatches => "replication_inflight_batches",
            Self::PeerSendQueueMessages => "peer_send_queue_messages",
            Self::PeerSendQueueBytes => "peer_send_queue_bytes",
            Self::MaxUncommittedBytes => "max_uncommitted_bytes",
            Self::MaxCommittedBytesPerReady => "max_committed_bytes_per_ready",
            Self::RaftEntryOverheadBound => "raft_entry_overhead_bound",
            Self::PeerMessageOverheadBound => "peer_message_overhead_bound",
        }
    }

    /// The parameter whose key is `key`, if any.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|param| param.key() == key)
    }

    /// The smallest allowed value.
    #[must_use]
    pub const fn minimum(self) -> u64 {
        match self {
            Self::MaxValueBytes => 0,
            Self::RaftEntryOverheadBound => RAFT_ENTRY_OVERHEAD_DEFAULT,
            Self::PeerMessageOverheadBound => PEER_MESSAGE_OVERHEAD_DEFAULT,
            Self::ProtocolVersion
            | Self::TickMs
            | Self::HeartbeatTicks
            | Self::ElectionMinTicks
            | Self::ElectionMaxTicksExclusive
            | Self::WriteDeadlineTicks
            | Self::CliDeadlineMs
            | Self::MaxKeyBytes
            | Self::MaxLiveKeys
            | Self::MaxLiveBytes
            | Self::MaxLogEntries
            | Self::MaxHistoryBytes
            | Self::MaxJournalBytes
            | Self::ReplicationBatchBytes
            | Self::ReplicationInflightBatches
            | Self::PeerSendQueueMessages
            | Self::PeerSendQueueBytes
            | Self::MaxUncommittedBytes
            | Self::MaxCommittedBytesPerReady => 1,
        }
    }

    /// The largest allowed value.
    #[must_use]
    pub const fn maximum(self) -> u64 {
        match self {
            Self::ProtocolVersion => PROTOCOL_VERSION,
            Self::TickMs | Self::HeartbeatTicks => 1_000,
            Self::ElectionMinTicks | Self::MaxLiveKeys => 10_000,
            Self::ElectionMaxTicksExclusive => 20_000,
            Self::WriteDeadlineTicks | Self::MaxLogEntries => 1_000_000,
            Self::CliDeadlineMs => 24 * 60 * 60 * 1_000,
            Self::MaxKeyBytes => Key::MAX_BYTES as u64,
            Self::MaxValueBytes => Value::MAX_BYTES as u64,
            Self::MaxLiveBytes => 64 * MIB,
            Self::MaxHistoryBytes => 3 * GIB,
            Self::MaxJournalBytes => 1024 * GIB,
            Self::ReplicationBatchBytes => 256 * KIB,
            Self::ReplicationInflightBatches => 16,
            Self::PeerSendQueueMessages => 64,
            Self::PeerSendQueueBytes => 8 * MIB,
            Self::MaxUncommittedBytes => 4 * MIB,
            Self::MaxCommittedBytesPerReady => MIB,
            Self::RaftEntryOverheadBound | Self::PeerMessageOverheadBound => 64 * KIB,
        }
    }

    /// The default of an optional parameter: the §7.11 value. `None` for a required one
    /// (every §7.10 key is required, D-79).
    #[must_use]
    pub const fn default_value(self) -> Option<u64> {
        match self {
            Self::ReplicationBatchBytes
            | Self::ReplicationInflightBatches
            | Self::PeerSendQueueMessages
            | Self::PeerSendQueueBytes
            | Self::MaxUncommittedBytes
            | Self::MaxCommittedBytesPerReady => Some(self.maximum()),
            Self::RaftEntryOverheadBound | Self::PeerMessageOverheadBound => Some(self.minimum()),
            Self::ProtocolVersion
            | Self::TickMs
            | Self::HeartbeatTicks
            | Self::ElectionMinTicks
            | Self::ElectionMaxTicksExclusive
            | Self::WriteDeadlineTicks
            | Self::CliDeadlineMs
            | Self::MaxKeyBytes
            | Self::MaxValueBytes
            | Self::MaxLiveKeys
            | Self::MaxLiveBytes
            | Self::MaxLogEntries
            | Self::MaxHistoryBytes
            | Self::MaxJournalBytes => None,
        }
    }

    /// How the parameter takes part in the cluster's identity.
    #[must_use]
    pub const fn class(self) -> ConfigParamClass {
        match self {
            Self::ProtocolVersion => ConfigParamClass::Protocol,
            Self::TickMs
            | Self::HeartbeatTicks
            | Self::ElectionMinTicks
            | Self::ElectionMaxTicksExclusive
            | Self::WriteDeadlineTicks
            | Self::CliDeadlineMs => ConfigParamClass::Timing,
            Self::MaxKeyBytes
            | Self::MaxValueBytes
            | Self::MaxLiveKeys
            | Self::MaxLiveBytes
            | Self::MaxLogEntries
            | Self::MaxHistoryBytes => ConfigParamClass::Semantic,
            Self::MaxJournalBytes
            | Self::ReplicationBatchBytes
            | Self::ReplicationInflightBatches
            | Self::PeerSendQueueMessages
            | Self::PeerSendQueueBytes
            | Self::MaxUncommittedBytes
            | Self::MaxCommittedBytesPerReady
            | Self::RaftEntryOverheadBound
            | Self::PeerMessageOverheadBound => ConfigParamClass::Local,
        }
    }

    /// Whether `value` lies in `minimum()..=maximum()`.
    #[must_use]
    pub const fn allows(self, value: u64) -> bool {
        value >= self.minimum() && value <= self.maximum()
    }
}

impl fmt::Display for ConfigParam {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}
