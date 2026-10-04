//! W14.1's unit tests of the inbound queue and the deadline book (PLAN.md §7.8, §7.11,
//! CD-9): bounds at N−1, N and N+1, arrival order, merged ticks.
#![cfg(test)]

// check: NODE-CONTROLLER

use types::{AttemptId, ClientOp, NodeId, Tick};

use crate::deadlines::{Deadlines, DeadlinesFull};
use crate::queue::{InboundQueue, QueueFull, Queued};
use crate::test_support::put;

fn node() -> NodeId {
    NodeId::new(1).unwrap()
}

fn peer() -> NodeId {
    NodeId::new(2).unwrap()
}

fn get() -> ClientOp {
    ClientOp::Get {
        key: crate::test_support::key("k"),
    }
}

#[test]
fn the_queue_takes_its_bound_and_refuses_the_next_input() {
    let bound = 1_024;
    let mut queue = InboundQueue::new(node(), bound);
    for n in 0..bound - 1 {
        let accepted = if n % 2 == 0 {
            queue.offer_peer(peer(), vec![1]).is_ok()
        } else {
            queue
                .offer_client(AttemptId::new(u64::try_from(n).unwrap()), get())
                .is_ok()
        };
        assert!(accepted, "rule: input {n} of {bound} is queued (§7.11)");
    }
    assert_eq!(queue.len(), bound - 1, "fixture: N-1 inputs queued");
    assert!(
        queue.offer_client(AttemptId::new(9_999), get()).is_ok(),
        "rule: the N-th input still fits (bound {bound})"
    );
    assert_eq!(
        (
            queue.offer_peer(peer(), vec![2]),
            queue.offer_client(AttemptId::new(10_000), get()),
            queue.len()
        ),
        (Err(QueueFull), Err((AttemptId::new(10_000), get())), bound),
        "rule: at the bound, a peer envelope is refused and a client attempt is handed back \
         unchanged; nothing is queued past {bound} (§7.11)"
    );
}

#[test]
fn ticks_merge_into_a_count_and_never_take_a_place() {
    let mut queue = InboundQueue::new(node(), 1);
    queue.offer_peer(peer(), vec![1]).unwrap();
    for _ in 0..10_000 {
        queue.add_tick();
    }
    assert_eq!(
        (queue.len(), queue.pending_ticks()),
        (1, 10_000),
        "rule: ticks merge into the pending count, even when the queue is full (§7.11)"
    );
    let mut taken = 0;
    while queue.take_tick() {
        taken += 1;
    }
    assert_eq!(
        (taken, queue.pending_ticks(), queue.take_tick()),
        (10_000, 0, false),
        "rule: every merged tick is taken once"
    );
}

#[test]
fn inputs_leave_in_arrival_order_and_peers_wait_for_the_library() {
    let mut queue = InboundQueue::new(node(), 8);
    queue.offer_peer(peer(), vec![1]).unwrap();
    queue.offer_client(AttemptId::new(1), get()).unwrap();
    queue.offer_peer(peer(), vec![2]).unwrap();
    queue.offer_client(AttemptId::new(2), get()).unwrap();
    assert_eq!(
        queue.next(true),
        Some(Queued::Client {
            attempt: AttemptId::new(1),
            op: get()
        }),
        "rule: while a sync is outstanding, the oldest client attempt is taken past waiting \
         peer envelopes (§7.4 step 3, §7.9)"
    );
    let rest: Vec<Queued> = std::iter::from_fn(|| queue.next(false)).collect();
    assert_eq!(
        rest,
        vec![
            Queued::Peer {
                from: peer(),
                envelope: vec![1]
            },
            Queued::Peer {
                from: peer(),
                envelope: vec![2]
            },
            Queued::Client {
                attempt: AttemptId::new(2),
                op: get()
            },
        ],
        "rule: otherwise inputs leave in arrival order, across both kinds"
    );
    queue.offer_peer(peer(), vec![3]).unwrap();
    assert_eq!(
        (queue.next(true), queue.len()),
        (None, 1),
        "rule: a peer envelope waits while the library cannot take it"
    );
}

#[test]
fn a_write_is_queued_like_any_attempt() {
    let mut queue = InboundQueue::new(node(), 1);
    queue
        .offer_client(AttemptId::new(7), put(1, "k", "v"))
        .unwrap();
    assert_eq!(
        queue.next(false),
        Some(Queued::Client {
            attempt: AttemptId::new(7),
            op: put(1, "k", "v")
        }),
        "rule: the queue never inspects an attempt; the request lifecycle does (§7.8)"
    );
}

#[test]
fn deadlines_fire_in_tick_then_attempt_order_and_are_bounded() {
    let bound = 256;
    let mut book = Deadlines::new(node(), bound);
    for n in 0..bound - 1 {
        let n = u64::try_from(n).unwrap();
        book.set(AttemptId::new(1_000 - n), Tick::new(10 + n % 3))
            .unwrap();
    }
    assert!(
        book.set(AttemptId::new(5_000), Tick::new(10)).is_ok(),
        "rule: the N-th deadline fits (pending_attempts = {bound})"
    );
    assert_eq!(
        (book.set(AttemptId::new(5_001), Tick::new(10)), book.len()),
        (Err(DeadlinesFull { limit: bound }), bound),
        "rule: past pending_attempts a deadline is refused and nothing changes (§7.11)"
    );
    assert_eq!(
        (
            book.set(AttemptId::new(5_000), Tick::new(11)),
            book.get(AttemptId::new(5_000)),
            book.len()
        ),
        (Ok(()), Some(Tick::new(11)), bound),
        "rule: setting an attempt's deadline again replaces it, even when the book is full"
    );
    assert_eq!(
        book.pop_due(Tick::new(9)),
        None,
        "rule: nothing fires before its tick"
    );
    let mut fired = Vec::new();
    while let Some(attempt) = book.pop_due(Tick::new(10)) {
        fired.push(attempt);
    }
    let mut expected: Vec<AttemptId> = (0..u64::try_from(bound - 1).unwrap())
        .filter(|n| n % 3 == 0)
        .map(|n| AttemptId::new(1_000 - n))
        .collect();
    expected.sort();
    assert_eq!(
        fired, expected,
        "rule: the deadlines due at tick 10 fire once each, in attempt order"
    );
    assert_eq!(
        (
            book.cancel(AttemptId::new(5_000)),
            book.cancel(AttemptId::new(5_000))
        ),
        (Some(Tick::new(11)), None),
        "rule: a cancelled deadline is gone and never fires"
    );
}
