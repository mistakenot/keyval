//! [`ErrorCode`]: every error the HTTP API can answer with (PLAN.md §8.4), one variant
//! per code (CD-6). Registry condition 5 (§10.13) diffs this list against the spec.

use core::fmt;

/// An error code of the public API (PLAN.md §8.4).
///
/// Invariant: the variants are exactly the `ErrorCode` enum of `api/openapi.yaml`, in
/// the same order (a test diffs them). A new code is added to the spec first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ErrorCode {
    /// `307`: a write reached a non-leader that believes it knows the leader.
    NotLeader,
    /// `400`: malformed input, a bad key, request ID, range or pagination parameter.
    InvalidRequest,
    /// `404`: GET of an absent key.
    NotFound,
    /// `409`: the compare-and-swap expectation did not match when applied. Terminal.
    VersionMismatch,
    /// `409`: the request ID was already used for a different command. Terminal for the ID.
    RequestIdConflict,
    /// `413`: the value is over 64 KiB (DM-2) or the body over 512 KiB.
    TooLarge,
    /// `429`: a local admission bound was hit, or the proposal was dropped synchronously.
    Busy,
    /// `503`: no leader is known, or the node is shutting down.
    Unavailable,
    /// `503`: the node is not ready (starting, recovering or failed).
    Recovering,
    /// `504`: submitted, but the commit was not observed; the outcome is unresolved.
    UnknownOutcome,
    /// `507`: the live-state quota (DM-6) refused the executed write. Terminal.
    CapacityExceeded,
    /// `507`: the history capacity (DM-7) refused admission before proposal.
    HistoryFull,
}

/// Whether an outcome settles a request for good (PLAN.md §8.4 "Terminal?").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Terminality {
    /// A committed decision: the request ID is consumed, retries return the same result.
    Terminal,
    /// Terminal for this request ID, but not an execution of this command.
    TerminalForId,
    /// Not decided by this answer: this attempt was refused or its outcome is unknown.
    NotTerminal,
    /// Unresolved: the attempt was submitted and may or may not commit.
    Unresolved,
    /// Not about a write at all (a read's `not_found`).
    NotApplicable,
}

impl ErrorCode {
    /// Every code, in the spec's order.
    pub const ALL: [Self; 12] = [
        Self::NotLeader,
        Self::InvalidRequest,
        Self::NotFound,
        Self::VersionMismatch,
        Self::RequestIdConflict,
        Self::TooLarge,
        Self::Busy,
        Self::Unavailable,
        Self::Recovering,
        Self::UnknownOutcome,
        Self::CapacityExceeded,
        Self::HistoryFull,
    ];

    /// The code as the API spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotLeader => "not_leader",
            Self::InvalidRequest => "invalid_request",
            Self::NotFound => "not_found",
            Self::VersionMismatch => "version_mismatch",
            Self::RequestIdConflict => "request_id_conflict",
            Self::TooLarge => "too_large",
            Self::Busy => "busy",
            Self::Unavailable => "unavailable",
            Self::Recovering => "recovering",
            Self::UnknownOutcome => "unknown_outcome",
            Self::CapacityExceeded => "capacity_exceeded",
            Self::HistoryFull => "history_full",
        }
    }

    /// The HTTP status the code is answered with (PLAN.md §8.4).
    #[must_use]
    pub const fn http_status(self) -> u16 {
        match self {
            Self::NotLeader => 307,
            Self::InvalidRequest => 400,
            Self::NotFound => 404,
            Self::VersionMismatch | Self::RequestIdConflict => 409,
            Self::TooLarge => 413,
            Self::Busy => 429,
            Self::Unavailable | Self::Recovering => 503,
            Self::UnknownOutcome => 504,
            Self::CapacityExceeded | Self::HistoryFull => 507,
        }
    }

    /// Whether the code settles the request (PLAN.md §8.4, F-10).
    #[must_use]
    pub const fn terminality(self) -> Terminality {
        match self {
            Self::VersionMismatch | Self::CapacityExceeded => Terminality::Terminal,
            Self::RequestIdConflict => Terminality::TerminalForId,
            Self::UnknownOutcome => Terminality::Unresolved,
            Self::NotFound => Terminality::NotApplicable,
            Self::NotLeader
            | Self::InvalidRequest
            | Self::TooLarge
            | Self::Busy
            | Self::Unavailable
            | Self::Recovering
            | Self::HistoryFull => Terminality::NotTerminal,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    // check: ERROR-CODES

    use super::*;

    /// The spec, read at compile time: no runtime I/O in a deterministic crate.
    const SPEC: &str = include_str!("../../../api/openapi.yaml");

    /// The `ErrorCode` enum members of the spec, in order.
    fn spec_codes() -> Vec<&'static str> {
        let mut codes = Vec::new();
        let mut in_enum = false;
        for line in SPEC
            .lines()
            .skip_while(|line| *line != "    ErrorCode:")
            .skip(1)
        {
            if line == "      enum:" {
                in_enum = true;
            } else if in_enum {
                match line.strip_prefix("        - ") {
                    Some(code) => codes.push(code.trim()),
                    None => break,
                }
            } else if !line.starts_with("      ") {
                break;
            }
        }
        codes
    }

    #[test]
    fn variants_match_the_spec_error_codes_exactly() {
        let ours: Vec<&str> = ErrorCode::ALL.iter().map(|code| code.as_str()).collect();
        let spec = spec_codes();
        assert_eq!(
            spec.len(),
            12,
            "fixture: api/openapi.yaml lists twelve codes; parsed={spec:?}"
        );
        assert_eq!(
            ours, spec,
            "rule: the domain error codes equal the spec's, in order (§10.13 condition 5)"
        );
    }

    #[test]
    fn statuses_and_terminality_follow_the_outcome_table() {
        // PLAN.md §8.4, transcribed independently of the implementation.
        let table: [(&str, u16, Terminality); 12] = [
            ("not_leader", 307, Terminality::NotTerminal),
            ("invalid_request", 400, Terminality::NotTerminal),
            ("not_found", 404, Terminality::NotApplicable),
            ("version_mismatch", 409, Terminality::Terminal),
            ("request_id_conflict", 409, Terminality::TerminalForId),
            ("too_large", 413, Terminality::NotTerminal),
            ("busy", 429, Terminality::NotTerminal),
            ("unavailable", 503, Terminality::NotTerminal),
            ("recovering", 503, Terminality::NotTerminal),
            ("unknown_outcome", 504, Terminality::Unresolved),
            ("capacity_exceeded", 507, Terminality::Terminal),
            ("history_full", 507, Terminality::NotTerminal),
        ];
        for (code, (name, status, terminality)) in ErrorCode::ALL.iter().zip(table) {
            assert_eq!(
                code.as_str(),
                name,
                "rule: ALL is in the §8.4 order; code={code:?}"
            );
            assert_eq!(code.http_status(), status, "rule: §8.4 status; code={name}");
            assert_eq!(
                code.terminality(),
                terminality,
                "rule: §8.4 Terminal? column; code={name}"
            );
        }
    }

    #[test]
    fn capacity_exceeded_and_history_full_stay_distinct() {
        assert_ne!(
            ErrorCode::CapacityExceeded.terminality(),
            ErrorCode::HistoryFull.terminality(),
            "rule: one is a committed decision, the other a refusal before proposal (D-54)"
        );
        assert_eq!(
            ErrorCode::HistoryFull.to_string(),
            "history_full",
            "rule: Display is the API spelling"
        );
    }
}
