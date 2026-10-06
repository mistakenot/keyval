//! The client retry policy (PLAN.md §10.3, §8.4's client guidance, §8.7, §7.11; A-5, P8;
//! W36.1): what a client does after each thing it hears about one logical operation. The
//! simulated clients use it, and so will the CLI (W33), so the two retry alike by
//! construction.
//!
//! A [`Retry`] follows one logical operation, a write or a read, from its first send to the
//! end:
//! - **One identity:** every send of a write carries the same request ID and the same command.
//!   The policy never invents a new ID; the caller resends what it first sent.
//! - **Terminal answers end it:** a written result (a success, `version_mismatch` or
//!   `capacity_exceeded`), `request_id_conflict`, a read's answer, or a status lookup that
//!   knows the result.
//! - **Redirects:** `not_leader` sends to the named leader at once.
//! - **Backoff:** after `busy` the client waits and tries the same node again; after
//!   `unavailable` or `recovering` it waits and tries another node. The wait doubles from
//!   [`FIRST_BACKOFF_MS`] up to [`MAX_BACKOFF_MS`], with seeded jitter.
//! - **Ambiguity:** after `unknown_outcome`, or silence past the caller's response timeout (a
//!   lost response), the write may have been submitted. The client waits and resends it with
//!   the same request ID, or sometimes asks the status lookup instead. A status of `unknown`
//!   means "resend", never "did not happen" (P8).
//! - **`history_full`:** if no send so far may have been submitted, the write was refused
//!   before proposal and retrying cannot help ([`Next::Refused`]). Otherwise only the status
//!   lookup can resolve it, so the client asks (§10.2).
//! - **Bounds (§7.11):** at most `CliRetryAttempts` sends (status lookups included), to at
//!   most `CliRetryNodes` distinct nodes, within `cli_deadline_ms` of the first. When they run
//!   out the operation is [`Next::Unresolved`], never reported as failed (§8.7): the caller
//!   keeps the request ID and command to resume later.
//!
//! Every choice (which other node, the jitter, whether to ask the status lookup) comes from
//! the `draw` the caller passes in, so a seeded caller gets a seeded sequence. The policy
//! reads no clock: times are milliseconds the caller supplies.

use crate::{Config, Limit, NodeId, Refusal, RequestState, Response};

/// The first backoff, in milliseconds.
pub const FIRST_BACKOFF_MS: u64 = 50;

/// The longest backoff, in milliseconds: the doubling stops here.
pub const MAX_BACKOFF_MS: u64 = 1_000;

/// One in this many ambiguous outcomes of a write is resolved through the status lookup
/// rather than by resending the write.
const ASK_STATUS_ONE_IN: u64 = 4;

/// What kind of operation a [`Retry`] follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    /// A write: PUT, DELETE or compare-and-swap, under one request ID.
    Write,
    /// A read: GET or SCAN. It has no request ID and nothing to resolve.
    Read,
}

/// The bounds of one retry sequence (§7.11).
///
/// Invariant: at least one send, one node and one millisecond.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryBounds {
    attempts: u64,
    nodes: usize,
    deadline_ms: u64,
}

impl RetryBounds {
    /// The bounds `config` sets: `CliRetryAttempts`, `CliRetryNodes` and `cli_deadline_ms`.
    #[must_use]
    pub fn of(config: &Config) -> Self {
        Self::new(
            config.limit(Limit::CliRetryAttempts),
            config.limit(Limit::CliRetryNodes),
            config.limit(Limit::CliDeadlineMs),
        )
    }

    /// Bounds of `attempts` sends to at most `nodes` distinct nodes within `deadline_ms`;
    /// each is at least 1.
    #[must_use]
    pub fn new(attempts: u64, nodes: u64, deadline_ms: u64) -> Self {
        Self {
            attempts: attempts.max(1),
            nodes: usize::try_from(nodes.max(1)).unwrap_or(usize::MAX),
            deadline_ms: deadline_ms.max(1),
        }
    }
}

/// Which bound ran out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exhausted {
    /// `CliRetryAttempts` sends were made.
    Attempts,
    /// The next send would go to one node more than `CliRetryNodes`.
    Nodes,
    /// The next send would fall `cli_deadline_ms` or more after the first.
    Deadline,
}

/// What the client does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Send the operation again, unchanged (the same request ID and command for a write), to
    /// `node` at `at_ms`.
    Send {
        /// Where.
        node: NodeId,
        /// When, in the caller's milliseconds.
        at_ms: u64,
    },
    /// Ask `node`'s status lookup for the write's request ID at `at_ms`.
    AskStatus {
        /// Where.
        node: NodeId,
        /// When, in the caller's milliseconds.
        at_ms: u64,
    },
    /// The operation has its terminal answer: the last thing heard.
    Done,
    /// A write `history_full` refused before proposal, with no send that may have been
    /// submitted: it did not execute, and retrying cannot help until the limit is raised.
    Refused,
    /// A bound ran out first: the operation is unresolved, never failed (§8.7).
    Unresolved(Exhausted),
}

/// What the client heard after its last send.
#[derive(Clone, Copy, Debug)]
pub enum Heard<'a> {
    /// An answer.
    Answer(&'a Response),
    /// Nothing by the caller's response timeout: the response was lost, or never came.
    Silence,
}

/// One logical operation's retry sequence.
///
/// Invariants: `tried` holds distinct nodes from `nodes`, at most `bounds.nodes` of them, and
/// includes `last`; `sends` is at most `bounds.attempts`; `backoff_ms` is 0 or between
/// [`FIRST_BACKOFF_MS`] and [`MAX_BACKOFF_MS`].
#[derive(Clone, Debug)]
pub struct Retry {
    bounds: RetryBounds,
    kind: OpKind,
    /// The nodes a client may send to, in configured order.
    nodes: Vec<NodeId>,
    /// The distinct nodes sent to so far, in first-send order.
    tried: Vec<NodeId>,
    /// Sends so far, status lookups included.
    sends: u64,
    /// When the first send went, in the caller's milliseconds.
    started_ms: u64,
    /// The last backoff, or 0 before the first.
    backoff_ms: u64,
    /// Where the last send went.
    last: NodeId,
    /// Whether some send of this write may have been submitted: one answered
    /// `unknown_outcome`, or met silence.
    maybe_submitted: bool,
}

impl Retry {
    /// Starts the sequence of one `kind` operation that may go to any of `nodes`: the first
    /// send goes to `first` at `now_ms`.
    #[must_use]
    pub fn start(
        bounds: RetryBounds,
        kind: OpKind,
        nodes: &[NodeId],
        first: NodeId,
        now_ms: u64,
    ) -> (Self, Next) {
        let mut retry = Self {
            bounds,
            kind,
            nodes: nodes.to_vec(),
            tried: Vec::new(),
            sends: 0,
            started_ms: now_ms,
            backoff_ms: 0,
            last: first,
            maybe_submitted: false,
        };
        let next = retry.dispatch(first, now_ms, false);
        (retry, next)
    }

    /// The sends made so far, status lookups included.
    #[must_use]
    pub const fn sends(&self) -> u64 {
        self.sends
    }

    /// The distinct nodes sent to so far.
    #[must_use]
    pub fn tried(&self) -> &[NodeId] {
        &self.tried
    }

    /// What to do after hearing `heard` about the last send, at `now_ms`, with `draw` for any
    /// choice (module documentation).
    pub fn heard(&mut self, heard: Heard<'_>, now_ms: u64, draw: u64) -> Next {
        match heard {
            Heard::Silence => self.ambiguous(now_ms, draw),
            Heard::Answer(response) => self.answer(response, now_ms, draw),
        }
    }

    fn answer(&mut self, response: &Response, now_ms: u64, draw: u64) -> Next {
        match response {
            Response::Written { .. }
            | Response::Conflict { .. }
            | Response::Found { .. }
            | Response::NotFound { .. }
            | Response::Page(_) => Next::Done,
            Response::Status { state, .. } => match state {
                RequestState::Known(_) => Next::Done,
                // Not applied there yet: never "did not happen" (P8). Resend the write.
                RequestState::Unknown => self.dispatch(self.last, now_ms, false),
            },
            Response::Refused(refusal) => match refusal {
                Refusal::NotLeader { leader } => self.dispatch(*leader, now_ms, false),
                Refusal::Busy => {
                    let at_ms = self.back_off(now_ms, draw);
                    self.dispatch(self.last, at_ms, false)
                }
                Refusal::Unavailable | Refusal::Recovering { .. } => {
                    let at_ms = self.back_off(now_ms, draw);
                    let node = self.other(draw >> 16);
                    self.dispatch(node, at_ms, false)
                }
                Refusal::UnknownOutcome => self.ambiguous(now_ms, draw),
                Refusal::HistoryFull { .. } => {
                    if self.maybe_submitted {
                        self.dispatch(self.last, now_ms, true)
                    } else {
                        Next::Refused
                    }
                }
            },
        }
    }

    /// No terminal answer, and a write may have been submitted: resend it with the same
    /// request ID, to this node or another, or sometimes ask the status lookup (§10.3).
    fn ambiguous(&mut self, now_ms: u64, draw: u64) -> Next {
        let write = match self.kind {
            OpKind::Write => true,
            OpKind::Read => false,
        };
        self.maybe_submitted |= write;
        let at_ms = self.back_off(now_ms, draw);
        let node = if (draw >> 16).is_multiple_of(2) {
            self.last
        } else {
            self.other(draw >> 17)
        };
        let ask = write && (draw >> 24).is_multiple_of(ASK_STATUS_ONE_IN);
        self.dispatch(node, at_ms, ask)
    }

    /// The next backoff from `now_ms`: double the last (from [`FIRST_BACKOFF_MS`], at most
    /// [`MAX_BACKOFF_MS`]), and wait between half of it and all of it.
    fn back_off(&mut self, now_ms: u64, draw: u64) -> u64 {
        self.backoff_ms = if self.backoff_ms == 0 {
            FIRST_BACKOFF_MS
        } else {
            self.backoff_ms.saturating_mul(2).min(MAX_BACKOFF_MS)
        };
        let half = self.backoff_ms / 2;
        now_ms.saturating_add(half + draw % (self.backoff_ms - half + 1))
    }

    /// A node other than the last: an untried one while the node bound allows, otherwise a
    /// tried one; the last itself if there is no other.
    fn other(&self, draw: u64) -> NodeId {
        let room = self.tried.len() < self.bounds.nodes;
        let candidates: Vec<NodeId> = self
            .nodes
            .iter()
            .copied()
            .filter(|node| *node != self.last && (room || self.tried.contains(node)))
            .collect();
        u64::try_from(candidates.len())
            .ok()
            .filter(|count| *count > 0)
            .and_then(|count| usize::try_from(draw % count).ok())
            .and_then(|at| candidates.get(at).copied())
            .unwrap_or(self.last)
    }

    /// Sends to `node` at `at_ms` (a status lookup if `ask`), or stops at the first bound it
    /// would break.
    fn dispatch(&mut self, node: NodeId, at_ms: u64, ask: bool) -> Next {
        if self.sends >= self.bounds.attempts {
            return Next::Unresolved(Exhausted::Attempts);
        }
        if at_ms.saturating_sub(self.started_ms) >= self.bounds.deadline_ms {
            return Next::Unresolved(Exhausted::Deadline);
        }
        if !self.tried.contains(&node) {
            if self.tried.len() >= self.bounds.nodes {
                return Next::Unresolved(Exhausted::Nodes);
            }
            self.tried.push(node);
        }
        self.sends += 1;
        self.last = node;
        if ask {
            Next::AskStatus { node, at_ms }
        } else {
            Next::Send { node, at_ms }
        }
    }
}

#[cfg(test)]
mod tests {
    // check: CLIENT-RETRY

    use super::*;
    use crate::{
        Decision, EntryIndex, KeyValueLimits, LogIndex, NotReadyLifecycle, ServedBy,
        TerminalResult, Value, Version,
    };

    fn node(n: u64) -> NodeId {
        NodeId::new(n).unwrap()
    }

    fn nodes() -> Vec<NodeId> {
        (1..=5).map(node).collect()
    }

    /// §7.11's CLI bounds: 10 sends, 5 nodes, 15 s.
    fn bounds() -> RetryBounds {
        RetryBounds::new(10, 5, 15_000)
    }

    /// Bounds loose enough that only the rule under test decides.
    fn loose() -> RetryBounds {
        RetryBounds::new(1_000, 5, u64::MAX)
    }

    fn served_by() -> ServedBy {
        ServedBy {
            node: node(1),
            applied: LogIndex::new(3).unwrap(),
        }
    }

    fn result() -> TerminalResult {
        TerminalResult::executed_at(EntryIndex::new(3).unwrap(), Decision::Set)
    }

    fn refused(refusal: Refusal) -> Response {
        Response::Refused(refusal)
    }

    /// A write sequence whose first send went to node `first` at 1,000 ms.
    fn write(bounds: RetryBounds, first: u64) -> Retry {
        Retry::start(bounds, OpKind::Write, &nodes(), node(first), 1_000).0
    }

    /// Where and when `next` sends, and whether it asks the status lookup.
    const fn sent(next: Next) -> Option<(NodeId, u64, bool)> {
        match next {
            Next::Send { node, at_ms } => Some((node, at_ms, false)),
            Next::AskStatus { node, at_ms } => Some((node, at_ms, true)),
            Next::Done | Next::Refused | Next::Unresolved(_) => None,
        }
    }

    #[test]
    fn the_first_send_goes_to_the_chosen_node_at_once() {
        let (retry, next) = Retry::start(bounds(), OpKind::Write, &nodes(), node(2), 1_000);
        assert_eq!(
            (next, retry.sends(), retry.tried()),
            (
                Next::Send {
                    node: node(2),
                    at_ms: 1_000
                },
                1,
                &[node(2)][..]
            ),
            "rule: a sequence starts with one send to the chosen node, at once"
        );
    }

    #[test]
    fn terminal_answers_end_the_sequence() {
        let value = Value::parse("v".to_owned(), KeyValueLimits::MAXIMA).unwrap();
        let terminal = [
            Response::Written {
                result: result(),
                replayed: false,
                served_by: served_by(),
            },
            Response::Written {
                result: TerminalResult::executed_at(
                    EntryIndex::new(3).unwrap(),
                    Decision::VersionMismatch { current: None },
                ),
                replayed: true,
                served_by: served_by(),
            },
            Response::Conflict {
                original_index: EntryIndex::new(2).unwrap(),
            },
            Response::Found {
                value,
                version: Version::at(LogIndex::new(3).unwrap()).unwrap(),
                served_by: served_by(),
            },
            Response::NotFound {
                served_by: served_by(),
            },
            Response::Status {
                state: RequestState::Known(result()),
                served_by: served_by(),
            },
        ];
        let nexts: Vec<Next> = terminal
            .iter()
            .map(|response| write(bounds(), 1).heard(Heard::Answer(response), 1_010, 7))
            .collect();
        assert_eq!(
            nexts,
            vec![Next::Done; terminal.len()],
            "rule: a written result (success, version_mismatch, capacity_exceeded), \
             request_id_conflict, a read's answer and a known status end the sequence \
             (§8.3, §8.4)"
        );
    }

    #[test]
    fn not_leader_redirects_at_once_to_the_named_leader() {
        let mut retry = write(bounds(), 1);
        let next = retry.heard(
            Heard::Answer(&refused(Refusal::NotLeader { leader: node(4) })),
            1_020,
            7,
        );
        assert_eq!(
            (next, retry.tried()),
            (
                Next::Send {
                    node: node(4),
                    at_ms: 1_020
                },
                &[node(1), node(4)][..]
            ),
            "rule: not_leader sends the same request to the named leader at once (§8.4)"
        );
    }

    #[test]
    fn busy_backs_off_on_the_same_node_doubling_to_the_cap() {
        let mut retry = write(loose(), 3);
        let mut now = 1_000;
        let mut waits = Vec::new();
        for draw in [0, u64::MAX, 5, 9_999, 1, 77, 3] {
            let next = retry.heard(Heard::Answer(&refused(Refusal::Busy)), now, draw);
            let (node, at, ask) = sent(next).expect("rule: busy is retried");
            assert!(
                node == self::node(3) && !ask,
                "rule: busy retries the same node; got {next:?}"
            );
            waits.push(at - now);
            now = at;
        }
        let caps = [50, 100, 200, 400, 800, 1_000, 1_000];
        assert!(
            waits
                .iter()
                .zip(caps)
                .all(|(wait, cap)| *wait >= cap / 2 && *wait <= cap),
            "rule: the backoff doubles from {FIRST_BACKOFF_MS} ms to at most {MAX_BACKOFF_MS} \
             ms, waiting between half of it and all of it; waits {waits:?}, caps {caps:?}"
        );
    }

    #[test]
    fn unavailable_and_recovering_back_off_to_another_node() {
        let refusals = [
            Refusal::Unavailable,
            Refusal::Recovering {
                lifecycle: NotReadyLifecycle::Recovering,
            },
        ];
        let moved = refusals.iter().all(|refusal| {
            (0..32).all(|draw| {
                let mut retry = write(loose(), 2);
                let next = retry.heard(Heard::Answer(&refused(*refusal)), 1_000, draw);
                sent(next).is_some_and(|(to, at, ask)| to != node(2) && at > 1_000 && !ask)
            })
        });
        assert!(
            moved,
            "rule: unavailable and recovering back off and try another node (§8.4)"
        );
    }

    #[test]
    fn an_ambiguous_write_is_resent_with_its_identity_or_resolved_by_the_status_lookup() {
        let mut kinds = (0, 0);
        for draw in 0..256_u64 {
            for heard in [
                Heard::Answer(&refused(Refusal::UnknownOutcome)),
                Heard::Silence,
            ] {
                let mut retry = write(loose(), 1);
                let next = retry.heard(heard, 1_000, draw.wrapping_mul(0x9E37_79B9_7F4A_7C15));
                match sent(next) {
                    Some((_, at, false)) if at > 1_000 => kinds.0 += 1,
                    Some((_, at, true)) if at > 1_000 => kinds.1 += 1,
                    Some(_) | None => panic!(
                        "rule: an ambiguous write is resent or resolved after a backoff, never \
                         ended; got {next:?} for draw {draw}"
                    ),
                }
            }
        }
        let read_asks = (0..256_u64).any(|draw| {
            let mut retry = Retry::start(loose(), OpKind::Read, &nodes(), node(1), 1_000).0;
            sent(retry.heard(Heard::Silence, 1_000, draw)).is_some_and(|(_, _, ask)| ask)
        });
        assert!(
            kinds.0 > 0 && kinds.1 > 0 && !read_asks,
            "rule: after unknown_outcome or a lost response, a write is resent with the same \
             request ID or sometimes resolved through the status lookup (§10.3, A-5); a read is \
             only resent; (resent, asked) = {kinds:?}"
        );
    }

    #[test]
    fn a_status_of_unknown_resends_the_write() {
        let mut retry = write(loose(), 1);
        let unknown = Response::Status {
            state: RequestState::Unknown,
            served_by: served_by(),
        };
        let next = retry.heard(Heard::Answer(&unknown), 1_500, 3);
        assert_eq!(
            sent(next).map(|(_, _, ask)| ask),
            Some(false),
            "rule: a status of unknown never means \"did not happen\": the write is resent with \
             its request ID (P8); got {next:?}"
        );
    }

    #[test]
    fn history_full_is_refused_only_when_nothing_may_have_been_submitted() {
        let full = refused(Refusal::HistoryFull {
            limit: crate::HistoryLimit::LogEntries,
        });
        let mut fresh = write(loose(), 1);
        let first = fresh.heard(Heard::Answer(&full), 1_000, 0);
        let mut ambiguous = write(loose(), 1);
        let resent = ambiguous.heard(Heard::Answer(&refused(Refusal::UnknownOutcome)), 1_000, 1);
        let at = sent(resent).map_or(1_000, |(_, at, _)| at);
        let after = ambiguous.heard(Heard::Answer(&full), at, 0);
        assert_eq!(
            (first, sent(after).map(|(_, _, ask)| ask)),
            (Next::Refused, Some(true)),
            "rule: history_full refuses a write that was never submitted; once a send may have \
             been submitted, only the status lookup can resolve it (§10.2, A-5)"
        );
    }

    #[test]
    fn the_bounds_end_the_sequence_unresolved_never_failed() {
        let busy = refused(Refusal::Busy);
        let mut attempts = write(bounds(), 1);
        let mut now = 1_000;
        let mut last = Next::Done;
        for _ in 0..20 {
            last = attempts.heard(Heard::Answer(&busy), now, 0);
            match sent(last) {
                Some((_, at, _)) => now = at,
                None => break,
            }
        }
        let mut deadline = write(bounds(), 1);
        let late = deadline.heard(Heard::Answer(&busy), 1_000 + 15_000, 0);
        let mut nodes_bound = write(RetryBounds::new(10, 2, 15_000), 1);
        nodes_bound.heard(
            Heard::Answer(&refused(Refusal::NotLeader { leader: node(2) })),
            1_000,
            0,
        );
        let third = nodes_bound.heard(
            Heard::Answer(&refused(Refusal::NotLeader { leader: node(3) })),
            1_000,
            0,
        );
        assert_eq!(
            (last, attempts.sends(), late, third),
            (
                Next::Unresolved(Exhausted::Attempts),
                10,
                Next::Unresolved(Exhausted::Deadline),
                Next::Unresolved(Exhausted::Nodes),
            ),
            "rule: at most CliRetryAttempts sends, within cli_deadline_ms, to at most \
             CliRetryNodes nodes; then the operation is unresolved, never failed (§7.11, §8.7)"
        );
    }

    #[test]
    fn the_same_draws_give_the_same_sequence() {
        let script = [
            refused(Refusal::Busy),
            refused(Refusal::UnknownOutcome),
            refused(Refusal::Unavailable),
            refused(Refusal::NotLeader { leader: node(5) }),
            refused(Refusal::UnknownOutcome),
        ];
        let run = || {
            let mut retry = write(bounds(), 1);
            let mut now = 1_000;
            script
                .iter()
                .enumerate()
                .map(|(at, response)| {
                    let next = retry.heard(
                        Heard::Answer(response),
                        now,
                        u64::try_from(at).unwrap() * 977,
                    );
                    if let Some((_, when, _)) = sent(next) {
                        now = when;
                    }
                    next
                })
                .collect::<Vec<Next>>()
        };
        assert_eq!(
            run(),
            run(),
            "rule: the policy is a pure function of what it heard, the times and the draws (P4)"
        );
    }
}
