//! Attempt deadlines in ticks (PLAN.md §7.8, §7.10): a write's deadline is counted in ticks
//! from admission, so it behaves the same in simulation and production. The controller
//! advances the clock on every tick it processes, including while a sync is outstanding,
//! so deadlines always fire (§7.12).

use std::collections::{BTreeMap, BTreeSet};

use types::{AttemptId, NodeId, Tick, invariant};

#[cfg(test)]
mod invariant_fixtures;

/// The deadline book already holds `pending_attempts` deadlines (§7.11): the attempt is
/// answered `busy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeadlinesFull {
    /// The bound.
    pub limit: usize,
}

/// One deadline per pending attempt.
///
/// Invariants: `by_time` and `by_attempt` hold the same `(tick, attempt)` pairs, at most
/// `max` of them, one per attempt.
#[derive(Debug)]
pub struct Deadlines {
    /// The node, named in invariant messages (CD-7).
    node: NodeId,
    /// The deadlines in firing order: by tick, then by attempt.
    by_time: BTreeSet<(Tick, AttemptId)>,
    /// Each attempt's deadline.
    by_attempt: BTreeMap<AttemptId, Tick>,
    /// The bound (`pending_attempts`).
    max: usize,
}

impl Deadlines {
    /// An empty book holding at most `max` deadlines.
    pub const fn new(node: NodeId, max: usize) -> Self {
        Self {
            node,
            by_time: BTreeSet::new(),
            by_attempt: BTreeMap::new(),
            max,
        }
    }

    /// The deadlines held.
    pub fn len(&self) -> usize {
        self.by_attempt.len()
    }

    /// `attempt`'s deadline, if it has one.
    pub fn get(&self, attempt: AttemptId) -> Option<Tick> {
        self.by_attempt.get(&attempt).copied()
    }

    /// Sets `attempt`'s deadline to `at`, replacing any earlier one.
    ///
    /// # Errors
    ///
    /// [`DeadlinesFull`] if the attempt has none and the book is full; nothing changes.
    pub fn set(&mut self, attempt: AttemptId, at: Tick) -> Result<(), DeadlinesFull> {
        if let Some(old) = self.by_attempt.get(&attempt).copied() {
            self.by_time.remove(&(old, attempt));
        } else if self.by_attempt.len() >= self.max {
            return Err(DeadlinesFull { limit: self.max });
        }
        self.by_attempt.insert(attempt, at);
        self.by_time.insert((at, attempt));
        self.check();
        Ok(())
    }

    /// Removes `attempt`'s deadline, returning it.
    pub fn cancel(&mut self, attempt: AttemptId) -> Option<Tick> {
        let at = self.by_attempt.remove(&attempt)?;
        self.by_time.remove(&(at, attempt));
        self.check();
        Some(at)
    }

    /// Removes and returns the earliest deadline at or before `now`, if any.
    pub fn pop_due(&mut self, now: Tick) -> Option<AttemptId> {
        let (at, attempt) = *self.by_time.first()?;
        if at > now {
            return None;
        }
        self.by_time.remove(&(at, attempt));
        self.by_attempt.remove(&attempt);
        self.check();
        Some(attempt)
    }

    /// # Panics
    ///
    /// `NODE-DEADLINES` if the two indexes disagree or the book is over its bound.
    fn check(&self) {
        invariant!(
            self.by_time.len() == self.by_attempt.len() && self.by_attempt.len() <= self.max,
            "NODE-DEADLINES",
            "node {}: the deadline book holds {} by time and {} by attempt, bound {}",
            self.node,
            self.by_time.len(),
            self.by_attempt.len(),
            self.max
        );
    }
}
