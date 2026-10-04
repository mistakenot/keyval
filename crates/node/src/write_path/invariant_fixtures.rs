//! Corrupt-state fixtures for the write path's invariants (CD-7): each one fires.

use statemachine::{StateLimits, StateMachine};
use types::{AttemptId, ClientOp, Lifecycle, LogIndex, Mutation, Role, Status, Term, Tick};

use super::WritePath;
use crate::deadlines::Deadlines;
use crate::requests::{Core, View};
use crate::test_support::{config, key, put, request, value};

/// Runs `f` on a fresh leader's write path and view, under the config called `name`.
fn on_leader(
    name: &str,
    deadlines: usize,
    f: impl FnOnce(&mut WritePath, &mut View<'_>, &mut Vec<types::Effect>),
) {
    let config = config(name);
    let node = config.voters().next().unwrap();
    let mut core = Core {
        node,
        now: Tick::ZERO,
        lifecycle: Lifecycle::Ready,
        shutting_down: false,
        write_deadline: config.write_deadline(),
        deadlines: Deadlines::new(node, deadlines),
        state: StateMachine::new(node, StateLimits::of(&config)),
    };
    let status = Status {
        role: Role::Leader,
        term: Term::new(2).unwrap(),
        leader: Some(node),
        commit: LogIndex::ZERO,
        last: LogIndex::ZERO,
        progress: Vec::new(),
    };
    let mut path = WritePath::new(&config, node);
    let mut view = View::new(&mut core, &status);
    f(&mut path, &mut view, &mut Vec::new());
}

#[test]
#[should_panic(
    expected = "invariant NODE-WRITE-VALID violated: node 1: attempt 1's command \
                does not decode under the configured limits"
)]
fn a_value_past_the_configured_limit_fires_node_write_valid() {
    // small-limits allows 64-byte values; the boundary would have refused this one.
    on_leader("small-limits", 4, |path, view, effects| {
        let mutation = Mutation::Put {
            key: key("k"),
            value: value(&"v".repeat(65)),
        };
        path.attempt(view, AttemptId::new(1), request(1), mutation, effects);
    });
}

#[test]
#[should_panic(
    expected = "invariant NODE-WRITE-DEADLINE violated: node 1: attempt 2 waits \
                without a deadline"
)]
fn a_waiting_attempt_without_a_deadline_fires_node_write_deadline() {
    // A deadline book with room for one: the second waiting attempt gets none.
    on_leader("one-voter", 1, |path, view, effects| {
        for (n, op) in [(1, put(1, "a", "v")), (2, put(2, "b", "v"))] {
            let ClientOp::Write {
                request_id,
                mutation,
            } = op
            else {
                panic!("fixture: a write");
            };
            path.attempt(view, AttemptId::new(n), request_id, mutation, effects);
        }
    });
}

#[test]
#[should_panic(
    expected = "invariant NODE-WRITE-TABLE violated: node 1: 1 attempts wait (0 \
                bytes counted"
)]
fn a_byte_count_that_disagrees_fires_node_write_table() {
    on_leader("one-voter", 4, |path, view, effects| {
        let ClientOp::Write {
            request_id,
            mutation,
        } = put(1, "a", "v")
        else {
            panic!("fixture: a write");
        };
        path.attempt(view, AttemptId::new(1), request_id, mutation, effects);
        path.attempt_bytes = 0;
        path.check();
    });
}

#[test]
#[should_panic(expected = "to_propose holds 2 for 1 queued (exact: false)")]
fn a_proposal_queued_twice_fires_node_write_table() {
    // br-nxsb: a request ID queued twice would be handed to the library twice.
    on_leader("one-voter", 4, |path, view, effects| {
        let ClientOp::Write {
            request_id,
            mutation,
        } = put(1, "a", "v")
        else {
            panic!("fixture: a write");
        };
        path.attempt(view, AttemptId::new(1), request_id, mutation, effects);
        path.to_propose.push_back(request_id);
        path.check();
    });
}
