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
    /// K-3: a logical write was invoked after another's first terminal decision was observed,
    /// yet executed at a smaller or equal index.
    RealTimeOrderViolated,
    /// K-4: a reported write result has no first execution in the committed log.
    ReportedWithoutCommand,
    /// K-4: a reported write result gives an index other than the model's first execution.
    ReportedIndexWrong,
    /// K-4: a reported `version` is not the model's (DM-3).
    ReportedVersionWrong,
    /// K-4: a reported `existed` is not the model's (F-3).
    ReportedExistedWrong,
    /// K-4: a CAS reported success where the model mismatched, or the reverse (F-5).
    ReportedCasOutcomeWrong,
    /// K-4: a reported mismatch names a current version other than the model's (F-5).
    ReportedCurrentVersionWrong,
    /// K-4: a reported capacity outcome differs from the model's (DM-6).
    ReportedCapacityWrong,
    /// K-4: a reported result is of another kind than the model's (a PUT's set reported as a
    /// delete, say).
    ReportedDecisionWrong,
    /// K-4: a `request_id_conflict` names an original index other than the first execution's
    /// (F-6).
    ConflictWrongOriginal,
    /// K-4: a `request_id_conflict` for a request that never executed (F-6).
    ConflictWithoutOriginal,
    /// K-4: a `request_id_conflict` for an attempt that is not a conflicting reuse (F-6).
    ConflictWithoutMisuse,
    /// K-5: a read names a serving node other than the node that answered.
    ReadServedByWrongNode,
    /// K-5: a read reports a prefix its node had not applied before answering (B-05, B-06).
    ReadPrefixNotApplied,
    /// K-5: a read reports a prefix that was not committed before it answered (B-05).
    ReadPrefixNotCommitted,
    /// K-5: a GET differs from the model at the reported prefix (C-3).
    ReadValueWrong,
    /// K-5: a SCAN page differs from the page the model's prefix gives, in items, order,
    /// versions, caps, `has_more` or `next_after` (B-07, F-4).
    ScanPageWrong,
    /// K-5: a status answer differs from the model at the reported prefix (F-9).
    StatusAnswerWrong,
    /// K-6: a node incarnation reported a smaller `applied_index` than it had before (C-4).
    ReadRegressed,
    /// K-8 election safety: two different nodes led one term.
    TwoLeadersInTerm,
    /// K-8 election safety: a node held votes for two different candidates in one term.
    DoubleVote,
    /// K-8 log matching: two logs hold different payload bytes under one index and term.
    SameTermDifferentBytes,
    /// K-8 log matching: two logs agree on an index and term but differ earlier.
    PrefixDiffersUnderSameTerm,
    /// K-8 leader append-only: a node removed log entries while it led.
    LeaderTruncated,
    /// K-8 leader completeness: a leader's log lacks an entry committed in an earlier term.
    LeaderMissingCommitted,
    /// K-8 state-machine safety: a node commits or applies past the certified prefix.
    CommitBeyondCertificate,
    /// K-8 state-machine safety: a node's log disagrees with a certified entry within its commit index.
    NodeDivergesFromCertified,
    /// K-8 state-machine safety: two nodes applied one index and hold different state digests.
    StateDigestDiverges,
    /// K-8: two different entries were certified at one index.
    ConflictingCertificates,
    /// K-8: a `committed` record names an entry no commit certificate supports.
    CommittedWithoutCertificate,
    /// K-11: a vote was granted before its term and vote were durable (B-02).
    VoteBeforeSync,
    /// K-11: an append was acknowledged before its term, vote and entries were durable (B-03).
    AckBeforeSync,
    /// K-11: a client success was released before the entries it rests on were durable.
    SuccessBeforeSync,
    /// K-11: a read was published before the log it was served from was durable.
    ReadBeforeSync,
    /// K-11: a replica accepted an entry before it was durable.
    AcceptBeforeSync,
    /// K-12: a response after submission says `not_submitted`, or a never-submitted attempt
    /// says `submitted` (B-15).
    WrongAttemptState,
    /// K-12: a response after submission reports a failure it cannot know (B-15).
    GuessedFailureAfterSubmission,
    /// K-12: an `unknown_outcome` for an attempt that was never submitted.
    UnknownOutcomeWithoutSubmission,
    /// K-12: an attempt was answered more than once.
    SecondResponse,
    /// K-12: a `status` answer whose state and result disagree.
    StatusContradictsItself,
    /// K-12: a write resolved as absent that has a submitted attempt or a stored result.
    AbsenceContradicted,
    /// K-12: a write resolved as not executed without `unknown` evidence on every lookup.
    AbsenceWithoutEvidence,
    /// K-12: a logical write has no resolution.
    WriteNotResolved,
    /// K-12: a logical write has more than one resolution.
    WriteResolvedTwice,
    /// K-10: the run is incomplete (an unresolved or unresolvable write, or a run that
    /// overflowed, exhausted its budget or broke its scenario envelope): never a pass.
    HistoryIncomplete,
    /// K-10: two logical writes report the same index.
    IndexCollision,
    /// K-10: a reported index no log could hold (beyond the configured log capacity).
    IndexOutOfRange,
    /// K-10: one logical write's observations disagree with each other.
    ObservationsDisagree,
    /// K-10: a write's resolution contradicts what clients observed of it.
    ResolutionContradicted,
    /// K-10: no placement of the writes within their intervals follows the index order.
    NoLinearization,
    /// K-7: a recoverable node never applied the catch-up marker.
    MarkerMissing,
    /// K-7: recoverable nodes applied markers at different indices.
    MarkerIndexDiffers,
    /// K-7: recoverable nodes hold different state digests where they must agree.
    DigestDiffers,
    /// K-7: a drain heal recorded no certified drain.
    DrainMissing,
    /// K-7: the drain is not settled (last = commit = applied, one index, the quiet period) or
    /// leaves out a recoverable node.
    DrainNotSettled,
    /// K-9: a logical write has no resolution or is `unresolved`.
    WriteUnresolved,
    /// K-9: the heal never completed (a node never applied the marker, the marker never
    /// committed, or no drain was certified).
    HealIncomplete,
    /// K-9: the heal completed after its budget.
    HealLate,
    /// K-9: client activity did not stop: no `quiescence`, or a non-status invocation after it.
    NotQuiescent,
    /// K-9: a `not_executed` resolution is not backed by lookups from every recoverable node
    /// after the drain (or quiescence).
    AbsenceNotCertified,
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
            Self::RealTimeOrderViolated => "real_time_order_violated",
            Self::ReportedWithoutCommand => "reported_without_command",
            Self::ReportedIndexWrong => "reported_index_wrong",
            Self::ReportedVersionWrong => "reported_version_wrong",
            Self::ReportedExistedWrong => "reported_existed_wrong",
            Self::ReportedCasOutcomeWrong => "reported_cas_outcome_wrong",
            Self::ReportedCurrentVersionWrong => "reported_current_version_wrong",
            Self::ReportedCapacityWrong => "reported_capacity_wrong",
            Self::ReportedDecisionWrong => "reported_decision_wrong",
            Self::ConflictWrongOriginal => "conflict_wrong_original",
            Self::ConflictWithoutOriginal => "conflict_without_original",
            Self::ConflictWithoutMisuse => "conflict_without_misuse",
            Self::ReadServedByWrongNode => "read_served_by_wrong_node",
            Self::ReadPrefixNotApplied => "read_prefix_not_applied",
            Self::ReadPrefixNotCommitted => "read_prefix_not_committed",
            Self::ReadValueWrong => "read_value_wrong",
            Self::ScanPageWrong => "scan_page_wrong",
            Self::StatusAnswerWrong => "status_answer_wrong",
            Self::ReadRegressed => "read_regressed",
            Self::TwoLeadersInTerm => "two_leaders_in_term",
            Self::DoubleVote => "double_vote",
            Self::SameTermDifferentBytes => "same_term_different_bytes",
            Self::PrefixDiffersUnderSameTerm => "prefix_differs_under_same_term",
            Self::LeaderTruncated => "leader_truncated",
            Self::LeaderMissingCommitted => "leader_missing_committed",
            Self::CommitBeyondCertificate => "commit_beyond_certificate",
            Self::NodeDivergesFromCertified => "node_diverges_from_certified",
            Self::StateDigestDiverges => "state_digest_diverges",
            Self::ConflictingCertificates => "conflicting_certificates",
            Self::CommittedWithoutCertificate => "committed_without_certificate",
            Self::VoteBeforeSync => "vote_before_sync",
            Self::AckBeforeSync => "ack_before_sync",
            Self::SuccessBeforeSync => "success_before_sync",
            Self::ReadBeforeSync => "read_before_sync",
            Self::AcceptBeforeSync => "accept_before_sync",
            Self::WrongAttemptState => "wrong_attempt_state",
            Self::GuessedFailureAfterSubmission => "guessed_failure_after_submission",
            Self::UnknownOutcomeWithoutSubmission => "unknown_outcome_without_submission",
            Self::SecondResponse => "second_response",
            Self::StatusContradictsItself => "status_contradicts_itself",
            Self::AbsenceContradicted => "absence_contradicted",
            Self::AbsenceWithoutEvidence => "absence_without_evidence",
            Self::WriteNotResolved => "write_not_resolved",
            Self::WriteResolvedTwice => "write_resolved_twice",
            Self::HistoryIncomplete => "history_incomplete",
            Self::IndexCollision => "index_collision",
            Self::IndexOutOfRange => "index_out_of_range",
            Self::ObservationsDisagree => "observations_disagree",
            Self::ResolutionContradicted => "resolution_contradicted",
            Self::NoLinearization => "no_linearization",
            Self::MarkerMissing => "marker_missing",
            Self::MarkerIndexDiffers => "marker_index_differs",
            Self::DigestDiffers => "digest_differs",
            Self::DrainMissing => "drain_missing",
            Self::DrainNotSettled => "drain_not_settled",
            Self::WriteUnresolved => "write_unresolved",
            Self::HealIncomplete => "heal_incomplete",
            Self::HealLate => "heal_late",
            Self::NotQuiescent => "not_quiescent",
            Self::AbsenceNotCertified => "absence_not_certified",
        })
    }
}

/// One broken rule, with the evidence to find it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// The rule broken.
    pub kind: Kind,
    /// The request ID concerned, as 32 hex characters; empty when the violation concerns no
    /// request (K-8 judges nodes, terms and indices).
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
