//! The numeric parameters' values and §7.10's rules across them: timing, then the
//! cross-limit rules. Each rule is its own function, so each has its own test even where
//! the ranges already make it unreachable from a file.

use super::error::ConfigError;
use super::params::{
    COMMAND_ENVELOPE_BOUND, ConfigParam, JOURNAL_PROTOCOL_ALLOWANCE, MAX_COMMAND_BYTES,
    PER_ENTRY_OVERHEAD_BOUND,
};

/// Every numeric parameter's value. In a validated config each lies in its range and
/// together they pass [`timing`] and [`limits`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Values {
    /// `protocol_version`.
    pub(super) protocol_version: u64,
    /// `tick_ms`.
    pub(super) tick_ms: u64,
    /// `heartbeat_ticks`.
    pub(super) heartbeat_ticks: u64,
    /// `election_min_ticks`.
    pub(super) election_min_ticks: u64,
    /// `election_max_ticks_exclusive`.
    pub(super) election_max_ticks_exclusive: u64,
    /// `write_deadline_ticks`.
    pub(super) write_deadline_ticks: u64,
    /// `cli_deadline_ms`.
    pub(super) cli_deadline_ms: u64,
    /// `max_key_bytes`.
    pub(super) max_key_bytes: u64,
    /// `max_value_bytes`.
    pub(super) max_value_bytes: u64,
    /// `max_live_keys`.
    pub(super) max_live_keys: u64,
    /// `max_live_bytes`.
    pub(super) max_live_bytes: u64,
    /// `max_log_entries`.
    pub(super) max_log_entries: u64,
    /// `max_history_bytes`.
    pub(super) max_history_bytes: u64,
    /// `max_journal_bytes`.
    pub(super) max_journal_bytes: u64,
    /// `replication_batch_bytes`.
    pub(super) replication_batch_bytes: u64,
    /// `replication_inflight_batches`.
    pub(super) replication_inflight_batches: u64,
    /// `peer_send_queue_messages`.
    pub(super) peer_send_queue_messages: u64,
    /// `peer_send_queue_bytes`.
    pub(super) peer_send_queue_bytes: u64,
    /// `max_uncommitted_bytes`.
    pub(super) max_uncommitted_bytes: u64,
    /// `max_committed_bytes_per_ready`.
    pub(super) max_committed_bytes_per_ready: u64,
    /// `raft_entry_overhead_bound`.
    pub(super) raft_entry_overhead_bound: u64,
    /// `peer_message_overhead_bound`.
    pub(super) peer_message_overhead_bound: u64,
}

impl Values {
    /// The value of `param`.
    pub(super) const fn get(&self, param: ConfigParam) -> u64 {
        match param {
            ConfigParam::ProtocolVersion => self.protocol_version,
            ConfigParam::TickMs => self.tick_ms,
            ConfigParam::HeartbeatTicks => self.heartbeat_ticks,
            ConfigParam::ElectionMinTicks => self.election_min_ticks,
            ConfigParam::ElectionMaxTicksExclusive => self.election_max_ticks_exclusive,
            ConfigParam::WriteDeadlineTicks => self.write_deadline_ticks,
            ConfigParam::CliDeadlineMs => self.cli_deadline_ms,
            ConfigParam::MaxKeyBytes => self.max_key_bytes,
            ConfigParam::MaxValueBytes => self.max_value_bytes,
            ConfigParam::MaxLiveKeys => self.max_live_keys,
            ConfigParam::MaxLiveBytes => self.max_live_bytes,
            ConfigParam::MaxLogEntries => self.max_log_entries,
            ConfigParam::MaxHistoryBytes => self.max_history_bytes,
            ConfigParam::MaxJournalBytes => self.max_journal_bytes,
            ConfigParam::ReplicationBatchBytes => self.replication_batch_bytes,
            ConfigParam::ReplicationInflightBatches => self.replication_inflight_batches,
            ConfigParam::PeerSendQueueMessages => self.peer_send_queue_messages,
            ConfigParam::PeerSendQueueBytes => self.peer_send_queue_bytes,
            ConfigParam::MaxUncommittedBytes => self.max_uncommitted_bytes,
            ConfigParam::MaxCommittedBytesPerReady => self.max_committed_bytes_per_ready,
            ConfigParam::RaftEntryOverheadBound => self.raft_entry_overhead_bound,
            ConfigParam::PeerMessageOverheadBound => self.peer_message_overhead_bound,
        }
    }

    /// The write deadline in milliseconds: `write_deadline_ticks × tick_ms`.
    pub(super) const fn write_deadline_ms(&self) -> u64 {
        self.write_deadline_ticks.saturating_mul(self.tick_ms)
    }
}

/// The timing rules of §7.10, in its order (a heartbeat below 1 tick is a range error).
///
/// # Errors
///
/// The first rule broken.
pub(super) const fn timing(values: &Values) -> Result<(), ConfigError> {
    let min = values.election_min_ticks;
    let max = values.election_max_ticks_exclusive;
    if min < values.heartbeat_ticks.saturating_mul(5) {
        Err(ConfigError::ElectionMinTooShort {
            election_min_ticks: min,
            heartbeat_ticks: values.heartbeat_ticks,
        })
    } else if max <= min {
        Err(ConfigError::ElectionMaxNotAboveMin {
            election_min_ticks: min,
            election_max_ticks_exclusive: max,
        })
    } else if max != min.saturating_mul(2) {
        Err(ConfigError::ElectionRangeShape {
            election_min_ticks: min,
            election_max_ticks_exclusive: max,
        })
    } else if values.write_deadline_ticks < max {
        Err(ConfigError::WriteDeadlineTooShort {
            write_deadline_ticks: values.write_deadline_ticks,
            election_max_ticks_exclusive: max,
        })
    } else if values.cli_deadline_ms < values.write_deadline_ms() {
        Err(ConfigError::CliDeadlineTooShort {
            cli_deadline_ms: values.cli_deadline_ms,
            write_deadline_ms: values.write_deadline_ms(),
        })
    } else {
        Ok(())
    }
}

/// The cross-limit rules of §7.10, in its order.
///
/// # Errors
///
/// The first rule broken.
pub(super) fn limits(values: &Values) -> Result<(), ConfigError> {
    command_fits(values)?;
    journal_margin(values)?;
    message_fits(values)?;
    queue_counts_fit(values)?;
    replication_fits(values)
}

/// The largest command (`max_key_bytes + max_value_bytes` + the envelope bound) fits the
/// canonical-command limit.
///
/// # Errors
///
/// [`ConfigError::CommandTooLarge`].
pub(super) const fn command_fits(values: &Values) -> Result<(), ConfigError> {
    let command_bytes = values
        .max_key_bytes
        .saturating_add(values.max_value_bytes)
        .saturating_add(COMMAND_ENVELOPE_BOUND);
    if command_bytes > MAX_COMMAND_BYTES {
        Err(ConfigError::CommandTooLarge {
            command_bytes,
            limit: MAX_COMMAND_BYTES,
        })
    } else {
        Ok(())
    }
}

/// The smallest `max_journal_bytes` the margin formula allows (§7.10, D-68):
/// ⌈1.25 × `max_history_bytes`⌉ + `max_log_entries` × [`PER_ENTRY_OVERHEAD_BOUND`] +
/// [`JOURNAL_PROTOCOL_ALLOWANCE`], computed exactly.
pub(super) fn required_journal_bytes(max_history_bytes: u64, max_log_entries: u64) -> u128 {
    (u128::from(max_history_bytes) * 5).div_ceil(4)
        + u128::from(max_log_entries) * u128::from(PER_ENTRY_OVERHEAD_BOUND)
        + u128::from(JOURNAL_PROTOCOL_ALLOWANCE)
}

/// `max_journal_bytes` satisfies the margin formula.
///
/// # Errors
///
/// [`ConfigError::JournalMarginTooSmall`].
pub(super) fn journal_margin(values: &Values) -> Result<(), ConfigError> {
    let required = required_journal_bytes(values.max_history_bytes, values.max_log_entries);
    if u128::from(values.max_journal_bytes) < required {
        Err(ConfigError::JournalMarginTooSmall {
            max_journal_bytes: values.max_journal_bytes,
            required: u64::try_from(required).unwrap_or(u64::MAX),
        })
    } else {
        Ok(())
    }
}

/// One replication message's byte bound (§7.10, D-81): ⌈3/2 × max(`replication_batch_bytes`,
/// the largest single entry)⌉ + `peer_message_overhead_bound`, computed exactly. The
/// largest entry is `max_key_bytes` + `max_value_bytes` + [`COMMAND_ENVELOPE_BOUND`] +
/// `raft_entry_overhead_bound`. The consensus library always sends at least one entry,
/// and the 3/2 covers per-entry framing.
pub(super) fn message_bound(values: &Values) -> u128 {
    let largest_entry = u128::from(values.max_key_bytes)
        + u128::from(values.max_value_bytes)
        + u128::from(COMMAND_ENVELOPE_BOUND)
        + u128::from(values.raft_entry_overhead_bound);
    let payload = largest_entry.max(u128::from(values.replication_batch_bytes));
    (payload * 3).div_ceil(2) + u128::from(values.peer_message_overhead_bound)
}

/// The per-peer send queue holds at least one replication message (§7.10, D-81).
///
/// # Errors
///
/// [`ConfigError::SendQueueBelowMessage`].
pub(super) fn message_fits(values: &Values) -> Result<(), ConfigError> {
    let bound = message_bound(values);
    if u128::from(values.peer_send_queue_bytes) < bound {
        Err(ConfigError::SendQueueBelowMessage {
            peer_send_queue_bytes: values.peer_send_queue_bytes,
            message_bound: u64::try_from(bound).unwrap_or(u64::MAX),
        })
    } else {
        Ok(())
    }
}

/// The per-peer send queue holds a message for every in-flight batch (§7.10, D-81).
///
/// # Errors
///
/// [`ConfigError::SendQueueMessagesBelowInflight`].
pub(super) const fn queue_counts_fit(values: &Values) -> Result<(), ConfigError> {
    if values.peer_send_queue_messages < values.replication_inflight_batches {
        Err(ConfigError::SendQueueMessagesBelowInflight {
            peer_send_queue_messages: values.peer_send_queue_messages,
            replication_inflight_batches: values.replication_inflight_batches,
        })
    } else {
        Ok(())
    }
}

/// In-flight replication messages fit the per-peer send queue's bytes, so ordinary
/// catch-up bursts are never dropped (§7.10, §7.11, D-81).
///
/// # Errors
///
/// [`ConfigError::ReplicationExceedsSendQueue`].
pub(super) fn replication_fits(values: &Values) -> Result<(), ConfigError> {
    let inflight_bytes = u128::from(values.replication_inflight_batches) * message_bound(values);
    if inflight_bytes > u128::from(values.peer_send_queue_bytes) {
        Err(ConfigError::ReplicationExceedsSendQueue {
            inflight_bytes: u64::try_from(inflight_bytes).unwrap_or(u64::MAX),
            peer_send_queue_bytes: values.peer_send_queue_bytes,
        })
    } else {
        Ok(())
    }
}
