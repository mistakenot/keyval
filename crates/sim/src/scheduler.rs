//! The discrete-event scheduler (PLAN.md §10.2): one thread, one queue, events ordered
//! by `(virtual_time, seeded_tie_priority, insertion_sequence)`.
//!
//! The seeded tie priority varies the order of simultaneous events between seeds, so
//! runs explore different interleavings; the insertion sequence makes the order total,
//! so one seed always gives one order. The queue is a `BTreeMap` keyed by that triple:
//! no hashing and no unordered iteration (§7.14).
//!
//! An event's causal parent is the event whose handling scheduled it: the one dispatched
//! last. The run's initial events have none.
//!
//! A run has an event budget: 1,000,000 per scenario manifest by default, the registered
//! `types::Limit::SimEventsPerRun` (§7.11). Exhausting it with events still pending makes
//! the run incomplete, which fails (P10); an incomplete run is never reported as a pass.
//!
//! The same budget bounds the pending queue (CD-9): it never holds more events than the
//! run may dispatch. The queue is a `types::BoundedMap` of that size. Scheduling past the
//! bound is a typed refusal, checked before any randomness is drawn, and the run is
//! incomplete from then on, so a fan-out bug ends in `Incomplete` instead of exhausting
//! memory.

use std::error::Error;
use std::fmt;

use types::{BoundedMap, Incarnation, ItemCount, Limit, NodeId};

use crate::entropy::SplitMix64;

/// Simulated time: virtual microseconds since the run started.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct VirtualTime(u64);

impl VirtualTime {
    /// The start of every run.
    pub const ZERO: Self = Self(0);

    /// `micros` virtual microseconds after the start.
    #[must_use]
    pub const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    /// Virtual microseconds since the start.
    #[must_use]
    pub const fn as_micros(self) -> u64 {
        self.0
    }
}

/// An event's insertion sequence: unique within a run and increasing in scheduling
/// order. It names the event in causal-parent links and breaks the last ties.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EventSeq(u64);

impl EventSeq {
    /// The sequence number (the run's first event is 0).
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A node in one incarnation: the node an event concerns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeIncarnation {
    /// The node.
    pub node: NodeId,
    /// Its incarnation when the event was scheduled.
    pub incarnation: Incarnation,
}

/// A dispatched event: when it fired, its place in the order, what caused it, the node
/// incarnation it concerns, and what happens (§10.2 event model).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event<E> {
    /// When it fired.
    pub time: VirtualTime,
    /// Its seeded tie priority among events at the same time.
    pub tie: u64,
    /// Its insertion sequence.
    pub seq: EventSeq,
    /// The event whose handling scheduled it; `None` for the run's initial events.
    pub parent: Option<EventSeq>,
    /// The node incarnation it concerns, where relevant.
    pub node: Option<NodeIncarnation>,
    /// What happens.
    pub payload: E,
}

/// How many events a run may dispatch (§5 limits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventBudget(u64);

impl EventBudget {
    /// The registered budget, [`Limit::SimEventsPerRun`]: a fixed 1,000,000 events per
    /// scenario manifest, read from the limits registry rather than restated here (CD-9).
    pub const DEFAULT: Self = Self(Limit::SimEventsPerRun.maximum());

    /// A budget of `events` events.
    #[must_use]
    pub const fn new(events: u64) -> Self {
        Self(events)
    }

    /// The number of events.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Why an event could not be scheduled. Nothing is scheduled and no randomness is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleError {
    /// It would fire before the current virtual time.
    InThePast {
        /// The scheduler's current time.
        now: VirtualTime,
        /// The time the event was scheduled for.
        at: VirtualTime,
    },
    /// The pending queue already holds as many events as the run's budget, its bound
    /// (CD-9). The run is incomplete from then on (P10).
    QueueFull {
        /// The bound: the run's event budget.
        bound: EventBudget,
    },
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InThePast { now, at } => write!(
                f,
                "an event at {} µs cannot be scheduled at virtual time {} µs",
                at.as_micros(),
                now.as_micros()
            ),
            Self::QueueFull { bound } => write!(
                f,
                "the pending queue is at its bound of {} events; the run is incomplete",
                bound.get()
            ),
        }
    }
}

impl Error for ScheduleError {}

/// Why a run is incomplete (P10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shortfall {
    /// The event budget was spent with events still pending.
    BudgetExhausted,
    /// An event was refused because the pending queue was at its bound.
    QueueFull,
}

/// What [`Scheduler::step`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step<E> {
    /// The next event, now dispatched.
    Event(Event<E>),
    /// The queue is empty: the run completed within its budget.
    Complete {
        /// Events dispatched.
        events: ItemCount,
    },
    /// The run cannot finish within its bounds: it is incomplete, which fails (P10).
    Incomplete {
        /// Events dispatched.
        events: ItemCount,
        /// Events still queued, never to be dispatched.
        pending: ItemCount,
        /// Which bound it hit.
        reason: Shortfall,
    },
}

/// The queue order: virtual time, then seeded tie priority, then insertion sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Order {
    time: VirtualTime,
    tie: u64,
    seq: EventSeq,
}

/// A queued event's record, apart from its order.
#[derive(Debug)]
struct Pending<E> {
    parent: Option<EventSeq>,
    node: Option<NodeIncarnation>,
    payload: E,
}

/// The single-threaded discrete-event scheduler.
#[derive(Debug)]
pub struct Scheduler<E> {
    /// The pending events, at most the event budget's worth (CD-9).
    queue: BoundedMap<Order, Pending<E>>,
    ties: SplitMix64,
    budget: EventBudget,
    now: VirtualTime,
    next_seq: u64,
    current: Option<EventSeq>,
    dispatched: u64,
    refused: bool,
}

impl<E> Scheduler<E> {
    /// An empty scheduler at virtual time zero. Its tie priorities come from `ties`, the
    /// run's [`Subsystem::Scheduler`](crate::Subsystem::Scheduler) stream.
    #[must_use]
    pub fn new(ties: SplitMix64, budget: EventBudget) -> Self {
        Self {
            queue: BoundedMap::new(usize::try_from(budget.get()).unwrap_or(usize::MAX)),
            ties,
            budget,
            now: VirtualTime::ZERO,
            next_seq: 0,
            current: None,
            dispatched: 0,
            refused: false,
        }
    }

    /// The current virtual time: that of the event dispatched last.
    #[must_use]
    pub const fn now(&self) -> VirtualTime {
        self.now
    }

    /// Events dispatched so far.
    #[must_use]
    pub const fn dispatched(&self) -> ItemCount {
        ItemCount::new(self.dispatched)
    }

    /// Events scheduled but not yet dispatched.
    #[must_use]
    pub fn pending(&self) -> ItemCount {
        ItemCount::new(u64::try_from(self.queue.len()).unwrap_or(u64::MAX))
    }

    /// The virtual time of the event [`Self::step`] would dispatch next, if any is pending.
    /// Looking draws no tie priority and dispatches nothing, so a driver can stop at a time
    /// without scheduling an event of its own: how often it looks never changes the run.
    #[must_use]
    pub fn next_time(&self) -> Option<VirtualTime> {
        self.queue.iter().next().map(|(order, _)| order.time)
    }

    /// Schedules `payload` at `at`, concerning `node` if any. Its causal parent is the
    /// event dispatched last; it draws its tie priority from the scheduler stream.
    ///
    /// # Errors
    ///
    /// Nothing is scheduled and no randomness is drawn on:
    /// - [`ScheduleError::InThePast`] if `at` is before [`Self::now`];
    /// - [`ScheduleError::QueueFull`] if the pending queue is at its bound, the run's
    ///   event budget. The run is then incomplete: every later [`Self::step`] reports
    ///   [`Shortfall::QueueFull`].
    pub fn schedule(
        &mut self,
        at: VirtualTime,
        node: Option<NodeIncarnation>,
        payload: E,
    ) -> Result<EventSeq, ScheduleError> {
        if at < self.now {
            return Err(ScheduleError::InThePast { now: self.now, at });
        }
        if self.queue.len() >= self.queue.max_entries() {
            self.refused = true;
            return Err(ScheduleError::QueueFull { bound: self.budget });
        }
        let seq = EventSeq(self.next_seq);
        self.next_seq += 1;
        let order = Order {
            time: at,
            tie: self.ties.next_u64(),
            seq,
        };
        let pending = Pending {
            parent: self.current,
            node,
            payload,
        };
        // The bound was checked above, before the tie was drawn, so this always fits.
        if self.queue.insert(order, pending).is_err() {
            self.refused = true;
            return Err(ScheduleError::QueueFull { bound: self.budget });
        }
        Ok(seq)
    }

    /// Dispatches the next event in order, advancing virtual time to it, or reports that
    /// the run is complete or incomplete. Once an event has been refused for a full
    /// queue, the run is incomplete and nothing more is dispatched.
    #[must_use]
    pub fn step(&mut self) -> Step<E> {
        if self.refused {
            return Step::Incomplete {
                events: self.dispatched(),
                pending: self.pending(),
                reason: Shortfall::QueueFull,
            };
        }
        if self.queue.is_empty() {
            return Step::Complete {
                events: self.dispatched(),
            };
        }
        if self.dispatched >= self.budget.get() {
            return Step::Incomplete {
                events: self.dispatched(),
                pending: self.pending(),
                reason: Shortfall::BudgetExhausted,
            };
        }
        let next = self.queue.iter().next().map(|(order, _)| *order);
        let Some((order, pending)) =
            next.and_then(|order| self.queue.remove(&order).map(|pending| (order, pending)))
        else {
            return Step::Complete {
                events: self.dispatched(),
            };
        };
        self.now = order.time;
        self.current = Some(order.seq);
        self.dispatched += 1;
        Step::Event(Event {
            time: order.time,
            tie: order.tie,
            seq: order.seq,
            parent: pending.parent,
            node: pending.node,
            payload: pending.payload,
        })
    }
}

#[cfg(test)]
mod tests {
    // check: SIM-SCHEDULER
    use super::*;
    use crate::entropy::{MasterSeed, Subsystem};

    fn count(items: u64) -> ItemCount {
        ItemCount::new(items)
    }

    fn scheduler(seed: u64, budget: EventBudget) -> Scheduler<u32> {
        Scheduler::new(MasterSeed::new(seed).stream(Subsystem::Scheduler), budget)
    }

    const fn at(micros: u64) -> VirtualTime {
        VirtualTime::from_micros(micros)
    }

    fn on(node: u64, incarnation: u64) -> NodeIncarnation {
        NodeIncarnation {
            node: NodeId::new(node).unwrap(),
            incarnation: Incarnation::new(incarnation),
        }
    }

    /// Dispatches every event; returns them and the final step.
    fn drain(scheduler: &mut Scheduler<u32>) -> (Vec<Event<u32>>, Step<u32>) {
        let mut events = Vec::new();
        loop {
            match scheduler.step() {
                Step::Event(event) => events.push(event),
                done => return (events, done),
            }
        }
    }

    fn payloads(events: &[Event<u32>]) -> Vec<u32> {
        events.iter().map(|event| event.payload).collect()
    }

    #[test]
    fn next_time_peeks_without_drawing_or_dispatching() {
        let mut peeked = scheduler(5, EventBudget::DEFAULT);
        let mut plain = scheduler(5, EventBudget::DEFAULT);
        assert_eq!(
            peeked.next_time(),
            None,
            "rule: an empty queue has no next event"
        );
        for s in [&mut peeked, &mut plain] {
            s.schedule(at(20), None, 2).unwrap();
            s.schedule(at(10), None, 1).unwrap();
        }
        for _ in 0..3 {
            assert_eq!(
                peeked.next_time(),
                Some(at(10)),
                "rule: the earliest pending event's time, however often one looks"
            );
        }
        assert_eq!(
            peeked.now(),
            VirtualTime::ZERO,
            "rule: looking dispatches nothing"
        );
        assert_eq!(
            peeked.pending(),
            count(2),
            "rule: looking dispatches nothing"
        );
        for s in [&mut peeked, &mut plain] {
            s.schedule(at(10), None, 3).unwrap();
        }
        let (looked, _) = drain(&mut peeked);
        let (unlooked, _) = drain(&mut plain);
        assert_eq!(
            looked, unlooked,
            "rule: looking draws no tie priority, so the run is the one it would have been"
        );
        assert_eq!(
            peeked.next_time(),
            None,
            "rule: a drained queue has no next event"
        );
    }

    #[test]
    fn events_fire_in_virtual_time_order() {
        let mut s = scheduler(1, EventBudget::DEFAULT);
        for (time, payload) in [(30, 3), (10, 1), (20, 2)] {
            s.schedule(at(time), None, payload).unwrap();
        }
        let (events, done) = drain(&mut s);
        assert_eq!(payloads(&events), [1, 2, 3]);
        assert_eq!(
            s.now(),
            at(30),
            "rule: time advances to the event dispatched last"
        );
        assert_eq!(done, Step::Complete { events: count(3) });
    }

    #[test]
    fn simultaneous_events_order_by_seeded_priority_and_vary_between_seeds() {
        let order = |seed| {
            let mut s = scheduler(seed, EventBudget::DEFAULT);
            for payload in 0..6 {
                s.schedule(at(100), None, payload).unwrap();
            }
            let (events, _) = drain(&mut s);
            assert!(
                events.windows(2).all(|pair| pair[0].tie <= pair[1].tie),
                "rule: simultaneous events dispatch in tie-priority order"
            );
            payloads(&events)
        };
        assert_eq!(order(11), order(11), "rule: one seed gives one order");
        let orders: Vec<Vec<u32>> = (0..16).map(order).collect();
        assert!(
            orders.iter().any(|other| *other != orders[0]),
            "rule: the seeded tie priority varies simultaneous-event order between seeds"
        );
    }

    #[test]
    fn the_insertion_sequence_makes_the_order_total() {
        let first = Order {
            time: at(5),
            tie: 7,
            seq: EventSeq(1),
        };
        let second = Order {
            time: at(5),
            tie: 7,
            seq: EventSeq(2),
        };
        assert!(
            first < second,
            "rule: equal time and tie fall back to insertion order"
        );
        assert!(
            Order {
                time: at(4),
                tie: u64::MAX,
                seq: EventSeq(9),
            } < first,
            "rule: virtual time orders first"
        );
        assert!(
            Order {
                time: at(5),
                tie: 6,
                seq: EventSeq(9),
            } < first,
            "rule: then the tie priority"
        );
    }

    #[test]
    fn records_carry_time_sequence_parent_and_incarnation() {
        let mut s = scheduler(2, EventBudget::DEFAULT);
        let boot = s.schedule(at(0), Some(on(1, 1)), 10).unwrap();
        let tick = s.schedule(at(50), Some(on(2, 3)), 20).unwrap();
        assert!(
            boot < tick,
            "rule: insertion sequences grow in scheduling order"
        );
        let Step::Event(first) = s.step() else {
            panic!("expected the boot event");
        };
        assert_eq!(
            (
                first.time,
                first.seq,
                first.parent,
                first.node,
                first.payload
            ),
            (at(0), boot, None, Some(on(1, 1)), 10),
            "rule: an initial event has no causal parent"
        );
        let child = s.schedule(at(20), None, 30).unwrap();
        let (events, _) = drain(&mut s);
        assert_eq!(payloads(&events), [30, 20]);
        assert_eq!(
            (events[0].seq, events[0].parent, events[0].node),
            (child, Some(boot), None),
            "rule: the causal parent is the event whose handling scheduled it"
        );
        assert_eq!(events[1].node, Some(on(2, 3)));
    }

    #[test]
    fn scheduling_into_the_past_is_refused() {
        let mut s = scheduler(3, EventBudget::DEFAULT);
        s.schedule(at(100), None, 1).unwrap();
        assert!(matches!(s.step(), Step::Event(_)));
        assert_eq!(
            s.schedule(at(99), None, 2),
            Err(ScheduleError::InThePast {
                now: at(100),
                at: at(99),
            })
        );
        assert_eq!(s.pending(), count(0), "rule: a refused event is not queued");
        assert!(
            s.schedule(at(100), None, 3).is_ok(),
            "rule: the current time is not the past"
        );
    }

    #[test]
    fn exhausting_the_budget_with_events_pending_is_incomplete() {
        let mut s = scheduler(4, EventBudget::new(3));
        for payload in 0..2 {
            s.schedule(at(u64::from(payload)), None, payload).unwrap();
        }
        let mut events = Vec::new();
        let done = loop {
            match s.step() {
                Step::Event(event) => {
                    // Each event schedules a follow-up, so work outlasts the budget.
                    s.schedule(at(event.time.as_micros() + 10), None, event.payload + 10)
                        .unwrap();
                    events.push(event);
                }
                done => break done,
            }
        };
        assert_eq!(payloads(&events), [0, 1, 10]);
        let incomplete = Step::Incomplete {
            events: count(3),
            pending: count(2),
            reason: Shortfall::BudgetExhausted,
        };
        assert_eq!(
            done, incomplete,
            "rule: an exhausted budget with work left is incomplete, never a pass (P10)"
        );
        assert_eq!(s.step(), incomplete, "rule: nothing more is dispatched");
        assert_eq!(s.dispatched(), count(3));
    }

    #[test]
    fn the_pending_queue_is_bounded_by_the_event_budget() {
        let bound = EventBudget::new(4);
        let mut s = scheduler(8, bound);
        for payload in 0..3 {
            s.schedule(at(5), None, payload).unwrap();
        }
        assert_eq!(s.pending(), count(3), "rule: N-1 pending events fit");
        assert!(
            s.schedule(at(5), None, 3).is_ok(),
            "rule: N pending events fit"
        );
        assert_eq!(
            s.schedule(at(5), None, 4),
            Err(ScheduleError::QueueFull { bound }),
            "rule: the (N+1)th pending event is a typed refusal, not silent growth (CD-9)"
        );
        assert_eq!(s.pending(), count(4), "rule: a refused event is not queued");
        let incomplete = Step::Incomplete {
            events: count(0),
            pending: count(4),
            reason: Shortfall::QueueFull,
        };
        assert_eq!(
            s.step(),
            incomplete,
            "rule: after a refusal the run is incomplete, even if the caller ignores the error"
        );
        assert_eq!(s.step(), incomplete, "rule: it never turns complete");
        assert_eq!(s.dispatched(), count(0));
    }

    #[test]
    fn dispatching_frees_room_in_the_pending_queue() {
        let mut s = scheduler(9, EventBudget::new(2));
        s.schedule(at(1), None, 1).unwrap();
        s.schedule(at(2), None, 2).unwrap();
        assert!(matches!(s.step(), Step::Event(_)));
        assert!(
            s.schedule(at(3), None, 3).is_ok(),
            "rule: the bound counts pending events, not events ever scheduled"
        );
        assert!(matches!(
            s.schedule(at(4), None, 4),
            Err(ScheduleError::QueueFull { .. })
        ));
    }

    #[test]
    fn draining_within_the_budget_is_complete() {
        let mut s = scheduler(5, EventBudget::new(3));
        for payload in 0..3 {
            s.schedule(at(1), None, payload).unwrap();
        }
        let (events, done) = drain(&mut s);
        assert_eq!(events.len(), 3);
        assert_eq!(
            done,
            Step::Complete { events: count(3) },
            "rule: spending the whole budget with nothing pending is complete"
        );
    }

    #[test]
    fn the_default_budget_is_the_registered_one() {
        assert_eq!(
            Limit::SimEventsPerRun.source(),
            types::LimitSource::Fixed(EventBudget::DEFAULT.get()),
            "rule: the budget is the registered limit (§7.11, CD-9), not a restated literal"
        );
    }

    #[test]
    fn a_refused_schedule_draws_no_tie_and_takes_no_sequence() {
        let mut refused = scheduler(13, EventBudget::new(4));
        let mut clean = scheduler(13, EventBudget::new(4));
        for s in [&mut refused, &mut clean] {
            s.schedule(at(10), None, 0).unwrap();
            assert!(matches!(s.step(), Step::Event(_)));
        }
        assert!(matches!(
            refused.schedule(at(5), None, 1),
            Err(ScheduleError::InThePast { .. })
        ));
        assert_eq!(
            (&refused.ties, refused.next_seq),
            (&clean.ties, clean.next_seq),
            "rule: a refusal in the past draws no randomness and takes no sequence number"
        );
        let mut full = scheduler(14, EventBudget::new(1));
        let mut room = scheduler(14, EventBudget::new(1));
        full.schedule(at(1), None, 0).unwrap();
        room.schedule(at(1), None, 0).unwrap();
        assert!(matches!(
            full.schedule(at(2), None, 1),
            Err(ScheduleError::QueueFull { .. })
        ));
        assert_eq!(
            (&full.ties, full.next_seq),
            (&room.ties, room.next_seq),
            "rule: a queue-full refusal draws no randomness and takes no sequence number"
        );
    }

    #[test]
    fn inspecting_the_scheduler_draws_no_randomness() {
        let mut observed = scheduler(6, EventBudget::DEFAULT);
        let mut untouched = scheduler(6, EventBudget::DEFAULT);
        for payload in 0..4 {
            observed.schedule(at(10), None, payload).unwrap();
            untouched.schedule(at(10), None, payload).unwrap();
            for _ in 0..100 {
                let _ = (observed.now(), observed.pending(), observed.dispatched());
            }
        }
        assert_eq!(
            drain(&mut observed),
            drain(&mut untouched),
            "rule: observing a run never consumes randomness (§10.2)"
        );
    }

    #[test]
    fn the_same_seed_gives_the_same_schedule() {
        let run = |seed| {
            let mut s = scheduler(seed, EventBudget::new(200));
            for payload in 0..4 {
                s.schedule(at(0), Some(on(u64::from(payload) + 1, 1)), payload)
                    .unwrap();
            }
            let mut trace = Vec::new();
            while let Step::Event(event) = s.step() {
                trace.push((
                    event.time,
                    event.tie,
                    event.seq,
                    event.parent,
                    event.payload,
                ));
                let delay = u64::from(event.payload % 3) * 10;
                s.schedule(
                    at(event.time.as_micros() + delay),
                    event.node,
                    event.payload + 1,
                )
                .unwrap();
            }
            trace
        };
        let first = run(77);
        assert_eq!(first.len(), 200);
        assert_eq!(
            run(77),
            first,
            "rule: the same seed and build give the same schedule (P4)"
        );
        assert_ne!(run(78), first);
    }
}
