//! Corrupt-state fixtures for the queue's invariants (CD-7): each one fires.

use types::{AttemptId, ClientOp, NodeId};

use super::InboundQueue;

fn queue(max_inputs: usize) -> InboundQueue {
    InboundQueue::new(NodeId::new(1).unwrap(), max_inputs)
}

#[test]
#[should_panic(
    expected = "invariant NODE-QUEUE-BOUND violated: node 1: the controller queue \
                holds 1 inputs, over its bound of 0"
)]
fn an_input_past_the_bound_fires_node_queue_bound() {
    let mut queue = queue(0);
    queue.clients.push_back((
        0,
        AttemptId::new(1),
        ClientOp::Get {
            key: crate::test_support::key("k"),
        },
    ));
    queue.check_bound();
}

#[test]
#[should_panic(
    expected = "invariant NODE-TICK-COUNT violated: node 1: the pending tick count \
                would pass u64::MAX"
)]
fn a_tick_count_past_u64_max_fires_node_tick_count() {
    let mut queue = queue(1);
    queue.pending_ticks = u64::MAX;
    queue.add_tick();
}
