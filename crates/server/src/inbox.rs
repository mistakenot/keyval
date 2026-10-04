//! The controller's hand-off (PLAN.md §7.11, §7.12): how inputs reach the one serial
//! controller task from the adapters' threads.
//!
//! It has three lanes, each bounded:
//! - **inputs**, client attempts and peer messages: at most `controller_queue_inputs`
//!   (1,024). Past that an offer is refused, and the adapter answers a client attempt
//!   `busy` (not submitted) or drops and counts a peer message, as the core's own queue
//!   does (§7.11);
//! - **unrefusable** inputs: I/O completions and the shutdown signal. They are never
//!   refused, so a waiting Ready always resolves (§7.12). They are bounded by construction:
//!   at most one sync is outstanding per node (§7.11), so at most one Ready's storage
//!   effects can complete before the controller takes them. `INBOX-UNREFUSABLE` asserts it;
//! - **ticks**, merged into a count, so a pause never queues one input per tick.
//!
//! The controller takes everything pending at once, and waits while nothing is.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

use types::{Input, invariant};

/// What the controller takes on one wake, in the order it admits them.
#[derive(Debug, Default)]
pub struct Batch {
    /// I/O completions and the shutdown signal.
    pub unrefusable: Vec<Input>,
    /// Ticks that elapsed since the last take.
    pub ticks: u64,
    /// Client attempts and peer messages, in arrival order.
    pub inputs: Vec<Input>,
    /// Whether the hand-off was closed: nothing more will arrive.
    pub closed: bool,
}

impl Batch {
    /// Whether the batch holds nothing to admit.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.unrefusable.is_empty() && self.ticks == 0 && self.inputs.is_empty()
    }
}

/// Why an offered input was not handed over: the caller answers or counts it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The inputs lane holds its bound.
    Full,
    /// The node is stopping: nothing more is taken.
    Closed,
}

/// The pending inputs.
#[derive(Debug, Default)]
struct Lanes {
    inputs: VecDeque<Input>,
    unrefusable: VecDeque<Input>,
    ticks: u64,
    closed: bool,
}

/// The hand-off: a mutex over the lanes, and a condition the controller waits on.
#[derive(Debug)]
pub struct Inbox {
    lanes: Mutex<Lanes>,
    wake: Condvar,
    /// The inputs lane's bound: `controller_queue_inputs`.
    max_inputs: usize,
    /// The unrefusable lane's bound, by construction.
    max_unrefusable: usize,
}

impl Inbox {
    /// A hand-off holding at most `max_inputs` client attempts and peer messages, and at
    /// most `max_unrefusable` completions (by construction, asserted).
    #[must_use]
    pub fn new(max_inputs: usize, max_unrefusable: usize) -> Self {
        Self {
            lanes: Mutex::new(Lanes::default()),
            wake: Condvar::new(),
            max_inputs,
            max_unrefusable,
        }
    }

    fn lanes(&self) -> MutexGuard<'_, Lanes> {
        self.lanes.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Offers a client attempt or a peer message.
    ///
    /// # Errors
    ///
    /// The input back, with [`Refused::Full`] when the lane holds its bound or
    /// [`Refused::Closed`] once the node is stopping.
    pub fn offer(&self, input: Input) -> Result<(), (Refused, Input)> {
        let mut lanes = self.lanes();
        if lanes.closed {
            return Err((Refused::Closed, input));
        }
        if lanes.inputs.len() >= self.max_inputs {
            return Err((Refused::Full, input));
        }
        lanes.inputs.push_back(input);
        drop(lanes);
        self.wake.notify_one();
        Ok(())
    }

    /// Hands over an I/O completion or the shutdown signal: never refused.
    ///
    /// # Panics
    ///
    /// `INBOX-UNREFUSABLE` if more than the lane's bound are pending: more storage effects
    /// completed than one Ready can have outstanding.
    pub fn complete(&self, input: Input) {
        let mut lanes = self.lanes();
        lanes.unrefusable.push_back(input);
        let pending = lanes.unrefusable.len();
        drop(lanes);
        invariant!(
            pending <= self.max_unrefusable,
            "INBOX-UNREFUSABLE",
            "{pending} completions are pending, more than the {} one Ready's storage \
             effects can produce",
            self.max_unrefusable
        );
        self.wake.notify_one();
    }

    /// Adds `ticks` elapsed ticks.
    pub fn tick(&self, ticks: u64) {
        if ticks == 0 {
            return;
        }
        let mut lanes = self.lanes();
        lanes.ticks = lanes.ticks.saturating_add(ticks);
        drop(lanes);
        self.wake.notify_one();
    }

    /// Closes the hand-off: later offers are refused, and the controller's next take
    /// reports it.
    pub fn close(&self) {
        self.lanes().closed = true;
        self.wake.notify_all();
    }

    /// Whether the hand-off was closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.lanes().closed
    }

    /// The client attempts and peer messages waiting.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.lanes().inputs.len()
    }

    /// Waits until something is pending or the hand-off is closed, then takes everything.
    pub fn take(&self) -> Batch {
        let mut lanes = self.lanes();
        while lanes.inputs.is_empty()
            && lanes.unrefusable.is_empty()
            && lanes.ticks == 0
            && !lanes.closed
        {
            lanes = self
                .wake
                .wait(lanes)
                .unwrap_or_else(PoisonError::into_inner);
        }
        Batch {
            unrefusable: lanes.unrefusable.drain(..).collect(),
            ticks: std::mem::take(&mut lanes.ticks),
            inputs: lanes.inputs.drain(..).collect(),
            closed: lanes.closed,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use types::{Incarnation, Input, NodeId, SyncSeq};

    use super::{Inbox, Refused};

    /// A peer message standing for any input of the inputs lane.
    fn client(n: u8) -> Input {
        Input::Peer {
            from: NodeId::new(2).unwrap(),
            bytes: vec![n],
        }
    }

    fn sync_done(n: u64) -> Input {
        Input::SyncDone {
            seq: SyncSeq::new(n),
            incarnation: Incarnation::new(1),
        }
    }

    #[test]
    fn the_inputs_lane_refuses_past_its_bound_and_completions_and_ticks_still_get_in() {
        let inbox = Inbox::new(2, 2);
        let offered: Vec<bool> = (1..=3).map(|n| inbox.offer(client(n)).is_ok()).collect();
        let refused = inbox.offer(Input::Peer {
            from: NodeId::new(2).unwrap(),
            bytes: vec![1],
        });
        inbox.complete(sync_done(1));
        inbox.tick(3);
        inbox.tick(2);
        let batch = inbox.take();
        assert_eq!(
            (
                offered,
                refused.map_err(|(why, _)| why),
                batch.unrefusable,
                batch.ticks,
                batch.inputs.len(),
                batch.closed
            ),
            (
                vec![true, true, false],
                Err(Refused::Full),
                vec![sync_done(1)],
                5,
                2,
                false
            ),
            "rule: client attempts and peer messages fill a lane bounded by \
             controller_queue_inputs and are refused past it; completions bypass the bound \
             and ticks merge into a count (§7.11, §7.12)"
        );
    }

    #[test]
    fn take_waits_for_an_input_and_closing_refuses_later_offers() {
        let inbox = Arc::new(Inbox::new(4, 2));
        let waiter = {
            let inbox = Arc::clone(&inbox);
            thread::spawn(move || inbox.take())
        };
        inbox.complete(sync_done(7));
        let woken = waiter.join().unwrap();
        inbox.close();
        let after = inbox.offer(client(1)).map_err(|(why, _)| why);
        let last = inbox.take();
        assert_eq!(
            (woken.unrefusable, after, last.closed, last.is_empty()),
            (vec![sync_done(7)], Err(Refused::Closed), true, true),
            "rule: the controller sleeps until something is handed over; once closed, offers \
             are refused and the take reports the close"
        );
    }

    #[test]
    #[should_panic(expected = "invariant INBOX-UNREFUSABLE violated")]
    fn more_completions_than_one_ready_can_produce_break_the_invariant() {
        let inbox = Inbox::new(4, 2);
        for n in 1..=3 {
            inbox.complete(sync_done(n));
        }
    }
}
