//! The limits registry (PLAN.md §7.11, CD-9): every bound the system enforces, named
//! once.
//!
//! Each [`Limit`] says where its value comes from:
//! - a configured parameter ([`ConfigParam`]), whose allowed range the config parser
//!   enforces (§7.10); or
//! - a fixed value, defined here or by the type that owns it, and nowhere else.
//!
//! [`Config::limit`] resolves any limit for a configuration. The API spec's byte, body
//! and page bounds are checked against this table by a test, so no bound lives in two
//! places (§7.10). Every allocation-bearing collection gets an entry here before it is
//! introduced (§7.11), and is built from [`crate::BoundedQueue`] or [`crate::BoundedMap`],
//! whose refusals are typed.

use crate::config::{
    ConfigParam, JOURNAL_PROTOCOL_ALLOWANCE, MAX_COMMAND_BYTES, MAX_FRAME_PAYLOAD_BYTES,
    MAX_REQUEST_BODY_BYTES, MAX_SCAN_PAGE_BYTES,
};
use crate::{Config, PageSize, ReadLen};

/// One mebibyte.
const MIB: u64 = 1024 * 1024;

/// Where a [`Limit`]'s value comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitSource {
    /// A configured parameter, within its declared range (§7.10).
    Config(ConfigParam),
    /// A fixed value: a format constant, or a local bound v1 does not configure.
    Fixed(u64),
}

/// A bound from the §7.11 limits table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Limit {
    /// Key bytes (DM-1, D-80): `max_key_bytes`.
    KeyBytes,
    /// Value bytes (DM-2, D-80): `max_value_bytes`.
    ValueBytes,
    /// HTTP request body bytes: 512 KiB.
    RequestBodyBytes,
    /// Canonical command bytes: 66 KiB.
    CommandBytes,
    /// SCAN page items: 128.
    ScanPageItems,
    /// SCAN page decoded key and value bytes: 1 MiB.
    ScanPageBytes,
    /// Live keys (DM-6): `max_live_keys`.
    LiveKeys,
    /// Live key and value bytes (DM-6): `max_live_bytes`.
    LiveBytes,
    /// The commit-index admission ceiling (DM-7): `max_log_entries`.
    HistoryEntries,
    /// First-execution command bytes (DM-7): `max_history_bytes`.
    HistoryBytes,
    /// The local journal hard stop (DM-7): `max_journal_bytes`.
    JournalBytes,
    /// `journal_protocol_allowance` in the margin formula: 64 MiB.
    JournalProtocolAllowance,
    /// Committed entry bytes per Ready: `max_committed_bytes_per_ready`.
    CommittedBytesPerReady,
    /// Pending client attempts per node: 256.
    PendingAttempts,
    /// Pending client attempt bytes per node: 16 MiB.
    PendingAttemptBytes,
    /// Waiters per request ID: 16.
    WaitersPerRequest,
    /// Raft uncommitted proposal bytes: `max_uncommitted_bytes`.
    UncommittedBytes,
    /// Entry cache bytes: 8 MiB.
    EntryCacheBytes,
    /// Per-peer send queue messages: `peer_send_queue_messages`.
    PeerQueueMessages,
    /// Per-peer send queue bytes: `peer_send_queue_bytes`.
    PeerQueueBytes,
    /// Replication batch bytes: `replication_batch_bytes`.
    ReplicationBatchBytes,
    /// Replication batches in flight per peer: `replication_inflight_batches`.
    ReplicationInflightBatches,
    /// Journal frame payload bytes: 1 MiB.
    FramePayloadBytes,
    /// Journal read chunk bytes: 256 KiB.
    ReadChunkBytes,
    /// Inbound controller queue inputs: 1,024.
    ControllerQueueInputs,
    /// CLI automatic retry attempts: 10.
    CliRetryAttempts,
    /// Distinct nodes a CLI retry sequence tries: 5.
    CliRetryNodes,
    /// The CLI's overall deadline in milliseconds: `cli_deadline_ms`.
    CliDeadlineMs,
    /// Structured log queue records: 4,096.
    LogQueueRecords,
    /// Simulation events per run: 1,000,000 by default (per scenario manifest).
    SimEventsPerRun,
}

impl Limit {
    /// Every limit, in §7.11 order.
    pub const ALL: [Self; 30] = [
        Self::KeyBytes,
        Self::ValueBytes,
        Self::RequestBodyBytes,
        Self::CommandBytes,
        Self::ScanPageItems,
        Self::ScanPageBytes,
        Self::LiveKeys,
        Self::LiveBytes,
        Self::HistoryEntries,
        Self::HistoryBytes,
        Self::JournalBytes,
        Self::JournalProtocolAllowance,
        Self::CommittedBytesPerReady,
        Self::PendingAttempts,
        Self::PendingAttemptBytes,
        Self::WaitersPerRequest,
        Self::UncommittedBytes,
        Self::EntryCacheBytes,
        Self::PeerQueueMessages,
        Self::PeerQueueBytes,
        Self::ReplicationBatchBytes,
        Self::ReplicationInflightBatches,
        Self::FramePayloadBytes,
        Self::ReadChunkBytes,
        Self::ControllerQueueInputs,
        Self::CliRetryAttempts,
        Self::CliRetryNodes,
        Self::CliDeadlineMs,
        Self::LogQueueRecords,
        Self::SimEventsPerRun,
    ];

    /// The limit's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::KeyBytes => "key_bytes",
            Self::ValueBytes => "value_bytes",
            Self::RequestBodyBytes => "request_body_bytes",
            Self::CommandBytes => "command_bytes",
            Self::ScanPageItems => "scan_page_items",
            Self::ScanPageBytes => "scan_page_bytes",
            Self::LiveKeys => "live_keys",
            Self::LiveBytes => "live_bytes",
            Self::HistoryEntries => "history_entries",
            Self::HistoryBytes => "history_bytes",
            Self::JournalBytes => "journal_bytes",
            Self::JournalProtocolAllowance => "journal_protocol_allowance",
            Self::CommittedBytesPerReady => "committed_bytes_per_ready",
            Self::PendingAttempts => "pending_attempts",
            Self::PendingAttemptBytes => "pending_attempt_bytes",
            Self::WaitersPerRequest => "waiters_per_request",
            Self::UncommittedBytes => "uncommitted_bytes",
            Self::EntryCacheBytes => "entry_cache_bytes",
            Self::PeerQueueMessages => "peer_queue_messages",
            Self::PeerQueueBytes => "peer_queue_bytes",
            Self::ReplicationBatchBytes => "replication_batch_bytes",
            Self::ReplicationInflightBatches => "replication_inflight_batches",
            Self::FramePayloadBytes => "frame_payload_bytes",
            Self::ReadChunkBytes => "read_chunk_bytes",
            Self::ControllerQueueInputs => "controller_queue_inputs",
            Self::CliRetryAttempts => "cli_retry_attempts",
            Self::CliRetryNodes => "cli_retry_nodes",
            Self::CliDeadlineMs => "cli_deadline_ms",
            Self::LogQueueRecords => "log_queue_records",
            Self::SimEventsPerRun => "sim_events_per_run",
        }
    }

    /// Where the limit's value comes from. Fixed values are defined here, or by the
    /// type that owns the bound.
    #[must_use]
    pub const fn source(self) -> LimitSource {
        match self {
            Self::KeyBytes => LimitSource::Config(ConfigParam::MaxKeyBytes),
            Self::ValueBytes => LimitSource::Config(ConfigParam::MaxValueBytes),
            Self::RequestBodyBytes => LimitSource::Fixed(MAX_REQUEST_BODY_BYTES),
            Self::CommandBytes => LimitSource::Fixed(MAX_COMMAND_BYTES),
            Self::ScanPageItems => LimitSource::Fixed(PageSize::MAX),
            Self::ScanPageBytes => LimitSource::Fixed(MAX_SCAN_PAGE_BYTES),
            Self::LiveKeys => LimitSource::Config(ConfigParam::MaxLiveKeys),
            Self::LiveBytes => LimitSource::Config(ConfigParam::MaxLiveBytes),
            Self::HistoryEntries => LimitSource::Config(ConfigParam::MaxLogEntries),
            Self::HistoryBytes => LimitSource::Config(ConfigParam::MaxHistoryBytes),
            Self::JournalBytes => LimitSource::Config(ConfigParam::MaxJournalBytes),
            Self::JournalProtocolAllowance => LimitSource::Fixed(JOURNAL_PROTOCOL_ALLOWANCE),
            Self::CommittedBytesPerReady => {
                LimitSource::Config(ConfigParam::MaxCommittedBytesPerReady)
            }
            Self::PendingAttempts => LimitSource::Fixed(256),
            Self::PendingAttemptBytes => LimitSource::Fixed(16 * MIB),
            Self::WaitersPerRequest => LimitSource::Fixed(16),
            Self::UncommittedBytes => LimitSource::Config(ConfigParam::MaxUncommittedBytes),
            Self::EntryCacheBytes => LimitSource::Fixed(8 * MIB),
            Self::PeerQueueMessages => LimitSource::Config(ConfigParam::PeerSendQueueMessages),
            Self::PeerQueueBytes => LimitSource::Config(ConfigParam::PeerSendQueueBytes),
            Self::ReplicationBatchBytes => LimitSource::Config(ConfigParam::ReplicationBatchBytes),
            Self::ReplicationInflightBatches => {
                LimitSource::Config(ConfigParam::ReplicationInflightBatches)
            }
            Self::FramePayloadBytes => LimitSource::Fixed(MAX_FRAME_PAYLOAD_BYTES),
            Self::ReadChunkBytes => LimitSource::Fixed(ReadLen::MAX as u64),
            Self::ControllerQueueInputs => LimitSource::Fixed(1024),
            Self::CliRetryAttempts => LimitSource::Fixed(10),
            Self::CliRetryNodes => LimitSource::Fixed(5),
            Self::CliDeadlineMs => LimitSource::Config(ConfigParam::CliDeadlineMs),
            Self::LogQueueRecords => LimitSource::Fixed(4096),
            Self::SimEventsPerRun => LimitSource::Fixed(1_000_000),
        }
    }

    /// The smallest value the limit can take: its parameter's minimum, or its fixed
    /// value.
    #[must_use]
    pub const fn minimum(self) -> u64 {
        match self.source() {
            LimitSource::Config(param) => param.minimum(),
            LimitSource::Fixed(value) => value,
        }
    }

    /// The largest value the limit can take: its parameter's maximum, or its fixed
    /// value.
    #[must_use]
    pub const fn maximum(self) -> u64 {
        match self.source() {
            LimitSource::Config(param) => param.maximum(),
            LimitSource::Fixed(value) => value,
        }
    }
}

impl Config {
    /// The value of `limit` under this configuration (§7.11).
    #[must_use]
    pub const fn limit(&self, limit: Limit) -> u64 {
        match limit.source() {
            LimitSource::Config(param) => self.get(param),
            LimitSource::Fixed(value) => value,
        }
    }
}

#[cfg(test)]
mod tests {
    // check: LIMITS

    use std::path::Path;

    use super::{Limit, LimitSource};
    use crate::config::ConfigParam;
    use crate::{Config, Key, Value};

    /// The repository's local cluster config.
    const LOCAL: &str = include_str!("../../../config/local.toml");

    /// The API contract.
    const SPEC: &str = include_str!("../../../api/openapi.yaml");

    /// Every integer that follows `marker` in the spec.
    fn spec_values(marker: &str) -> Vec<u64> {
        SPEC.lines()
            .filter_map(|line| line.trim().strip_prefix(marker))
            .filter_map(|value| value.trim().parse().ok())
            .collect()
    }

    #[test]
    fn the_registry_matches_the_section_7_11_table() {
        let local = Config::parse(LOCAL, Path::new("config")).unwrap();
        // Written by hand from PLAN.md §7.11 (and §7.10 for the configured defaults).
        let expected: [(Limit, u64); 30] = [
            (Limit::KeyBytes, 256),
            (Limit::ValueBytes, 65_536),
            (Limit::RequestBodyBytes, 512 * 1024),
            (Limit::CommandBytes, 66 * 1024),
            (Limit::ScanPageItems, 128),
            (Limit::ScanPageBytes, 1 << 20),
            (Limit::LiveKeys, 10_000),
            (Limit::LiveBytes, 64 << 20),
            (Limit::HistoryEntries, 1_000_000),
            (Limit::HistoryBytes, 3 << 30),
            (Limit::JournalBytes, 4 << 30),
            (Limit::JournalProtocolAllowance, 64 << 20),
            (Limit::CommittedBytesPerReady, 1 << 20),
            (Limit::PendingAttempts, 256),
            (Limit::PendingAttemptBytes, 16 << 20),
            (Limit::WaitersPerRequest, 16),
            (Limit::UncommittedBytes, 4 << 20),
            (Limit::EntryCacheBytes, 8 << 20),
            (Limit::PeerQueueMessages, 64),
            (Limit::PeerQueueBytes, 8 << 20),
            (Limit::ReplicationBatchBytes, 256 << 10),
            (Limit::ReplicationInflightBatches, 16),
            (Limit::FramePayloadBytes, 1 << 20),
            (Limit::ReadChunkBytes, 256 << 10),
            (Limit::ControllerQueueInputs, 1024),
            (Limit::CliRetryAttempts, 10),
            (Limit::CliRetryNodes, 5),
            (Limit::CliDeadlineMs, 15_000),
            (Limit::LogQueueRecords, 4096),
            (Limit::SimEventsPerRun, 1_000_000),
        ];
        assert_eq!(
            expected.map(|(limit, _)| limit),
            Limit::ALL,
            "rule: the registry lists every §7.11 bound, in order (CD-9)"
        );
        for (limit, value) in expected {
            assert_eq!(
                local.limit(limit),
                value,
                "rule: config/local.toml resolves {} to its §7.11 value",
                limit.name()
            );
            assert!(
                limit.minimum() <= value && value <= limit.maximum(),
                "rule: a limit lies in its declared range; {}",
                limit.name()
            );
        }
    }

    #[test]
    fn every_configured_bound_is_in_the_registry_exactly_once() {
        let timing = [
            ConfigParam::ProtocolVersion,
            ConfigParam::TickMs,
            ConfigParam::HeartbeatTicks,
            ConfigParam::ElectionMinTicks,
            ConfigParam::ElectionMaxTicksExclusive,
            ConfigParam::WriteDeadlineTicks,
        ];
        for param in ConfigParam::ALL {
            let entries = Limit::ALL
                .into_iter()
                .filter(|limit| limit.source() == LimitSource::Config(param))
                .count();
            let expected = usize::from(!timing.contains(&param));
            assert_eq!(
                entries, expected,
                "rule: every configured bound has one registry entry; the protocol version \
                 and timing are not bounds (§7.11); param={param}"
            );
        }
        let mut names: Vec<&str> = Limit::ALL.into_iter().map(Limit::name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Limit::ALL.len(), "rule: one name per limit");
    }

    #[test]
    fn the_api_spec_states_the_registry_s_bounds() {
        // §7.10: the OpenAPI spec's limits are checked against this table.
        let bodies = spec_values("x-maxBodyBytes:");
        assert!(
            !bodies.is_empty(),
            "test setup: the spec bounds request bodies"
        );
        for body in bodies {
            assert_eq!(
                body,
                Limit::RequestBodyBytes.maximum(),
                "rule: 512 KiB bodies"
            );
        }
        assert_eq!(
            spec_values("x-maxUtf8Bytes:"),
            [
                u64::try_from(Key::MAX_BYTES).unwrap(),
                u64::try_from(Value::MAX_BYTES).unwrap()
            ],
            "rule: the spec's key and value bounds are DM-1/DM-2, the registry's maxima"
        );
        assert_eq!(
            [Limit::KeyBytes.maximum(), Limit::ValueBytes.maximum()],
            [256, 65_536],
            "rule: the configured key and value limits stop at the spec's bounds (D-80)"
        );
        assert_eq!(
            spec_values("x-maxPageUtf8Bytes:"),
            [Limit::ScanPageBytes.maximum()],
            "rule: one SCAN page byte cap, 1 MiB"
        );
        assert_eq!(
            spec_values("maximum:")
                .into_iter()
                .filter(|value| *value == Limit::ScanPageItems.maximum())
                .count(),
            1,
            "rule: the spec's SCAN limit maximum is the registry's 128"
        );
    }
}
