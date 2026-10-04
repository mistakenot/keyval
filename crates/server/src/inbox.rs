//! The controller's hand-off (PLAN.md §7.11, §7.12): how inputs reach the one serial
//! controller task from the adapters' threads.
//!
//! It has three lanes, each bounded:
//! - **inputs**, client attempts and peer messages, under the node's **one** inbound bound,
//!   `controller_queue_inputs` (1,024, §7.11): the inputs waiting here plus those the
//!   controller **holds** (taken and not yet judged, or still queued in the core, such as
//!   peer messages waiting for an outstanding sync) never exceed it. Past it an offer is
//!   refused, and the adapter answers a client attempt `busy` (not submitted) or drops and
//!   counts a peer message, as §7.11 says. The controller settles what it holds after each
//!   batch ([`Inbox::settle`]), so the core's own queue of the same bound never fills and
//!   never refuses: the bound is applied once, and refusals are counted once (br-rl0u);
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
///
/// Invariant: `inputs.len() + held` never exceeds the inbox's `max_inputs`.
#[derive(Debug, Default)]
struct Lanes {
    inputs: VecDeque<Input>,
    /// Client attempts and peer messages the controller took and has not settled: being
    /// judged, or still queued in the core.
    held: usize,
    unrefusable: VecDeque<Input>,
    ticks: u64,
    closed: bool,
}

/// The hand-off: a mutex over the lanes, and a condition the controller waits on.
#[derive(Debug)]
pub struct Inbox {
    lanes: Mutex<Lanes>,
    wake: Condvar,
    /// The one inbound bound, `controller_queue_inputs`: over the inputs waiting here and
    /// those the controller holds.
    max_inputs: usize,
    /// The unrefusable lane's bound, by construction.
    max_unrefusable: usize,
}

impl Inbox {
    /// A hand-off under one bound of `max_inputs` client attempts and peer messages,
    /// counting those its controller holds, and at most `max_unrefusable` completions (by
    /// construction, asserted).
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
    /// The input back, with [`Refused::Full`] when the inputs waiting here and those the
    /// controller holds reach the one bound, or [`Refused::Closed`] once the node is
    /// stopping.
    ///
    /// # Panics
    ///
    /// `INBOX-ONE-BOUND` if accepting it left more than the bound held.
    pub fn offer(&self, input: Input) -> Result<(), (Refused, Input)> {
        let mut lanes = self.lanes();
        if lanes.closed {
            return Err((Refused::Closed, input));
        }
        if lanes.inputs.len() + lanes.held >= self.max_inputs {
            return Err((Refused::Full, input));
        }
        lanes.inputs.push_back(input);
        let total = lanes.inputs.len() + lanes.held;
        drop(lanes);
        invariant!(
            total <= self.max_inputs,
            "INBOX-ONE-BOUND",
            "{total} client attempts and peer messages are waiting or held, more than the one \
             inbound bound of {}",
            self.max_inputs
        );
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

    /// The client attempts and peer messages the controller holds: taken and not settled.
    #[must_use]
    pub fn held(&self) -> usize {
        self.lanes().held
    }

    /// The controller has judged everything it took: of the inputs it held, `queued` are
    /// still queued in the core ([`node::Controller::queued`]) and stay held; the rest were
    /// answered, dropped or handled, and leave the bound.
    ///
    /// # Panics
    ///
    /// `INBOX-ONE-BOUND` if the core queues more than the controller took and held: an
    /// input reached the core around the bound.
    pub fn settle(&self, queued: usize) {
        let mut lanes = self.lanes();
        let held = lanes.held;
        lanes.held = queued;
        drop(lanes);
        invariant!(
            queued <= held,
            "INBOX-ONE-BOUND",
            "the core queues {queued} client attempts and peer messages, more than the {held} \
             the controller took from the hand-off"
        );
    }

    /// Waits until something is pending or the hand-off is closed, then takes everything.
    /// The client attempts and peer messages taken are held until [`settle`](Self::settle).
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
        let inputs: Vec<Input> = lanes.inputs.drain(..).collect();
        // Taken inputs stay under the bound until the controller settles them.
        lanes.held += inputs.len();
        Batch {
            unrefusable: lanes.unrefusable.drain(..).collect(),
            ticks: std::mem::take(&mut lanes.ticks),
            inputs,
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
    fn what_the_controller_holds_counts_toward_the_one_bound() {
        let inbox = Inbox::new(4, 2);
        let first: Vec<bool> = (1..=4).map(|n| inbox.offer(client(n)).is_ok()).collect();
        let batch = inbox.take();
        // The controller is judging all four: the hand-off has no room.
        let while_judging = inbox.offer(client(5)).map_err(|(why, _)| why);
        // Three peer messages stay queued in the core behind an outstanding sync.
        inbox.settle(3);
        let behind_a_sync: Vec<bool> = (6..=7).map(|n| inbox.offer(client(n)).is_ok()).collect();
        let held = (inbox.held(), inbox.waiting());
        // The sync completes and the core takes them: the bound frees up.
        inbox.take();
        inbox.settle(0);
        let freed: Vec<bool> = (8..=12).map(|n| inbox.offer(client(n)).is_ok()).collect();
        assert_eq!(
            (
                first,
                batch.inputs.len(),
                while_judging,
                behind_a_sync,
                held,
                freed
            ),
            (
                vec![true; 4],
                4,
                Err(Refused::Full),
                vec![true, false],
                (3, 1),
                vec![true, true, true, true, false]
            ),
            "rule: client attempts and peer messages pass one bound end to end: the inputs \
             waiting in the hand-off plus those the controller holds, being judged or queued \
             in the core behind an outstanding sync, never exceed controller_queue_inputs \
             (§7.11, §7.12, br-rl0u)"
        );
    }

    #[test]
    fn concurrent_offers_never_hold_more_than_the_one_bound() {
        const BOUND: usize = 16;
        let inbox = Arc::new(Inbox::new(BOUND, 2));
        let offerers: Vec<_> = (0..4)
            .map(|_| {
                let inbox = Arc::clone(&inbox);
                thread::spawn(move || {
                    (0..500)
                        .filter(|n| inbox.offer(client(u8::try_from(n % 251).unwrap())).is_ok())
                        .count()
                })
            })
            .collect();
        // A controller that keeps a varying share queued, as peers behind a sync would be.
        let mut taken = 0;
        let mut queued = 0;
        let mut worst = 0;
        for round in 0..2_000 {
            // A tick keeps the take from waiting once the offerers are done.
            inbox.tick(1);
            let batch = inbox.take();
            taken += batch.inputs.len();
            let holding = queued + batch.inputs.len();
            worst = worst.max(holding + inbox.waiting());
            queued = holding.min(round % 7);
            inbox.settle(queued);
            if offerers.iter().all(thread::JoinHandle::is_finished) && inbox.waiting() == 0 {
                break;
            }
        }
        let accepted: usize = offerers
            .into_iter()
            .map(|offerer| offerer.join().unwrap())
            .sum();
        inbox.tick(1);
        taken += inbox.take().inputs.len();
        assert_eq!(
            (worst <= BOUND, accepted, accepted < 2_000),
            (true, taken, true),
            "rule: under concurrent offers the inputs waiting plus those the controller holds \
             never exceed the one bound, and every accepted input is taken exactly once \
             (§7.11, br-rl0u); worst {worst}, accepted {accepted}, taken {taken}"
        );
    }

    #[test]
    #[should_panic(expected = "invariant INBOX-ONE-BOUND violated")]
    fn a_core_queueing_more_than_it_took_breaks_the_invariant() {
        let inbox = Inbox::new(4, 2);
        inbox.offer(client(1)).unwrap();
        inbox.take();
        inbox.settle(2);
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
