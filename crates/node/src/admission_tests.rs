//! W29.3's end-to-end tests of history admission (PLAN.md §7.11, §8.4, DM-7, A-5, F-10) and of
//! the live-quota terminal flow (DM-6), through controllers running the real request lifecycle
//! on a three-voter cluster under the small-limits config. History admission:
//! - it reads committed replicated state at the leader: the commit index against
//!   `max_log_entries`, and the state machine's committed-command-bytes counter, at the applied
//!   index, against `max_history_bytes`;
//! - at either ceiling the leader refuses a new write before proposal with `history_full`,
//!   which is `not_submitted` and appends nothing;
//! - reads, status lookups and retries of executed requests keep working, a follower still
//!   redirects (the leader check comes first, F-7), and election no-ops are never refused.
//!
//! A live-quota refusal is the opposite: a terminal `capacity_exceeded`, recorded and
//! replayed, never executed again.
//!
//! The byte ceiling is reached by watching the production counter, never by an independent
//! byte count: the canonical command format the model would need is still open (br-camk).
#![cfg(test)]

// check: HISTORY-ADMISSION

use types::{
    ClientOp, Decision, HistoryLimit, LiveStateLimit, Mutation, Refusal, RequestState, Response,
};

use crate::test_support::{Cluster, config, key, put, request};

/// The write that request number `n` makes while the history fills: a 60-byte value for one
/// of eight keys, so the live state stays inside DM-6 while the command bytes grow.
fn filler(n: u8) -> ClientOp {
    put(n, &format!("k{}", n % 8), &"v".repeat(60))
}

/// A small-limits cluster whose leader's committed command bytes have reached
/// `max_history_bytes`, written with requests `1..=n`; returns the cluster, its leader and
/// `n`.
fn at_the_byte_ceiling() -> (Cluster, usize, u8) {
    let config = config("small-limits");
    let mut cluster = Cluster::elected("small-limits");
    let leader = cluster.leader().expect("fixture: a leader");
    let mut n: u8 = 0;
    while cluster.members[leader]
        .ctl
        .state()
        .committed_command_bytes()
        < config.max_history_bytes()
    {
        n += 1;
        cluster.write(u64::from(n), request(n), filler(n));
    }
    assert!(
        cluster.members[leader].ctl.status().commit < config.max_log_entries(),
        "fixture: the byte ceiling is reached below max_log_entries, so the byte half decides"
    );
    (cluster, leader, n)
}

/// What member `at` answered attempt `attempt` with, if anything.
fn answer(cluster: &Cluster, at: usize, attempt: u64) -> Option<Response> {
    cluster.answer(at, attempt)
}

#[test]
fn at_the_byte_ceiling_new_writes_are_history_full_and_everything_else_keeps_working() {
    let (mut cluster, leader, n) = at_the_byte_ceiling();
    let leader_id = cluster.members[leader].id;
    let follower = (0..cluster.members.len()).find(|at| *at != leader).unwrap();
    let last = cluster.members[leader].ctl.status().last;
    let full = Some(Response::Refused(Refusal::HistoryFull {
        limit: HistoryLimit::HistoryBytes,
    }));
    // New writes: a PUT and a DELETE at the leader, a PUT at a follower.
    cluster.send(leader, 1_000, put(200, "k0", "w"));
    cluster.send(
        leader,
        1_001,
        ClientOp::Write {
            request_id: request(201),
            mutation: Mutation::Delete { key: key("k0") },
        },
    );
    cluster.send(follower, 1_002, put(202, "k0", "w"));
    let appended = cluster.members[leader].ctl.status().last;
    // What keeps working: a retry of an executed request, a status lookup and a GET.
    cluster.send(leader, 1_003, filler(1));
    cluster.send(
        leader,
        1_004,
        ClientOp::Status {
            request_id: request(n),
        },
    );
    cluster.send(leader, 1_005, ClientOp::Get { key: key("k1") });
    let retried = match answer(&cluster, leader, 1_003) {
        Some(Response::Written { replayed, .. }) => replayed,
        Some(
            Response::Conflict { .. }
            | Response::Found { .. }
            | Response::NotFound { .. }
            | Response::Page(_)
            | Response::Status { .. }
            | Response::Refused(_),
        )
        | None => false,
    };
    let looked_up = match answer(&cluster, leader, 1_004) {
        Some(Response::Status { state, .. }) => match state {
            RequestState::Known(_) => true,
            RequestState::Unknown => false,
        },
        Some(
            Response::Written { .. }
            | Response::Conflict { .. }
            | Response::Found { .. }
            | Response::NotFound { .. }
            | Response::Page(_)
            | Response::Refused(_),
        )
        | None => false,
    };
    let read = match answer(&cluster, leader, 1_005) {
        Some(Response::Found { .. }) => true,
        Some(
            Response::Written { .. }
            | Response::Conflict { .. }
            | Response::NotFound { .. }
            | Response::Page(_)
            | Response::Status { .. }
            | Response::Refused(_),
        )
        | None => false,
    };
    assert_eq!(
        (
            answer(&cluster, leader, 1_000),
            answer(&cluster, leader, 1_001),
            answer(&cluster, follower, 1_002),
            appended,
            (retried, looked_up, read),
        ),
        (
            full.clone(),
            full,
            Some(Response::Refused(Refusal::NotLeader { leader: leader_id })),
            last,
            (true, true, true),
        ),
        "rule: once the committed command bytes at the leader's applied index reach \
         max_history_bytes, the leader refuses a new PUT or DELETE before proposal with \
         history_full naming max_history_bytes, not submitted, appending nothing; a follower \
         still redirects (the leader check comes first); a retry of an executed request \
         replays, and status lookups and GETs keep working (DM-7, F-7, §8.4)"
    );
    assert!(
        !Refusal::HistoryFull {
            limit: HistoryLimit::HistoryBytes
        }
        .submitted(),
        "rule: history_full is not_submitted (A-5, §8.4)"
    );
}

#[test]
fn election_no_ops_are_never_refused_at_the_history_ceiling() {
    let (mut cluster, old, _) = at_the_byte_ceiling();
    let old_id = cluster.members[old].id;
    let commit_before = cluster.members[old].ctl.status().commit;
    cluster.isolate(old_id);
    cluster.until(300, "a new leader whose no-op committed", |cluster| {
        cluster
            .leader_other_than(old)
            .is_some_and(|new| cluster.members[new].ctl.status().commit > commit_before)
    });
    let new = cluster
        .leader_other_than(old)
        .expect("fixture: a new leader");
    cluster.send(new, 1_000, put(200, "k0", "w"));
    assert_eq!(
        (
            cluster.members[new].ctl.status().commit > commit_before,
            answer(&cluster, new, 1_000)
        ),
        (
            true,
            Some(Response::Refused(Refusal::HistoryFull {
                limit: HistoryLimit::HistoryBytes
            }))
        ),
        "rule: protocol entries (a new leader's election no-op) are never refused, so a new \
         leader commits past the history ceiling, while new writes there are still \
         history_full (DM-7: soft ceilings on committed state)"
    );
}

#[test]
fn a_live_quota_refusal_is_a_terminal_result_end_to_end() {
    // small-limits holds 8 live keys: the ninth new key is capacity_exceeded, a committed
    // decision, unlike history_full.
    let mut cluster = Cluster::elected("small-limits");
    for n in 1..=8_u8 {
        cluster.write(u64::from(n), request(n), put(n, &format!("k{n}"), "v"));
    }
    cluster.write(9, request(9), put(9, "k9", "v"));
    let leader = cluster.leader().expect("fixture: a leader");
    let refused = cluster.members[leader]
        .ctl
        .state()
        .history()
        .get(&request(9))
        .map(statemachine::Record::result)
        .expect("fixture: request 9 applied");
    // Room appears: k1 is deleted. A retry of request 9 still replays the refusal; the same
    // write under a new request ID now executes.
    cluster.write(
        10,
        request(10),
        ClientOp::Write {
            request_id: request(10),
            mutation: Mutation::Delete { key: key("k1") },
        },
    );
    cluster.send(leader, 11, put(9, "k9", "v"));
    cluster.write(12, request(12), put(12, "k9", "v"));
    let decision_of = |attempt: u64| match answer(&cluster, leader, attempt) {
        Some(Response::Written {
            result, replayed, ..
        }) => Some((result.decision(), replayed)),
        Some(
            Response::Conflict { .. }
            | Response::Found { .. }
            | Response::NotFound { .. }
            | Response::Page(_)
            | Response::Status { .. }
            | Response::Refused(_),
        )
        | None => None,
    };
    assert_eq!(
        (
            refused.decision(),
            decision_of(9),
            decision_of(11),
            decision_of(12),
        ),
        (
            Decision::CapacityExceeded {
                limit: LiveStateLimit::Keys
            },
            Some((
                Decision::CapacityExceeded {
                    limit: LiveStateLimit::Keys
                },
                false
            )),
            Some((
                Decision::CapacityExceeded {
                    limit: LiveStateLimit::Keys
                },
                true
            )),
            Some((Decision::Set, false)),
        ),
        "rule: a write over the live quota is a terminal capacity_exceeded, answered as a \
         written result (not a refusal), recorded in the history and replayed to a retry even \
         after room appears; only a new request ID executes the write again (DM-6, §8.3, \
         F-10)"
    );
}
