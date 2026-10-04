//! The seam between the controller's mechanism and the request lifecycle's policy
//! (PLAN.md §7.8, §7.9).
//!
//! The [`Controller`](crate::Controller) owns the mechanism: the inbound queue, the tick
//! clock and the deadline book, the Ready loop, the peer envelopes, the lifecycle and the
//! incarnation discipline. A [`Requests`] implementation owns the policy: the F-7 decision
//! order, the attempt state machine and the read path (W14.2, W14.3). The controller calls
//! it at fixed points of a turn and never interprets a client attempt itself.

use statemachine::{Applied, StateMachine};
use types::{
    AttemptId, ClientOp, Effect, Entry, Lifecycle, LogIndex, NodeId, ProposeError, Status, Tick,
};

use crate::deadlines::{Deadlines, DeadlinesFull};

/// A command to propose for an attempt (§7.8): handed to the library as one input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    /// The attempt it was proposed for, reported back in [`Requests::proposed`].
    pub attempt: AttemptId,
    /// The canonical command bytes (≤ 66 KiB, §7.11).
    pub command: Vec<u8>,
}

/// The request lifecycle the controller drives, one call at a time, inside a turn.
///
/// Every call is deterministic: the same calls from the same state push the same effects
/// in the same order (T-3). Effects are appended to `effects`, after the controller's own
/// effects so far.
pub trait Requests {
    /// A client attempt, taken from the queue this turn (§7.8, §7.9). Reads and replays may
    /// be answered at once, even while a sync is outstanding; a write that must be proposed
    /// waits until [`next_proposal`](Self::next_proposal) hands it to the library.
    fn attempt(
        &mut self,
        view: &mut View<'_>,
        attempt: AttemptId,
        op: ClientOp,
        effects: &mut Vec<Effect>,
    );

    /// The next command to propose, if any. The controller asks only when the library can
    /// take an input, and proposes it as that turn's one input. The attempt is `Submitted`
    /// from the moment this returns it: it is marked before the library sees it (§7.8).
    fn next_proposal(&mut self, view: &mut View<'_>) -> Option<Proposal>;

    /// The library took the proposal for `attempt`, or refused it with nothing appended
    /// (§7.8, A-5).
    fn proposed(
        &mut self,
        view: &mut View<'_>,
        attempt: AttemptId,
        result: Result<(), ProposeError>,
        effects: &mut Vec<Effect>,
    );

    /// The loop applied an entry to the state machine (§7.4 step 5, §7.5). Entries arrive
    /// once each, in index order.
    fn applied(&mut self, applying: &mut Applying<'_>, effects: &mut Vec<Effect>);

    /// `attempt`'s deadline passed (§7.8): the clock reached the tick it was set for. A
    /// deadline is cancelled when its attempt is answered, so it fires only for an attempt
    /// still waiting.
    fn expired(&mut self, view: &mut View<'_>, attempt: AttemptId, effects: &mut Vec<Effect>);

    /// The node's lifecycle, or the library's role, term or believed leader, changed since
    /// the last call (§7.8: leadership loss gives `unknown_outcome`).
    fn status_changed(&mut self, view: &mut View<'_>, effects: &mut Vec<Effect>);

    /// The process is stopping (§7.8, §8.4).
    fn shutdown(&mut self, view: &mut View<'_>, effects: &mut Vec<Effect>);
}

/// The node's state a [`Requests`] call may see, owned by the controller: the clock, the
/// lifecycle, the deadline book and the state machine.
///
/// Invariant: `lifecycle` only moves forward ([`Lifecycle::can_become`]).
#[derive(Debug)]
pub struct Core {
    /// This node.
    pub node: NodeId,
    /// The ticks processed since the node started.
    pub now: Tick,
    /// The node's lifecycle.
    pub lifecycle: Lifecycle,
    /// Whether the process is stopping.
    pub shutting_down: bool,
    /// The write deadline (`write_deadline_ticks`).
    pub write_deadline: Tick,
    /// The attempt deadlines.
    pub deadlines: Deadlines,
    /// The state machine: the published view and the request history.
    pub state: StateMachine,
}

/// What a [`Requests`] call may see and change of the node: read-only state, and the
/// deadline book.
#[derive(Debug)]
pub struct View<'a> {
    /// The node's state.
    core: &'a mut Core,
    /// The library's status: role, term, leader, commit and last index.
    status: &'a Status,
}

impl<'a> View<'a> {
    /// The view of `core` with the library's `status`.
    pub(crate) const fn new(core: &'a mut Core, status: &'a Status) -> Self {
        Self { core, status }
    }

    /// This node.
    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.core.node
    }

    /// The ticks processed since the node started: its logical time (§7.10).
    #[must_use]
    pub const fn now(&self) -> Tick {
        self.core.now
    }

    /// The node's lifecycle. Only `Ready` serves reads or admits writes (A-2).
    #[must_use]
    pub const fn lifecycle(&self) -> Lifecycle {
        self.core.lifecycle
    }

    /// The library's status.
    #[must_use]
    pub const fn status(&self) -> &Status {
        self.status
    }

    /// The state machine: the published view and the request history (§7.5, §7.9).
    #[must_use]
    pub const fn state(&self) -> &StateMachine {
        &self.core.state
    }

    /// Whether the process is stopping.
    #[must_use]
    pub const fn shutting_down(&self) -> bool {
        self.core.shutting_down
    }

    /// Sets `attempt`'s deadline to the write deadline from now, replacing any earlier one,
    /// and returns the tick it fires at (§7.8).
    ///
    /// # Errors
    ///
    /// [`DeadlinesFull`] if `pending_attempts` attempts already have deadlines (§7.11).
    pub fn start_deadline(&mut self, attempt: AttemptId) -> Result<Tick, DeadlinesFull> {
        let at = self
            .core
            .now
            .checked_add(self.core.write_deadline)
            .unwrap_or(Tick::new(u64::MAX));
        self.core.deadlines.set(attempt, at)?;
        Ok(at)
    }

    /// Removes `attempt`'s deadline, returning the tick it would have fired at.
    pub fn cancel_deadline(&mut self, attempt: AttemptId) -> Option<Tick> {
        self.core.deadlines.cancel(attempt)
    }

    /// `attempt`'s deadline, if it has one.
    #[must_use]
    pub fn deadline(&self, attempt: AttemptId) -> Option<Tick> {
        self.core.deadlines.get(attempt)
    }
}

/// What the loop just applied, for [`Requests::applied`]: the entry, what the state machine
/// decided, the state after it, and the deadline book.
#[derive(Debug)]
pub struct Applying<'a> {
    /// This node.
    pub(crate) node: NodeId,
    /// The entry applied.
    pub(crate) entry: &'a Entry,
    /// What applying it decided.
    pub(crate) applied: &'a Applied,
    /// The commit index it was applied under.
    pub(crate) commit: LogIndex,
    /// The state machine, after the entry.
    pub(crate) state: &'a StateMachine,
    /// The attempt deadlines.
    pub(crate) deadlines: &'a mut Deadlines,
}

impl Applying<'_> {
    /// This node.
    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.node
    }

    /// The entry applied.
    #[must_use]
    pub const fn entry(&self) -> &Entry {
        self.entry
    }

    /// What applying it decided (§7.5).
    #[must_use]
    pub const fn applied(&self) -> &Applied {
        self.applied
    }

    /// The commit index it was applied under.
    #[must_use]
    pub const fn commit(&self) -> LogIndex {
        self.commit
    }

    /// The state machine after the entry: its applied index is the entry's.
    #[must_use]
    pub const fn state(&self) -> &StateMachine {
        self.state
    }

    /// Removes `attempt`'s deadline, returning the tick it would have fired at.
    pub fn cancel_deadline(&mut self, attempt: AttemptId) -> Option<Tick> {
        self.deadlines.cancel(attempt)
    }
}
