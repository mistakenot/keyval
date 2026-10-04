//! The inbound controller queue (PLAN.md §7.11, §7.12): client attempts and peer envelopes
//! wait here for their turn, under one bound of `controller_queue_inputs` (1,024) inputs.
//! Ticks never take a place: they merge into a pending count. Completions never enter it,
//! so they are never dropped (the controller handles them at once).

use std::collections::VecDeque;

use types::{AttemptId, ClientOp, NodeId, invariant};

#[cfg(test)]
mod invariant_fixtures;

/// An input waiting for its turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Queued {
    /// A client attempt (client API port).
    Client {
        /// The attempt.
        attempt: AttemptId,
        /// What it asks for.
        op: ClientOp,
    },
    /// A peer envelope, not yet opened (peer transport port).
    Peer {
        /// The sender, as the transport reported it.
        from: NodeId,
        /// The envelope's bytes.
        envelope: Vec<u8>,
    },
}

/// The bounded queue.
///
/// Invariants: client attempts and peer envelopes together number at most `max_inputs`;
/// each lane holds its inputs in arrival order, and `next` takes the oldest input it may
/// take, so the order across lanes is arrival order too.
#[derive(Debug)]
pub struct InboundQueue {
    /// The node, named in invariant messages (CD-7).
    node: NodeId,
    /// Client attempts, each with its arrival number.
    clients: VecDeque<(u64, AttemptId, ClientOp)>,
    /// Peer envelopes, each with its arrival number.
    peers: VecDeque<(u64, NodeId, Vec<u8>)>,
    /// The next arrival number.
    arrivals: u64,
    /// The bound on queued inputs (`controller_queue_inputs`).
    max_inputs: usize,
    /// Ticks that arrived and have not been processed yet.
    pending_ticks: u64,
}

impl InboundQueue {
    /// An empty queue holding at most `max_inputs` inputs.
    pub const fn new(node: NodeId, max_inputs: usize) -> Self {
        Self {
            node,
            clients: VecDeque::new(),
            peers: VecDeque::new(),
            arrivals: 0,
            max_inputs,
            pending_ticks: 0,
        }
    }

    /// The queued inputs: client attempts and peer envelopes, not ticks.
    pub fn len(&self) -> usize {
        self.clients.len() + self.peers.len()
    }

    /// The ticks waiting to be processed.
    pub const fn pending_ticks(&self) -> u64 {
        self.pending_ticks
    }

    /// Queues a client attempt, or hands it back if the queue is full.
    pub fn offer_client(
        &mut self,
        attempt: AttemptId,
        op: ClientOp,
    ) -> Result<(), (AttemptId, ClientOp)> {
        if self.len() >= self.max_inputs {
            return Err((attempt, op));
        }
        let arrival = self.arrival();
        self.clients.push_back((arrival, attempt, op));
        self.check_bound();
        Ok(())
    }

    /// Queues a peer envelope, or reports that the queue is full (the caller drops and
    /// counts it).
    pub fn offer_peer(&mut self, from: NodeId, envelope: Vec<u8>) -> Result<(), QueueFull> {
        if self.len() >= self.max_inputs {
            return Err(QueueFull);
        }
        let arrival = self.arrival();
        self.peers.push_back((arrival, from, envelope));
        self.check_bound();
        Ok(())
    }

    /// Merges one tick into the pending count.
    ///
    /// # Panics
    ///
    /// `NODE-TICK-COUNT` if the count would pass `u64::MAX`.
    pub fn add_tick(&mut self) {
        let next = self.pending_ticks.checked_add(1);
        invariant!(
            next.is_some(),
            "NODE-TICK-COUNT",
            "node {}: the pending tick count would pass u64::MAX",
            self.node
        );
        self.pending_ticks = next.unwrap_or(u64::MAX);
    }

    /// Takes one pending tick, if any.
    pub const fn take_tick(&mut self) -> bool {
        match self.pending_ticks.checked_sub(1) {
            Some(rest) => {
                self.pending_ticks = rest;
                true
            }
            None => false,
        }
    }

    /// The oldest queued input, or, while `peers_wait`, the oldest client attempt: peer
    /// envelopes stay queued until the library can take them.
    pub fn next(&mut self, peers_wait: bool) -> Option<Queued> {
        let client_first = match (self.clients.front(), self.peers.front()) {
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some((client, ..)), Some((peer, ..))) => peers_wait || client < peer,
            (None, None) => return None,
        };
        if client_first {
            self.clients
                .pop_front()
                .map(|(_, attempt, op)| Queued::Client { attempt, op })
        } else if peers_wait {
            None
        } else {
            self.peers
                .pop_front()
                .map(|(_, from, envelope)| Queued::Peer { from, envelope })
        }
    }

    /// The next arrival number. One is taken per queued input, so it cannot reach
    /// `u64::MAX` in any run.
    const fn arrival(&mut self) -> u64 {
        let arrival = self.arrivals;
        self.arrivals = self.arrivals.saturating_add(1);
        arrival
    }

    /// # Panics
    ///
    /// `NODE-QUEUE-BOUND` if the queue holds more than its bound.
    fn check_bound(&self) {
        invariant!(
            self.len() <= self.max_inputs,
            "NODE-QUEUE-BOUND",
            "node {}: the controller queue holds {} inputs, over its bound of {} (§7.11)",
            self.node,
            self.len(),
            self.max_inputs
        );
    }
}

/// The queue already holds `controller_queue_inputs` inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueFull;
