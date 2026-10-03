//! What a write decided and where an attempt stands (PLAN.md §7.5, §7.8, CD-2).

use core::fmt;

use crate::error_code::ErrorCode;
use crate::log_index::{EntryIndex, LogIndex, Version};

/// Which live-state quota refused an executed write (DM-6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveStateLimit {
    /// The live key count (`max_live_keys`, default 10,000).
    Keys,
    /// The live key+value bytes (`max_live_bytes`, default 64 MiB).
    Bytes,
}

impl LiveStateLimit {
    /// The limit as the API names it (the configuration parameter, §7.10).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Keys => "max_live_keys",
            Self::Bytes => "max_live_bytes",
        }
    }
}

/// What an executed write decided (PLAN.md §7.5 apply step 4). Every decision is
/// committed and terminal: retries return it unchanged (§8.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// A PUT, or a compare-and-swap whose expectation matched, set the key. Its version
    /// is the execution index (DM-3).
    Set,
    /// A DELETE ran; `existed` says whether the key was present (F-3).
    Deleted {
        /// Whether the key existed when the entry was applied.
        existed: bool,
    },
    /// A compare-and-swap's expectation did not match; nothing changed (F-5).
    VersionMismatch {
        /// The key's version when the entry was applied, `None` if it was absent.
        current: Option<Version>,
    },
    /// The live-state quota refused the write; nothing changed (DM-6).
    CapacityExceeded {
        /// Which quota refused it.
        limit: LiveStateLimit,
    },
}

impl Decision {
    /// The error code the API answers this decision with, or `None` for a success.
    #[must_use]
    pub const fn error_code(self) -> Option<ErrorCode> {
        match self {
            Self::Set | Self::Deleted { .. } => None,
            Self::VersionMismatch { .. } => Some(ErrorCode::VersionMismatch),
            Self::CapacityExceeded { .. } => Some(ErrorCode::CapacityExceeded),
        }
    }
}

/// A write's terminal result: what the request history stores for its request ID,
/// forever (DM-5). Retries and status lookups return it unchanged (§8.1).
///
/// Invariant: `index` is the log index of the request's first execution, so it is at
/// least 1. A successful set's version is that index (DM-3), and is never stored twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalResult {
    index: LogIndex,
    decision: Decision,
}

impl TerminalResult {
    /// The result of a write first executed at `index`, or `None` for index 0, which
    /// holds no entry.
    #[must_use]
    pub fn new(index: LogIndex, decision: Decision) -> Option<Self> {
        (index > LogIndex::ZERO).then_some(Self { index, decision })
    }

    /// The log index of the first execution.
    #[must_use]
    pub const fn index(self) -> LogIndex {
        self.index
    }

    /// What the write decided.
    #[must_use]
    pub const fn decision(self) -> Decision {
        self.decision
    }

    /// The version a successful set gave its key (equal to [`index`](Self::index)), or
    /// `None` for any other decision.
    #[must_use]
    pub const fn version(self) -> Option<Version> {
        match self.decision {
            Decision::Set => Version::at(self.index),
            Decision::Deleted { .. }
            | Decision::VersionMismatch { .. }
            | Decision::CapacityExceeded { .. } => None,
        }
    }
}

/// How an attempt that reached a terminal state ended (PLAN.md §7.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// The entry carrying the request ID was applied, as a first execution or a replay,
    /// with this attempt's command.
    Executed(TerminalResult),
    /// The request ID was already used for a different command: nothing changed
    /// (`request_id_conflict`, F-6).
    Conflict {
        /// The log index at which the ID was first executed: an entry, so at least 1
        /// (the spec's `EntryIndex`).
        original_index: EntryIndex,
    },
}

/// Where one write attempt (one HTTP request) stands (PLAN.md §7.8).
///
/// Invariant: an attempt is exactly one of these, never independent flags. An unknown
/// outcome is not a state: it is a response about a `Submitted` attempt (CD-2, A-5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attempt {
    /// Parsed and admitted, but no proposal was handed to consensus for it.
    NotSubmitted,
    /// Marked before the command was handed to consensus, or attached to a pending
    /// same-ID, same-command proposal.
    Submitted,
    /// The request ID's entry was applied.
    Terminal(AttemptOutcome),
}

impl Attempt {
    /// The API's `attempt_state` for a non-terminal answer about this attempt, or `None`
    /// once it is terminal (§8.1 non-terminal refusals).
    #[must_use]
    pub const fn api_state(self) -> Option<&'static str> {
        match self {
            Self::NotSubmitted => Some("not_submitted"),
            Self::Submitted => Some("submitted"),
            Self::Terminal(_) => None,
        }
    }
}

impl fmt::Display for LiveStateLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(raw: u64) -> LogIndex {
        LogIndex::new(raw).unwrap()
    }

    #[test]
    fn a_terminal_result_needs_a_real_entry_and_derives_the_version() {
        assert_eq!(
            TerminalResult::new(LogIndex::ZERO, Decision::Set),
            None,
            "rule: index 0 holds no entry, so nothing executed there"
        );
        let set = TerminalResult::new(index(42), Decision::Set).unwrap();
        assert_eq!(
            set.index(),
            index(42),
            "rule: the first execution's index is kept (§8.3)"
        );
        assert_eq!(
            set.version(),
            Version::new(42).ok(),
            "rule: version == index for a set (DM-3)"
        );
        let decisions = [
            Decision::Deleted { existed: true },
            Decision::VersionMismatch {
                current: Version::new(57).ok(),
            },
            Decision::CapacityExceeded {
                limit: LiveStateLimit::Keys,
            },
        ];
        for decision in decisions {
            let result = TerminalResult::new(index(58), decision).unwrap();
            assert_eq!(
                result.version(),
                None,
                "rule: only a set assigns a version (DM-3); decision={decision:?}"
            );
            assert_eq!(
                result.decision(),
                decision,
                "rule: the decision is stored unchanged"
            );
        }
    }

    #[test]
    fn decisions_map_to_the_terminal_error_codes() {
        let cases = [
            (Decision::Set, None),
            (Decision::Deleted { existed: false }, None),
            (
                Decision::VersionMismatch { current: None },
                Some(ErrorCode::VersionMismatch),
            ),
            (
                Decision::CapacityExceeded {
                    limit: LiveStateLimit::Bytes,
                },
                Some(ErrorCode::CapacityExceeded),
            ),
        ];
        for (decision, code) in cases {
            assert_eq!(
                decision.error_code(),
                code,
                "rule: §8.4 maps terminal decisions; decision={decision:?}"
            );
        }
        assert_eq!(
            [
                LiveStateLimit::Keys.as_str(),
                LiveStateLimit::Bytes.as_str()
            ],
            ["max_live_keys", "max_live_bytes"],
            "rule: the API's LiveStateLimit enum names the config parameters"
        );
    }

    #[test]
    fn attempt_states_are_exclusive_and_spelt_as_the_api() {
        let executed = Attempt::Terminal(AttemptOutcome::Executed(
            TerminalResult::new(index(9), Decision::Set).unwrap(),
        ));
        let conflict = Attempt::Terminal(AttemptOutcome::Conflict {
            original_index: EntryIndex::new(3).unwrap(),
        });
        let cases = [
            (Attempt::NotSubmitted, Some("not_submitted")),
            (Attempt::Submitted, Some("submitted")),
            (executed, None),
            (conflict, None),
        ];
        for (attempt, state) in cases {
            assert_eq!(
                attempt.api_state(),
                state,
                "rule: §8.1 attempt_state; terminal attempts have none; attempt={attempt:?}"
            );
        }
    }
}
