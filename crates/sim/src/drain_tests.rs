//! Tests of the certified drain (W43.2; PLAN.md §10.2, D-53). Negative fixtures: each rule
//! the judge states is broken once and must be rejected, and a control fixture shows the
//! same look passes when nothing is broken.
// check: DRAIN-CERTIFIED

use statemachine::{Command, StateDigest, StateLimits, StateMachine, codec};
use types::{
    ByteCount, Entry, EntryIndex, EntryPayload, ItemCount, Key, KeyValueLimits, LogIndex, Mutation,
    NodeId, RequestId, Role, Term, Value,
};

use crate::drain::{Agreement, NodeSample, QUIET_TICKS, QuietClock, Sample, Uncertified};
use crate::scheduler::VirtualTime;

const TICK_MS: u64 = 10;

type Change = fn(&mut NodeSample);

fn node(raw: u64) -> NodeId {
    NodeId::new(raw).unwrap()
}

fn index(raw: u64) -> LogIndex {
    LogIndex::new(raw).unwrap()
}

fn term(raw: u64) -> Term {
    Term::new(raw).unwrap()
}

fn machine() -> StateMachine {
    let limits = StateLimits {
        key_values: KeyValueLimits::MAXIMA,
        max_live_keys: ItemCount::new(100),
        max_live_bytes: ByteCount::new(1 << 20),
    };
    StateMachine::new(node(1), limits)
}

/// The digest of a state machine that applied one put of `value`.
fn digest_after_put(value: &str) -> StateDigest {
    let mut state = machine();
    let command = Command {
        request_id: RequestId::from_bytes([7; 16]),
        mutation: Mutation::Put {
            key: Key::parse("k".to_owned(), KeyValueLimits::MAXIMA).unwrap(),
            value: Value::parse(value.to_owned(), KeyValueLimits::MAXIMA).unwrap(),
        },
    };
    let entry = Entry {
        index: EntryIndex::new(1).unwrap(),
        term: term(1),
        payload: EntryPayload::Command(codec::encode(&command)),
    };
    let _ = state.apply(&entry, index(1));
    state.digest()
}

fn follower(raw: u64) -> NodeSample {
    NodeSample {
        node: node(raw),
        role: Role::Follower,
        term: term(3),
        leader: Some(node(1)),
        last: index(5),
        commit: index(5),
        applied: index(5),
        digest: digest_after_put("a"),
        queued: 0,
    }
}

fn leader() -> NodeSample {
    NodeSample {
        role: Role::Leader,
        leader: Some(node(1)),
        ..follower(1)
    }
}

/// A drained three-node look: nothing in flight, node 1 leads, all agree.
fn drained() -> Sample {
    Sample {
        client_work: 0,
        nodes: vec![leader(), follower(2), follower(3)],
        down: Vec::new(),
    }
}

fn rejected(sample: &Sample, rule: &str) -> Uncertified {
    let verdict = sample.judge();
    assert!(
        verdict.is_err(),
        "rule: {rule} must not certify a drain; sample={sample:?}"
    );
    verdict.unwrap_err()
}

#[test]
fn control_a_settled_cluster_is_a_drain() {
    let agreed = drained().judge();
    let agreement = agreed.expect("rule: a settled cluster certifies (control fixture)");
    assert_eq!(agreement.leader, node(1), "rule: the leader is reported");
    assert_eq!(
        agreement.index,
        index(5),
        "rule: the common index is reported"
    );
}

#[test]
fn client_work_in_flight_is_not_a_drain() {
    let mut sample = drained();
    sample.client_work = 2;
    assert_eq!(
        rejected(&sample, "client work in flight"),
        Uncertified::ClientWork { in_flight: 2 }
    );
}

#[test]
fn queued_node_work_is_not_a_drain() {
    let mut sample = drained();
    sample.nodes[1].queued = 3;
    assert_eq!(
        rejected(&sample, "queued node work"),
        Uncertified::Queued {
            node: node(2),
            queued: 3
        }
    );
}

#[test]
fn a_node_that_is_down_is_not_a_drain() {
    let mut sample = drained();
    sample.down.push(node(4));
    assert_eq!(
        rejected(&sample, "a recoverable node down"),
        Uncertified::NodeDown { node: node(4) }
    );
}

#[test]
fn no_leader_is_not_a_drain() {
    let mut sample = drained();
    sample.nodes[0].role = Role::Follower;
    assert_eq!(rejected(&sample, "no leader"), Uncertified::NoLeader);
}

#[test]
fn two_leaders_are_not_a_drain() {
    let mut sample = drained();
    sample.nodes[1].role = Role::Leader;
    assert_eq!(
        rejected(&sample, "two leaders"),
        Uncertified::TwoLeaders {
            nodes: [node(1), node(2)]
        }
    );
}

#[test]
fn a_node_in_another_term_is_not_a_drain() {
    let mut sample = drained();
    sample.nodes[2].term = term(2);
    assert_eq!(
        rejected(&sample, "an unstable term"),
        Uncertified::Term { node: node(3) }
    );
}

#[test]
fn a_follower_that_does_not_follow_is_not_a_drain() {
    let mut sample = drained();
    sample.nodes[1].leader = None;
    assert_eq!(
        rejected(&sample, "a follower with no leader"),
        Uncertified::Follows { node: node(2) }
    );
}

#[test]
fn last_commit_applied_must_all_match() {
    let cases: [(&str, Change); 3] = [
        ("last ahead of commit", |n| n.last = index(6)),
        ("commit ahead of applied", |n| {
            n.last = index(6);
            n.commit = index(6);
        }),
        ("applied behind commit", |n| n.applied = index(4)),
    ];
    for (name, change) in cases {
        let mut sample = drained();
        change(&mut sample.nodes[1]);
        assert_eq!(
            rejected(&sample, name),
            Uncertified::Indexes { node: node(2) },
            "rule: {name}; sample={sample:?}"
        );
    }
}

#[test]
fn a_different_common_index_is_not_a_drain() {
    let mut sample = drained();
    sample.nodes[2].last = index(4);
    sample.nodes[2].commit = index(4);
    sample.nodes[2].applied = index(4);
    assert_eq!(
        rejected(&sample, "a node behind the others"),
        Uncertified::IndexDiffers { node: node(3) }
    );
}

#[test]
fn differing_state_digests_are_not_a_drain() {
    let mut sample = drained();
    sample.nodes[2].digest = digest_after_put("b");
    assert_eq!(
        rejected(&sample, "differing map or history digests"),
        Uncertified::Digest { node: node(3) }
    );
}

fn at_tick(tick: u64) -> VirtualTime {
    VirtualTime::from_micros(tick * TICK_MS * 1_000)
}

fn agreement() -> Agreement {
    drained().judge().unwrap()
}

#[test]
fn no_certificate_before_the_full_quiet_period() {
    let mut clock = QuietClock::new(QUIET_TICKS, TICK_MS);
    assert_eq!(
        QUIET_TICKS, 100,
        "rule: the default quiet period is 100 ticks (§10.2)"
    );
    for tick in 0..QUIET_TICKS {
        let certified = clock.observe(at_tick(tick), &Ok(agreement()));
        assert!(
            certified.is_none(),
            "rule: lookup before the full {QUIET_TICKS}-tick quiet period is refused; tick={tick}"
        );
    }
    let certified = clock
        .observe(at_tick(QUIET_TICKS), &Ok(agreement()))
        .expect("rule: the quiet period is complete at 100 ticks");
    assert_eq!(certified.quiet_ticks(), QUIET_TICKS);
    assert_eq!(certified.since(), at_tick(0));
    assert_eq!(certified.agreement(), agreement());
}

#[test]
fn a_broken_look_restarts_the_quiet_period() {
    let mut clock = QuietClock::new(QUIET_TICKS, TICK_MS);
    assert!(clock.observe(at_tick(0), &Ok(agreement())).is_none());
    assert!(
        clock
            .observe(at_tick(60), &Err(Uncertified::NoLeader))
            .is_none(),
        "rule: a look that breaks the drain certifies nothing"
    );
    assert!(
        clock.holding().is_none(),
        "rule: a broken look forgets the period"
    );
    assert!(clock.observe(at_tick(61), &Ok(agreement())).is_none());
    assert!(
        clock.observe(at_tick(150), &Ok(agreement())).is_none(),
        "rule: 89 quiet ticks since the restart are not 100"
    );
    assert!(clock.observe(at_tick(161), &Ok(agreement())).is_some());
}

#[test]
fn a_changed_agreement_restarts_the_quiet_period() {
    let mut clock = QuietClock::new(QUIET_TICKS, TICK_MS);
    assert!(clock.observe(at_tick(0), &Ok(agreement())).is_none());
    let other = Agreement {
        digest: digest_after_put("b"),
        ..agreement()
    };
    assert!(clock.observe(at_tick(90), &Ok(other)).is_none());
    assert!(
        clock.observe(at_tick(100), &Ok(other)).is_none(),
        "rule: the new agreement has held only 10 ticks"
    );
    assert!(clock.observe(at_tick(190), &Ok(other)).is_some());
}
