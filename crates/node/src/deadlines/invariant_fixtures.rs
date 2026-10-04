//! Corrupt-state fixtures for the deadline book's invariant (CD-7): it fires.

use types::{AttemptId, NodeId, Tick};

use super::Deadlines;

#[test]
#[should_panic(
    expected = "invariant NODE-DEADLINES violated: node 1: the deadline book holds \
                1 by time and 0 by attempt, bound 4"
)]
fn indexes_that_disagree_fire_node_deadlines() {
    let mut book = Deadlines::new(NodeId::new(1).unwrap(), 4);
    book.by_time.insert((Tick::new(5), AttemptId::new(9)));
    book.check();
}
