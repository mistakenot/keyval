//! W29.1's tests of the request-status lookup (PLAN.md §7.9, §8.2, §8.3, F-9, P8), through
//! the controller with the real request lifecycle:
//! - a request this node has applied is `Known` with its original terminal result, whatever it
//!   decided, and later attempts with its ID never change it;
//! - one it has not applied is `Unknown`, which never means it did not happen: the same request
//!   turns `Known` once its entry applies, on a lone leader after its sync and on a lagging
//!   follower once it catches up;
//! - the lookup is answered in one turn with `served_by`, and proposes and records nothing;
//! - a node that is not `Ready` refuses it rather than answer either way.
#![cfg(test)]

// check: REQUEST-STATUS

use statemachine::Record;
use types::{
    AttemptId, ClientOp, Decision, Effect, EntryIndex, ExpectedVersion, Input, LiveStateLimit,
    LogIndex, Mutation, NotReadyLifecycle, Refusal, RequestState, Response, ServedBy, StorageOp,
    TerminalResult, Version,
};

use crate::RequestLifecycle;
use crate::test_support::{
    Cluster, INCARNATION, Member, answered, config, key, put, request, value,
};

/// A lone voter that leads, with the real request lifecycle.
fn leader() -> Member<RequestLifecycle> {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    let mut member = Member::fresh_lifecycle(&config, id);
    member.elect(100);
    member
}

fn status(n: u8) -> ClientOp {
    ClientOp::Status {
        request_id: request(n),
    }
}

/// A write of `mutation` under request ID `n`.
fn write(n: u8, mutation: Mutation) -> ClientOp {
    ClientOp::Write {
        request_id: request(n),
        mutation,
    }
}

fn delete(n: u8, k: &str) -> ClientOp {
    write(n, Mutation::Delete { key: key(k) })
}

fn cas(n: u8, k: &str, expected: ExpectedVersion, v: &str) -> ClientOp {
    write(
        n,
        Mutation::Cas {
            key: key(k),
            expected,
            value: value(v),
        },
    )
}

/// Steps client attempt `attempt` and returns every effect of the step.
fn step(member: &mut Member<RequestLifecycle>, attempt: u64, op: ClientOp) -> Vec<Effect> {
    member.step(Input::Client {
        attempt: AttemptId::new(attempt),
        op,
    })
}

/// Steps client attempt `attempt` and returns its response.
fn ask(member: &mut Member<RequestLifecycle>, attempt: u64, op: ClientOp) -> Option<Response> {
    response_to(&step(member, attempt, op), attempt)
}

fn response_to(effects: &[Effect], attempt: u64) -> Option<Response> {
    effects.iter().find_map(|effect| match effect {
        Effect::Respond {
            attempt: to,
            response,
        } if to.get() == attempt => Some(response.clone()),
        Effect::Respond { .. }
        | Effect::Append { .. }
        | Effect::Truncate { .. }
        | Effect::Sync { .. }
        | Effect::CreateJournal
        | Effect::SyncDir { .. }
        | Effect::Send { .. }
        | Effect::Log(_)
        | Effect::Observe(_) => None,
    })
}

/// The entry at which `member` first executed request `n`, if it has applied it.
fn first_index(member: &Member<RequestLifecycle>, n: u8) -> Option<EntryIndex> {
    member
        .ctl
        .state()
        .history()
        .get(&request(n))
        .map(Record::first_index)
}

/// Writes `op` (request `n`) through the lone leader and waits for its apply; returns the
/// entry it executed at.
fn commit(member: &mut Member<RequestLifecycle>, n: u8, op: ClientOp) -> EntryIndex {
    step(member, 1_000 + u64::from(n), op);
    while member.complete_sync() {}
    first_index(member, n).expect("fixture: the write applied")
}

fn served(member: &Member<RequestLifecycle>) -> ServedBy {
    ServedBy {
        node: member.id,
        applied: member.ctl.state().applied_index(),
    }
}

/// The answer to a lookup of a request this node applied at `index`, deciding `decision`.
fn known(index: EntryIndex, decision: Decision, served_by: ServedBy) -> Response {
    Response::Status {
        state: RequestState::Known(TerminalResult::executed_at(index, decision)),
        served_by,
    }
}

/// The answer to a lookup of a request this node has not applied.
const fn unknown(served_by: ServedBy) -> Response {
    Response::Status {
        state: RequestState::Unknown,
        served_by,
    }
}

fn version_at(index: EntryIndex) -> Version {
    Version::at(index.index()).unwrap()
}

#[test]
fn every_stored_decision_is_answered_known_with_its_original_result() {
    let mut member = leader();
    let set = commit(&mut member, 1, put(1, "k", "v"));
    let deleted = commit(&mut member, 2, delete(2, "k"));
    let deleted_absent = commit(&mut member, 3, delete(3, "k"));
    let created = commit(&mut member, 4, cas(4, "k", ExpectedVersion::Absent, "w"));
    let live_mismatch = commit(&mut member, 5, cas(5, "k", ExpectedVersion::Absent, "x"));
    let absent_mismatch = commit(
        &mut member,
        6,
        cas(6, "gone", ExpectedVersion::Version(version_at(set)), "y"),
    );
    let served_by = served(&member);
    let answers: Vec<Option<Response>> = (1..=6)
        .map(|n| ask(&mut member, u64::from(n), status(n)))
        .collect();
    assert_eq!(
        answers,
        vec![
            Some(known(set, Decision::Set, served_by)),
            Some(known(
                deleted,
                Decision::Deleted { existed: true },
                served_by
            )),
            Some(known(
                deleted_absent,
                Decision::Deleted { existed: false },
                served_by
            )),
            Some(known(created, Decision::Set, served_by)),
            Some(known(
                live_mismatch,
                Decision::VersionMismatch {
                    current: Some(version_at(created))
                },
                served_by
            )),
            Some(known(
                absent_mismatch,
                Decision::VersionMismatch { current: None },
                served_by
            )),
        ],
        "rule: a status lookup answers known with the request's original terminal result, \
         whatever it decided: a set, a DELETE that existed or not, a version_mismatch naming \
         the current version or its absence (F-9, §8.3)"
    );
}

#[test]
fn a_capacity_refusal_is_answered_known_on_every_node_like_a_success() {
    // small-limits holds at most 8 live keys, so the ninth new key is a terminal
    // capacity_exceeded; every node that applied it reports it as known (§8.3).
    let mut cluster = Cluster::elected("small-limits");
    for n in 1..=9_u8 {
        cluster.write(u64::from(n), request(n), put(n, &format!("k{n}"), "v"));
    }
    let leader = cluster.leader().expect("fixture: a leader");
    let refused = first_index(&cluster.members[leader], 9).expect("fixture: request 9 applied");
    let expected: Vec<Option<Response>> = cluster
        .members
        .iter()
        .map(|member| {
            Some(known(
                refused,
                Decision::CapacityExceeded {
                    limit: LiveStateLimit::Keys,
                },
                served(member),
            ))
        })
        .collect();
    let answers: Vec<Option<Response>> = cluster
        .members
        .iter_mut()
        .map(|member| ask(member, 100, status(9)))
        .collect();
    assert_eq!(
        answers, expected,
        "rule: a live-quota refusal is a terminal result like a success, and a status lookup \
         on any node that applied it answers it known (§8.3, DM-6)"
    );
}

#[test]
fn unknown_while_the_write_awaits_its_sync_then_known_once_it_applies() {
    let mut member = leader();
    // Request 7 is appended, and its sync is outstanding: not applied yet.
    step(&mut member, 1, put(7, "k", "v"));
    let pending = served(&member);
    let before = ask(&mut member, 2, status(7));
    while member.complete_sync() {}
    let index = first_index(&member, 7).expect("fixture: request 7 applied after its sync");
    let applied = served(&member);
    let after = ask(&mut member, 3, status(7));
    assert_eq!(
        (before, after),
        (
            Some(unknown(pending)),
            Some(known(index, Decision::Set, applied))
        ),
        "rule: unknown never means \"did not happen\" (P8): a lookup answered while the \
         write's sync is outstanding is unknown, with the applied prefix it reflects, and the \
         same request is known once its entry applies (F-9, §7.9)"
    );
}

#[test]
fn a_lagging_follower_answers_unknown_for_a_committed_request_until_it_applies_it() {
    let mut cluster = Cluster::elected("three-voters");
    let leader = cluster.leader().expect("fixture: a leader");
    let lagging = (0..cluster.members.len())
        .find(|index| *index != leader)
        .unwrap();
    let lagging_id = cluster.members[lagging].id;
    cluster.isolate(lagging_id);
    cluster.write(1, request(7), put(7, "k", "v"));
    let leader = cluster
        .leader()
        .expect("fixture: the leader keeps its majority");
    let original = first_index(&cluster.members[leader], 7).expect("fixture: committed");
    let behind = served(&cluster.members[lagging]);
    let while_behind = ask(&mut cluster.members[lagging], 2, status(7));
    cluster.heal();
    cluster.settle(request(7));
    let caught_up = served(&cluster.members[lagging]);
    let after = ask(&mut cluster.members[lagging], 3, status(7));
    assert_eq!(
        (while_behind, after),
        (
            Some(unknown(behind)),
            Some(known(original, Decision::Set, caught_up))
        ),
        "rule: a follower that has not applied a committed request answers unknown from its \
         own applied prefix, which is not evidence that it did not happen (P8); once it \
         catches up it answers the original result the leader recorded (F-9, A-2)"
    );
}

#[test]
fn a_status_lookup_never_proposes_and_records_nothing() {
    let mut member = leader();
    let id = member.id;
    let last = member.ctl.status().last;
    let served_by = served(&member);
    let effects = step(&mut member, 1, status(9));
    let unchanged = (member.ctl.status().last, member.ctl.state().history().len());
    // Request 9 then executes as a first execution: the lookup left no trace of the ID.
    step(&mut member, 2, put(9, "k", "v"));
    while member.complete_sync() {}
    let index = first_index(&member, 9).expect("fixture: request 9 applied");
    let written = answered(&member, 2);
    assert_eq!(
        (effects, unchanged, written),
        (
            vec![Effect::Respond {
                attempt: AttemptId::new(1),
                response: Response::Status {
                    state: RequestState::Unknown,
                    served_by,
                },
            }],
            (last, 0),
            Some(Response::Written {
                result: TerminalResult::executed_at(index, Decision::Set),
                replayed: false,
                served_by: ServedBy {
                    node: id,
                    applied: index.index(),
                },
            }),
        ),
        "rule: a status lookup is answered in its own turn with nothing but its response: it \
         never proposes, appends or records anything, so the ID it asked about later executes \
         as a first execution (F-9, §7.9)"
    );
}

#[test]
fn a_retry_or_another_command_under_the_id_never_changes_the_known_result() {
    let mut member = leader();
    let original = commit(&mut member, 7, put(7, "k", "v"));
    // A retry replays the result; another command under the ID is request_id_conflict.
    step(&mut member, 2, put(7, "k", "v"));
    step(&mut member, 3, put(7, "k", "other"));
    while member.complete_sync() {}
    let served_by = served(&member);
    assert_eq!(
        (ask(&mut member, 4, status(7)), member.ctl.status().last),
        (
            Some(known(original, Decision::Set, served_by)),
            LogIndex::new(original.get()).unwrap()
        ),
        "rule: the status of a request ID is its first execution's result: retries and \
         conflicting commands under the ID never change it (F-9, F-6, DM-5)"
    );
}

#[test]
fn a_node_that_is_not_ready_refuses_the_lookup_rather_than_answer_either_way() {
    let mut failed = leader();
    commit(&mut failed, 7, put(7, "k", "v"));
    failed.step(Input::StorageFailed {
        op: StorageOp::Append,
        incarnation: INCARNATION,
    });
    let mut stopping = leader();
    commit(&mut stopping, 7, put(7, "k", "v"));
    stopping.step(Input::Shutdown);
    let recovering = Some(Response::Refused(Refusal::Recovering {
        lifecycle: NotReadyLifecycle::Failed,
    }));
    let unavailable = Some(Response::Refused(Refusal::Unavailable));
    assert_eq!(
        (
            ask(&mut failed, 1, status(7)),
            ask(&mut failed, 2, status(8)),
            ask(&mut stopping, 3, status(7)),
            ask(&mut stopping, 4, status(8)),
        ),
        (
            recovering.clone(),
            recovering,
            unavailable.clone(),
            unavailable
        ),
        "rule: a node that is not Ready answers recovering, and a stopping one unavailable, \
         rather than known or unknown: a lookup never comes from a state the node may not \
         serve (A-2, P8)"
    );
}
