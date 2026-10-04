//! Corrupt-state fixtures for the controller's invariants (CD-7): each one fires.

use raft_adapter::{Fed, RaftInput};
use types::{Effect, NodeId, Tick};

use super::{proposed, stepped};
use crate::test_support::{Member, config};

fn lone() -> Member {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    Member::fresh(&config, id)
}

#[test]
#[should_panic(
    expected = "invariant NODE-CLOCK violated: node 1: the clock would pass \
                u64::MAX ticks"
)]
fn a_clock_past_u64_max_fires_node_clock() {
    let mut member = lone();
    member.ctl.core.now = Tick::new(u64::MAX);
    let mut effects = Vec::new();
    member.ctl.tick(&mut effects);
}

#[test]
#[should_panic(
    expected = "invariant NODE-FEED-IDLE violated: node 1: an input was fed while \
                the loop could not take it"
)]
fn feeding_while_a_sync_is_outstanding_fires_node_feed_idle() {
    let mut member = lone();
    member.tick_until_syncing(100);
    let mut effects = Vec::new();
    member.ctl.feed(RaftInput::Tick, &mut effects);
}

#[test]
#[should_panic(
    expected = "invariant NODE-SEAL violated: node 1: a message to node 7 cannot \
                be sealed"
)]
fn a_send_to_a_non_voter_fires_node_seal() {
    let member = lone();
    let mut effects = vec![Effect::Send {
        to: NodeId::new(7).unwrap(),
        bytes: vec![1, 2, 3],
    }];
    member.ctl.seal(0, &mut effects);
}

#[test]
#[should_panic(
    expected = "invariant NODE-FED violated: node 1: a peer message was answered \
                Ticked"
)]
fn a_peer_message_answered_as_a_tick_fires_node_fed() {
    let _ = stepped(NodeId::new(1).unwrap(), &Fed::Ticked);
}

#[test]
#[should_panic(
    expected = "invariant NODE-FED violated: node 1: a proposal was answered \
                Ticked"
)]
fn a_proposal_answered_as_a_tick_fires_node_fed() {
    let _ = proposed(NodeId::new(1).unwrap(), &Fed::Ticked);
}
