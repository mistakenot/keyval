//! Why a config file is refused: one specific error per rule (PLAN.md §7.10, O-5).

use core::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;

use super::consensus::ConsensusViolation;
use super::params::{
    COMMAND_ENVELOPE_BOUND, ConfigParam, JOURNAL_PROTOCOL_ALLOWANCE, MAX_COMMAND_BYTES,
    MAX_CONFIG_BYTES, PER_ENTRY_OVERHEAD_BOUND,
};
use crate::{ClusterIdError, NodeId, OutOfRange};

/// A table of the config file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigTable {
    /// The top-level table.
    Top,
    /// The `[[nodes]]` table whose header is on this line.
    Node {
        /// The header's line number, counted from 1.
        line: usize,
    },
}

impl fmt::Display for ConfigTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Top => f.write_str("the top-level table"),
            Self::Node { line } => write!(f, "the [[nodes]] table at line {line}"),
        }
    }
}

/// Why the strict reader cannot read a line (D-79).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigSyntax {
    /// The line is not blank, a comment, `[[nodes]]` or `key = value`.
    ExpectedKeyValue,
    /// A quoted or dotted key: only bare keys (letters, digits, `_`, `-`) are supported.
    UnsupportedKey,
    /// `key =` with no value.
    MissingValue,
    /// A string with no closing quote on its line.
    UnterminatedString,
    /// A backslash in a string: escape sequences are not supported.
    EscapeSequence,
    /// A control character other than tab in a string or comment.
    ControlCharacter,
    /// A value other than a decimal integer or a basic string: a sign, underscores, a
    /// float, a date, a boolean, an array, an inline table, a literal or multi-line string.
    UnsupportedValue,
    /// An integer with a leading zero, which TOML forbids.
    LeadingZero,
    /// An integer above 2^63 − 1, TOML's largest.
    IntegerTooLarge,
    /// Something other than a comment after a value or header.
    TrailingCharacters,
    /// A table header other than `[[nodes]]`.
    UnsupportedTable,
}

impl fmt::Display for ConfigSyntax {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ExpectedKeyValue => "expected `key = value`, a `#` comment or `[[nodes]]`",
            Self::UnsupportedKey => {
                "unsupported key: only bare keys of letters, digits, `_` and `-` (no quoted or \
                 dotted keys)"
            }
            Self::MissingValue => "missing value after `=`",
            Self::UnterminatedString => {
                "unterminated string: the closing `\"` must be on the same line"
            }
            Self::EscapeSequence => "escape sequences are not supported in strings",
            Self::ControlCharacter => "control character (other than tab) in a string or comment",
            Self::UnsupportedValue => {
                "unsupported value: only decimal integers without sign, underscores or exponent, \
                 and \"basic strings\""
            }
            Self::LeadingZero => "integers may not have leading zeros",
            Self::IntegerTooLarge => "integer too large",
            Self::TrailingCharacters => {
                "unexpected characters after the value (only a `#` comment may follow)"
            }
            Self::UnsupportedTable => "unsupported table header: the only table is `[[nodes]]`",
        })
    }
}

/// The kind of value a key takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigValueKind {
    /// A decimal integer.
    Integer,
    /// A basic string.
    String,
}

impl fmt::Display for ConfigValueKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Integer => "an integer",
            Self::String => "a string",
        })
    }
}

/// Why an address is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressProblem {
    /// Not an IP address and port such as `127.0.0.1:7001` or `[::1]:7001`. Host names are
    /// refused: resolving them would need DNS.
    NotIpAndPort,
    /// Port 0, which binds an arbitrary port that peers and clients cannot know.
    PortZero,
}

impl fmt::Display for AddressProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotIpAndPort => "expected an IP address and port, such as \"127.0.0.1:7001\"",
            Self::PortZero => "port 0 is not a fixed port",
        })
    }
}

/// Why a config file is refused (§7.10). Every rule has its own variant, and every
/// variant names the values or lines involved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// The file is larger than [`MAX_CONFIG_BYTES`] (CD-9).
    FileTooLarge {
        /// The file's length in bytes.
        bytes: u64,
    },
    /// The strict reader cannot read a line (D-79).
    Syntax {
        /// The line number, counted from 1.
        line: usize,
        /// What is wrong with it.
        problem: ConfigSyntax,
    },
    /// A key defined twice in one table.
    DuplicateKey {
        /// The second definition's line.
        line: usize,
        /// The key.
        key: String,
        /// The first definition's line.
        first_line: usize,
    },
    /// A key the config does not have, such as a misspelling: it is never ignored (O-5).
    UnknownKey {
        /// The key's line.
        line: usize,
        /// The key.
        key: String,
        /// The table it is in.
        table: ConfigTable,
    },
    /// A required key is absent, including the cluster ID and protocol version.
    MissingKey {
        /// The key.
        key: &'static str,
        /// The table it is missing from.
        table: ConfigTable,
    },
    /// A key holds the wrong kind of value.
    WrongType {
        /// The key's line.
        line: usize,
        /// The key.
        key: &'static str,
        /// The kind it takes.
        expected: ConfigValueKind,
    },
    /// A numeric parameter outside its allowed range (§7.11), including a heartbeat
    /// interval below 1 tick.
    OutOfRange {
        /// The key's line.
        line: usize,
        /// The parameter.
        param: ConfigParam,
        /// The rejected value.
        value: u64,
    },
    /// An invalid cluster ID.
    ClusterId {
        /// The key's line.
        line: usize,
        /// Why it is invalid.
        error: ClusterIdError,
    },
    /// A node ID that is zero, or 2^53 or above (D-77).
    NodeId {
        /// The key's line.
        line: usize,
        /// The range error.
        error: OutOfRange,
    },
    /// An address that is not a fixed IP address and port.
    Address {
        /// The key's line.
        line: usize,
        /// `client_address` or `peer_address`.
        key: &'static str,
        /// What is wrong with it.
        problem: AddressProblem,
    },
    /// An empty data directory.
    EmptyDataDir {
        /// The key's line.
        line: usize,
    },
    /// A voter count other than 1, 3 or 5 (D-45).
    VoterCount {
        /// The number of `[[nodes]]` tables.
        count: usize,
    },
    /// Two nodes with one ID.
    DuplicateNodeId {
        /// The ID.
        id: NodeId,
        /// The second node's `id` line.
        line: usize,
        /// The first node's `id` line.
        first_line: usize,
    },
    /// One address used twice, as client or peer address of any nodes.
    DuplicateAddress {
        /// The address.
        address: SocketAddr,
        /// The second use's line.
        line: usize,
        /// The first use's line.
        first_line: usize,
    },
    /// Two nodes with one data directory, compared after resolving and normalising.
    DuplicateDataDir {
        /// The resolved directory.
        path: PathBuf,
        /// The second node's `data_dir` line.
        line: usize,
        /// The first node's `data_dir` line.
        first_line: usize,
    },
    /// The election timeout minimum is below 5 × the heartbeat interval.
    ElectionMinTooShort {
        /// `election_min_ticks`.
        election_min_ticks: u64,
        /// `heartbeat_ticks`.
        heartbeat_ticks: u64,
    },
    /// The election timeout maximum is not above its minimum.
    ElectionMaxNotAboveMin {
        /// `election_min_ticks`.
        election_min_ticks: u64,
        /// `election_max_ticks_exclusive`.
        election_max_ticks_exclusive: u64,
    },
    /// The election range is not `[t, 2t)`: raft-rs 0.7 fixes its shape (D-75).
    ElectionRangeShape {
        /// `election_min_ticks`.
        election_min_ticks: u64,
        /// `election_max_ticks_exclusive`.
        election_max_ticks_exclusive: u64,
    },
    /// The write deadline is shorter than the election timeout maximum, so a write could
    /// not outlive one election.
    WriteDeadlineTooShort {
        /// `write_deadline_ticks`.
        write_deadline_ticks: u64,
        /// `election_max_ticks_exclusive`.
        election_max_ticks_exclusive: u64,
    },
    /// The CLI deadline is shorter than the write deadline (`write_deadline_ticks ×
    /// tick_ms`), compared in milliseconds.
    CliDeadlineTooShort {
        /// `cli_deadline_ms`.
        cli_deadline_ms: u64,
        /// The write deadline in milliseconds.
        write_deadline_ms: u64,
    },
    /// The largest command (`max_key_bytes` + `max_value_bytes` + the envelope bound) does
    /// not fit the canonical-command limit.
    CommandTooLarge {
        /// The largest command's size in bytes.
        command_bytes: u64,
        /// The canonical-command limit.
        limit: u64,
    },
    /// `max_journal_bytes` is below the margin formula's requirement (§7.10, D-68).
    JournalMarginTooSmall {
        /// `max_journal_bytes`.
        max_journal_bytes: u64,
        /// The smallest value the formula allows.
        required: u64,
    },
    /// In-flight replication bytes exceed the per-peer send queue's bytes (§7.11).
    ReplicationExceedsSendQueue {
        /// `replication_inflight_batches × replication_batch_bytes`.
        inflight_bytes: u64,
        /// `peer_send_queue_bytes`.
        peer_send_queue_bytes: u64,
    },
    /// The consensus library would refuse this node's settings (raft-rs 0.7
    /// `Config::validate`, D-78).
    Consensus {
        /// The node.
        node: NodeId,
        /// The constraint it breaks.
        violation: ConsensusViolation,
    },
}

impl fmt::Display for ConfigError {
    #[allow(
        clippy::too_many_lines,
        reason = "one message per §7.10 rule reads best as one exhaustive match; br-w08-1-config-parser-rules-gwj"
    )]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileTooLarge { bytes } => write!(
                f,
                "the config file is {bytes} bytes; the limit is {MAX_CONFIG_BYTES} bytes"
            ),
            Self::Syntax { line, problem } => write!(f, "line {line}: {problem}"),
            Self::DuplicateKey {
                line,
                key,
                first_line,
            } => write!(
                f,
                "line {line}: `{key}` is already defined at line {first_line}"
            ),
            Self::UnknownKey { line, key, table } => {
                write!(f, "line {line}: unknown key `{key}` in {table}")
            }
            Self::MissingKey { key, table } => write!(f, "missing required key `{key}` in {table}"),
            Self::WrongType {
                line,
                key,
                expected,
            } => write!(f, "line {line}: `{key}` must be {expected}"),
            Self::OutOfRange { line, param, value } => write!(
                f,
                "line {line}: `{param}` = {value} is outside its allowed range {}..={}",
                param.minimum(),
                param.maximum()
            ),
            Self::ClusterId { line, error } => write!(f, "line {line}: `cluster_id`: {error}"),
            Self::NodeId { line, error } => write!(f, "line {line}: `id`: {error}"),
            Self::Address { line, key, problem } => write!(f, "line {line}: `{key}`: {problem}"),
            Self::EmptyDataDir { line } => write!(f, "line {line}: `data_dir` is empty"),
            Self::VoterCount { count } => write!(
                f,
                "{count} [[nodes]] tables: a cluster has 1, 3 or 5 voters (D-45)"
            ),
            Self::DuplicateNodeId {
                id,
                line,
                first_line,
            } => write!(
                f,
                "line {line}: node ID {id} is already used at line {first_line}"
            ),
            Self::DuplicateAddress {
                address,
                line,
                first_line,
            } => write!(
                f,
                "line {line}: address {address} is already used at line {first_line}"
            ),
            Self::DuplicateDataDir {
                path,
                line,
                first_line,
            } => write!(
                f,
                "line {line}: data directory {} is already used at line {first_line}",
                path.display()
            ),
            Self::ElectionMinTooShort {
                election_min_ticks,
                heartbeat_ticks,
            } => write!(
                f,
                "election_min_ticks = {election_min_ticks} is below 5 × heartbeat_ticks = {}",
                heartbeat_ticks.saturating_mul(5)
            ),
            Self::ElectionMaxNotAboveMin {
                election_min_ticks,
                election_max_ticks_exclusive,
            } => write!(
                f,
                "election_max_ticks_exclusive = {election_max_ticks_exclusive} is not above \
                 election_min_ticks = {election_min_ticks}"
            ),
            Self::ElectionRangeShape {
                election_min_ticks,
                election_max_ticks_exclusive,
            } => write!(
                f,
                "election_max_ticks_exclusive = {election_max_ticks_exclusive} must be exactly \
                 2 × election_min_ticks = {}: raft-rs fixes the range to [t, 2t) (D-75)",
                election_min_ticks.saturating_mul(2)
            ),
            Self::WriteDeadlineTooShort {
                write_deadline_ticks,
                election_max_ticks_exclusive,
            } => write!(
                f,
                "write_deadline_ticks = {write_deadline_ticks} is below \
                 election_max_ticks_exclusive = {election_max_ticks_exclusive}: a write must \
                 outlive one election"
            ),
            Self::CliDeadlineTooShort {
                cli_deadline_ms,
                write_deadline_ms,
            } => write!(
                f,
                "cli_deadline_ms = {cli_deadline_ms} is below the write deadline \
                 (write_deadline_ticks × tick_ms = {write_deadline_ms} ms)"
            ),
            Self::CommandTooLarge {
                command_bytes,
                limit,
            } => write!(
                f,
                "max_key_bytes + max_value_bytes + {COMMAND_ENVELOPE_BOUND} B of envelope = \
                 {command_bytes} B exceeds the canonical-command limit of {limit} B \
                 (MAX_COMMAND_BYTES = {MAX_COMMAND_BYTES})"
            ),
            Self::JournalMarginTooSmall {
                max_journal_bytes,
                required,
            } => write!(
                f,
                "max_journal_bytes = {max_journal_bytes} is below 1.25 × max_history_bytes + \
                 max_log_entries × {PER_ENTRY_OVERHEAD_BOUND} + {JOURNAL_PROTOCOL_ALLOWANCE} \
                 = {required} (§7.10)"
            ),
            Self::ReplicationExceedsSendQueue {
                inflight_bytes,
                peer_send_queue_bytes,
            } => write!(
                f,
                "replication_inflight_batches × replication_batch_bytes = {inflight_bytes} \
                 exceeds peer_send_queue_bytes = {peer_send_queue_bytes}"
            ),
            Self::Consensus { node, violation } => write!(
                f,
                "node {node}: the consensus library would refuse its settings: {violation}"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}
