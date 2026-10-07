//! What a checker returns: a report of violations, or an error for a document it cannot read.

use core::fmt;
use std::error::Error;

/// What a violation breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// K-1: a success was acknowledged for a request that never executed in the committed log.
    AcknowledgedNeverCommitted,
    /// K-1: the acknowledged index is not the first execution's index.
    AcknowledgedAtWrongIndex,
    /// K-1: the acknowledged version or `existed` is not what the first execution decided.
    AcknowledgedWithWrongEffect,
    /// K-1: a recoverable node's final log lacks the acknowledged entry.
    MissingOnNode,
    /// K-1: a recoverable node's final log holds other bytes at the acknowledged index.
    DivergentOnNode,
    /// K-2: a terminal execution response for a request that never executed.
    ExecutedWithoutCommit,
    /// K-2: a terminal execution response contradicts the first execution in the log.
    ResponseContradictsLog,
    /// K-2: two responses claim a first execution (`replayed: false`) at different indices.
    TwoFirstExecutions,
    /// K-2: two terminal responses for one request ID disagree on index or decision.
    InconsistentResponses,
    /// K-2: a misuse attempt (a conflicting reuse) was answered as an execution.
    MisuseAnsweredAsExecution,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AcknowledgedNeverCommitted => "acknowledged_never_committed",
            Self::AcknowledgedAtWrongIndex => "acknowledged_at_wrong_index",
            Self::AcknowledgedWithWrongEffect => "acknowledged_with_wrong_effect",
            Self::MissingOnNode => "missing_on_node",
            Self::DivergentOnNode => "divergent_on_node",
            Self::ExecutedWithoutCommit => "executed_without_commit",
            Self::ResponseContradictsLog => "response_contradicts_log",
            Self::TwoFirstExecutions => "two_first_executions",
            Self::InconsistentResponses => "inconsistent_responses",
            Self::MisuseAnsweredAsExecution => "misuse_answered_as_execution",
        })
    }
}

/// One broken rule, with the evidence to find it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The rule broken.
    pub kind: Kind,
    /// The request ID concerned, as 32 hex characters.
    pub request_id: String,
    /// The attempt whose response shows it, if one does.
    pub attempt: Option<u64>,
    /// What was expected and what was observed.
    pub detail: String,
}

/// A checker's verdict on one history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// The checker's name, such as `K-1`.
    pub checker: &'static str,
    /// How many cases it examined: the unit is the checker's own (see its docs).
    pub examined: u64,
    /// Every violation found, in a fixed order.
    pub violations: Vec<Violation>,
}

impl Report {
    /// Whether the history passes.
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.violations.is_empty()
    }

    /// Whether some violation is of `kind`.
    #[must_use]
    pub fn has(&self, kind: Kind) -> bool {
        self.violations.iter().any(|v| v.kind == kind)
    }
}

/// Why a document cannot be judged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckError {
    /// The text is not JSON.
    NotJson(String),
    /// A field the checkers need is absent or not what the schema says.
    Malformed {
        /// Where, as a path of record index and field names.
        at: String,
        /// What is wrong.
        reason: String,
    },
    /// The committed commands cannot be replayed through the model.
    Replay {
        /// The index that failed.
        index: u64,
        /// The model's reason.
        reason: String,
    },
}

impl CheckError {
    pub(crate) fn malformed(at: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Malformed {
            at: at.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotJson(reason) => write!(f, "not JSON: {reason}"),
            Self::Malformed { at, reason } => write!(f, "malformed at {at}: {reason}"),
            Self::Replay { index, reason } => {
                write!(f, "committed entry {index} cannot be replayed: {reason}")
            }
        }
    }
}

impl Error for CheckError {}
