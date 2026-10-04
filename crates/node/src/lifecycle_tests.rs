//! W14.3's tests of the read path and the request lifecycle (PLAN.md §7.9, F-2, F-9, A-2,
//! C-3, C-4), through the controller: GET and the status lookup from the applied prefix,
//! reads during a sync, follower reads, refusals when not ready, the published view
//! observed once per applied entry, and writes routed to the write path.
#![cfg(test)]

// check: NODE-READ-PATH

use types::{
    AttemptId, ClientOp, Config, Decision, Effect, EntryIndex, HardState, Input, Lifecycle,
    LogIndex, NodeId, NotReadyLifecycle, Observation, Refusal, Response, Role, ServedBy, StorageOp,
    Term, TerminalResult, Version,
};

use crate::RequestLifecycle;
use crate::test_support::{
    INCARNATION, Member, config, genesis_journal, key, put, put_entry, request, value, write_frame,
};

fn lone() -> (Config, NodeId) {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    (config, id)
}

/// A lone voter that leads, with the real request lifecycle.
fn leader() -> (Config, Member<RequestLifecycle>) {
    let (config, id) = lone();
    let mut member = Member::fresh_lifecycle(&config, id);
    member.elect(100);
    (config, member)
}

fn get(k: &str) -> ClientOp {
    ClientOp::Get { key: key(k) }
}

/// Steps client attempt `n` and returns only its response.
fn ask(member: &mut Member<RequestLifecycle>, n: u64, op: ClientOp) -> Option<Response> {
    let effects = member.step(Input::Client {
        attempt: AttemptId::new(n),
        op,
    });
    response_to(&effects, n)
}

fn response_to(effects: &[Effect], n: u64) -> Option<Response> {
    effects.iter().find_map(|effect| match effect {
        Effect::Respond { attempt, response } if attempt.get() == n => Some(response.clone()),
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

/// Writes `k = v` with request ID `n` through the lone leader, and waits for its apply.
fn commit_put(member: &mut Member<RequestLifecycle>, n: u8, k: &str, v: &str) -> LogIndex {
    member.step(Input::Client {
        attempt: AttemptId::new(1_000 + u64::from(n)),
        op: put(n, k, v),
    });
    while member.complete_sync() {}
    member
        .ctl
        .state()
        .history()
        .get(&request(n))
        .map(|record| record.first_index().index())
        .expect("fixture: the write applied")
}

fn served(member: &Member<RequestLifecycle>) -> ServedBy {
    ServedBy {
        node: member.id,
        applied: member.ctl.state().applied_index(),
    }
}

#[test]
fn a_get_answers_the_value_and_version_at_the_applied_prefix_with_served_by() {
    let (_, mut member) = leader();
    let index = commit_put(&mut member, 1, "k", "v");
    let served_by = served(&member);
    assert_eq!(
        (
            ask(&mut member, 1, get("k")),
            ask(&mut member, 2, get("absent"))
        ),
        (
            Some(Response::Found {
                value: value("v"),
                version: Version::at(index).unwrap(),
                served_by,
            }),
            Some(Response::NotFound { served_by }),
        ),
        "rule: GET answers the value and its version (the write's index), or not_found, \
         with served_by naming this node and its applied index, in one turn (F-2, §7.9)"
    );
}

#[test]
fn a_read_during_an_outstanding_sync_shows_only_the_applied_prefix() {
    let (_, mut member) = leader();
    commit_put(&mut member, 1, "a", "old");
    let before = served(&member);
    member.step(Input::Client {
        attempt: AttemptId::new(50),
        op: put(2, "a", "new"),
    });
    assert!(
        member.ctl.outstanding_sync().is_some(),
        "fixture: the second write's entry is appended and waits for its sync"
    );
    let during = ask(&mut member, 51, get("a"));
    assert_eq!(
        during,
        Some(Response::Found {
            value: value("old"),
            version: Version::at(before.applied).unwrap(),
            served_by: before,
        }),
        "rule: a read is answered while a sync is outstanding, from the applied prefix only: \
         never the unsynced, uncommitted entry (C-3, §7.4 step 3)"
    );
    while member.complete_sync() {}
    let after = ask(&mut member, 52, get("a"));
    let Some(Response::Found {
        value: now,
        served_by,
        ..
    }) = after
    else {
        panic!("rule: the key is found after the second write applies; got {after:?}");
    };
    assert!(
        now == value("new") && served_by.applied > before.applied,
        "rule: once the entry applied, reads show it at a later prefix; served_by={served_by:?}"
    );
}

#[test]
fn the_status_lookup_answers_known_or_unknown_from_the_history() {
    let (_, mut member) = leader();
    let index = commit_put(&mut member, 7, "k", "v");
    let served_by = served(&member);
    assert_eq!(
        (
            ask(
                &mut member,
                1,
                ClientOp::Status {
                    request_id: request(7)
                }
            ),
            ask(
                &mut member,
                2,
                ClientOp::Status {
                    request_id: request(8)
                }
            )
        ),
        (
            Some(Response::Status {
                result: Some(TerminalResult::executed_at(
                    EntryIndex::new(index.get()).unwrap(),
                    Decision::Set
                )),
                served_by,
            }),
            Some(Response::Status {
                result: None,
                served_by,
            }),
        ),
        "rule: a status lookup answers the stored terminal result if this node applied the \
         request, unknown otherwise, with served_by; it proposes nothing (F-9)"
    );
}

#[test]
fn a_node_that_is_not_ready_answers_recovering_and_a_stopping_one_unavailable() {
    let (config, id) = lone();
    let mut failed = Member::fresh_lifecycle(&config, id);
    failed.step(Input::StorageFailed {
        op: StorageOp::Append,
        incarnation: INCARNATION,
    });
    let recovering = Some(Response::Refused(Refusal::Recovering {
        lifecycle: NotReadyLifecycle::Failed,
    }));
    let mut stopping = Member::fresh_lifecycle(&config, id);
    stopping.step(Input::Shutdown);
    assert_eq!(
        (
            ask(&mut failed, 1, get("k")),
            ask(
                &mut failed,
                2,
                ClientOp::Status {
                    request_id: request(1)
                }
            ),
            ask(&mut stopping, 3, get("k")),
        ),
        (
            recovering.clone(),
            recovering,
            Some(Response::Refused(Refusal::Unavailable)),
        ),
        "rule: a node that is not Ready answers reads recovering, never an empty or partial \
         state (A-2, §7.9); a stopping node answers unavailable"
    );
}

#[test]
fn every_applied_entry_is_observed_once_before_what_it_answers() {
    let (_, mut member) = leader();
    commit_put(&mut member, 1, "a", "v");
    commit_put(&mut member, 2, "b", "v");
    let observed: Vec<LogIndex> = member
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Observe(Observation::Applied { index }) => Some(*index),
            Effect::Observe(Observation::Responded { .. })
            | Effect::Respond { .. }
            | Effect::Append { .. }
            | Effect::Truncate { .. }
            | Effect::Sync { .. }
            | Effect::CreateJournal
            | Effect::SyncDir { .. }
            | Effect::Send { .. }
            | Effect::Log(_) => None,
        })
        .collect();
    let applied = member.ctl.state().applied_index().get();
    assert_eq!(
        observed,
        (1..=applied)
            .map(|index| LogIndex::new(index).unwrap())
            .collect::<Vec<_>>(),
        "rule: the published view's advance is observed once per applied entry, the \
         election no-op included, in index order (C-3, C-4)"
    );
    let first_write = member
        .ctl
        .state()
        .history()
        .get(&request(1))
        .unwrap()
        .first_index()
        .index();
    let observe_at = member
        .effects
        .iter()
        .position(|effect| effect == &Effect::Observe(Observation::Applied { index: first_write }));
    let answer_at = member.effects.iter().position(|effect| match effect {
        Effect::Respond { attempt, .. } => attempt.get() == 1_001,
        Effect::Append { .. }
        | Effect::Truncate { .. }
        | Effect::Sync { .. }
        | Effect::CreateJournal
        | Effect::SyncDir { .. }
        | Effect::Send { .. }
        | Effect::Log(_)
        | Effect::Observe(_) => false,
    });
    assert!(
        observe_at < answer_at && observe_at.is_some(),
        "rule: an entry's observation precedes the answer its application gives; \
         observed at {observe_at:?}, answered at {answer_at:?}"
    );
}

#[test]
fn a_restarted_node_observes_its_replayed_prefix() {
    let (config, id) = lone();
    let journal = genesis_journal(&config, id);
    write_frame(
        &config,
        id,
        &journal,
        vec![put_entry(1, 1, 1, "a", "x"), put_entry(2, 1, 2, "b", "y")],
        HardState {
            term: Term::new(1).unwrap(),
            vote: Some(id),
            commit: LogIndex::new(2).unwrap(),
        },
    );
    let member = Member::start_with(&config, id, journal, 3, RequestLifecycle::new(&config, id));
    assert!(
        member
            .effects
            .contains(&Effect::Observe(Observation::Applied {
                index: LogIndex::new(2).unwrap()
            })),
        "rule: a node that starts from a journal observes the prefix it replayed: its \
         published view starts there (§7.7 step 5, C-4)"
    );
}

#[test]
fn reported_prefixes_never_go_back_within_an_incarnation() {
    let (_, mut member) = leader();
    let mut n = 0;
    for round in 0..5_u8 {
        commit_put(&mut member, round + 1, "k", "v");
        for _ in 0..3 {
            n += 1;
            ask(&mut member, n, get("k"));
        }
    }
    let reported: Vec<LogIndex> = member
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Respond { response, .. } => match response {
                Response::Found { served_by, .. }
                | Response::NotFound { served_by }
                | Response::Status { served_by, .. }
                | Response::Written { served_by, .. } => Some(served_by.applied),
                Response::Conflict { .. } | Response::Page(_) | Response::Refused(_) => None,
            },
            Effect::Append { .. }
            | Effect::Truncate { .. }
            | Effect::Sync { .. }
            | Effect::CreateJournal
            | Effect::SyncDir { .. }
            | Effect::Send { .. }
            | Effect::Log(_)
            | Effect::Observe(_) => None,
        })
        .collect();
    assert!(
        reported.len() >= 20 && reported.windows(2).all(|pair| pair[0] <= pair[1]),
        "rule: the applied index a node reports never goes back, in response-generation \
         order (C-4); reported={reported:?}"
    );
}

#[test]
fn a_follower_serves_reads_from_its_own_applied_prefix() {
    let config = config("three-voters");
    let mut members: Vec<Member<RequestLifecycle>> = config
        .voters()
        .map(|id| Member::fresh_lifecycle(&config, id))
        .collect();
    let run = |members: &mut Vec<Member<RequestLifecycle>>, ticks: bool| {
        for member in members.iter_mut() {
            if ticks {
                member.step(Input::Tick);
            }
            while member.complete_sync() {}
        }
        for from in 0..members.len() {
            let outbox: Vec<(NodeId, Vec<u8>)> = members[from].outbox.drain(..).collect();
            let sender = members[from].id;
            for (to, bytes) in outbox {
                if let Some(target) = members.iter_mut().find(|member| member.id == to) {
                    target.step(Input::Peer {
                        from: sender,
                        bytes,
                    });
                    while target.complete_sync() {}
                }
            }
        }
    };
    let mut rounds = 0;
    while !members
        .iter()
        .any(|member| member.ctl.status().role == Role::Leader)
    {
        assert!(rounds < 300, "fixture: a leader within 300 rounds");
        run(&mut members, true);
        rounds += 1;
    }
    let leader = members
        .iter()
        .position(|member| member.ctl.status().role == Role::Leader)
        .unwrap();
    members[leader].step(Input::Client {
        attempt: AttemptId::new(1),
        op: put(1, "k", "v"),
    });
    for _ in 0..10 {
        run(&mut members, true);
    }
    let follower = (0..members.len()).find(|index| *index != leader).unwrap();
    let served_by = served(&members[follower]);
    let answer = ask(&mut members[follower], 2, get("k"));
    assert!(
        served_by.applied > LogIndex::ZERO
            && answer
                == Some(Response::Found {
                    value: value("v"),
                    version: Version::at(
                        members[follower]
                            .ctl
                            .state()
                            .history()
                            .get(&request(1))
                            .unwrap()
                            .first_index()
                            .index()
                    )
                    .unwrap(),
                    served_by,
                }),
        "rule: a follower answers a GET locally from its own applied prefix, with no leader \
         round trip (A-2, §7.9); got {answer:?}"
    );
    assert_eq!(
        members[follower].ctl.lifecycle(),
        Lifecycle::Ready,
        "fixture: the follower is Ready"
    );
}
