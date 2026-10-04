//! W29.2's tests of the same-request-ID race (PLAN.md §7.8, §7.5 step 3, F-6, C-6, A-5), across
//! nodes and leader changes, through controllers running the real request lifecycle. Two
//! commands under one request ID never both execute:
//! - **One original, one conflict:** whichever entry carrying the ID applies first is the
//!   original, the other command is `request_id_conflict` naming it, and every node agrees.
//!   That holds on one leader in either arrival order, across a leader change that keeps a
//!   replicated command, and across one that loses it.
//! - **Nothing invented:** an attempt whose proposal is lost answers non-terminally:
//!   `unknown_outcome` if its command was submitted, `busy` or a redirect if it never was. It
//!   never answers a conflict before the original is known.
//! - **A seeded sweep:** two clients race and retry through partitions and leader changes, and
//!   every run that drives both commands to a terminal answer ends with exactly one original
//!   and one conflict.
#![cfg(test)]

// check: REQUEST-ID-RACE

use std::collections::BTreeMap;

use types::{ClientOp, Input, Refusal, Response};

use crate::RequestLifecycle;
use crate::test_support::{Cluster, Member, answered, config, key, put, request};

/// Random seeds of the sweep.
const SEEDS: u64 = 60;

/// Rounds a sweep run may take to give both clients a terminal answer.
const ROUNDS: u32 = 1_000;

/// The re-run command of the sweep.
const RERUN: &str = "./dev check REQUEST-ID-RACE --case \
                     race_tests::every_seeded_same_id_race_resolves_to_one_original_and_one_conflict";

/// Command "a" under request ID 1.
fn a() -> ClientOp {
    put(1, "k", "a")
}

/// Command "b" under the same request ID 1: another command.
fn b() -> ClientOp {
    put(1, "k", "b")
}

/// What an answer says, comparable across fixtures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Said {
    /// The command's own result: the first execution at `index`, `replayed` if recorded
    /// earlier.
    Written { index: u64, replayed: bool },
    /// `request_id_conflict`, naming the original's index.
    Conflict { original: u64 },
    /// A non-terminal refusal.
    Refused(Refusal),
    /// A read's answer, which no write gets.
    Read,
}

fn said(response: &Response) -> Said {
    match response {
        Response::Written {
            result, replayed, ..
        } => Said::Written {
            index: result.index().get(),
            replayed: *replayed,
        },
        Response::Conflict { original_index } => Said::Conflict {
            original: original_index.get(),
        },
        Response::Refused(refusal) => Said::Refused(*refusal),
        Response::Found { .. }
        | Response::NotFound { .. }
        | Response::Page(_)
        | Response::Status { .. } => Said::Read,
    }
}

/// What member `at` answered attempt `attempt`, if it has.
fn said_at(cluster: &Cluster, at: usize, attempt: u64) -> Option<Said> {
    cluster.answer(at, attempt).as_ref().map(said)
}

/// Request 1's original, which every node must agree on: the index of its first execution
/// and the value its command wrote ("a" or "b").
fn original(cluster: &Cluster) -> (u64, String) {
    let seen: Vec<(u64, Option<String>)> = cluster
        .members
        .iter()
        .map(|member| {
            let index = member
                .ctl
                .state()
                .history()
                .get(&request(1))
                .map_or(0, |record| record.first_index().get());
            let value = member
                .ctl
                .state()
                .get(&key("k"))
                .map(|stored| stored.value().as_str().to_owned());
            (index, value)
        })
        .collect();
    assert!(
        seen.windows(2).all(|pair| pair[0] == pair[1]) && seen[0].0 > 0,
        "rule: every node records the same original for the request ID, at the same index, \
         with the same command's effect (F-6, C-6); seen (index, value) per node: {seen:?}"
    );
    let (index, value) = seen[0].clone();
    (index, value.expect("fixture: the original set k"))
}

/// Whether `said` is a refusal that says nothing about the request's fate: `busy`, a redirect
/// or `unavailable` (never `unknown_outcome`, never terminal).
const fn never_submitted(said: Option<Said>) -> bool {
    match said {
        Some(Said::Refused(Refusal::Busy | Refusal::NotLeader { .. } | Refusal::Unavailable)) => {
            true
        }
        Some(
            Said::Refused(
                Refusal::UnknownOutcome | Refusal::Recovering { .. } | Refusal::HistoryFull { .. },
            )
            | Said::Written { .. }
            | Said::Conflict { .. }
            | Said::Read,
        )
        | None => false,
    }
}

#[test]
fn on_one_leader_the_command_admitted_first_is_the_original() {
    for (first, second, winner) in [(a(), b(), "a"), (b(), a(), "b")] {
        let mut cluster = Cluster::elected("three-voters");
        let leader = cluster.leader().expect("fixture: a leader");
        cluster.send(leader, 1, first);
        cluster.send(leader, 2, second);
        cluster.settle(request(1));
        let (index, value) = original(&cluster);
        assert_eq!(
            (
                value.as_str(),
                said_at(&cluster, leader, 1),
                said_at(&cluster, leader, 2)
            ),
            (
                winner,
                Some(Said::Written {
                    index,
                    replayed: false
                }),
                Some(Said::Conflict { original: index }),
            ),
            "rule: on one leader the command admitted first is proposed and becomes the \
             original; the other waits behind it, never proposed, and gets request_id_conflict \
             naming it once it applies (§7.8, §7.5 step 3); winner {winner:?}"
        );
    }
}

#[test]
fn a_command_replicated_before_a_leader_change_stays_the_original() {
    let mut cluster = Cluster::elected("three-voters");
    let old = cluster.leader().expect("fixture: a leader");
    let old_id = cluster.members[old].id;
    // The old leader still sends but hears nothing: "a" reaches the followers' logs and never
    // commits under it.
    cluster.deafen(old_id);
    cluster.send(old, 1, a());
    let a_at = cluster.members[old].ctl.status().last;
    cluster.until(10, "\"a\" in both followers' logs", |cluster| {
        (0..cluster.members.len())
            .filter(|at| *at != old)
            .all(|at| cluster.members[at].ctl.status().last >= a_at)
    });
    cluster.isolate(old_id);
    cluster.until(300, "a new leader", |cluster| {
        cluster.leader_other_than(old).is_some()
    });
    let new = cluster
        .leader_other_than(old)
        .expect("fixture: a new leader");
    // "b" reaches the new leader, whose log already holds "a".
    cluster.send(new, 2, b());
    cluster.heal();
    cluster.settle(request(1));
    cluster.send(new, 3, a());
    cluster.send(new, 4, b());
    let (index, value) = original(&cluster);
    let first = said_at(&cluster, old, 1);
    assert!(
        value == "a"
            && [
                Some(Said::Refused(Refusal::UnknownOutcome)),
                Some(Said::Written {
                    index,
                    replayed: false
                }),
            ]
            .contains(&first)
            && said_at(&cluster, new, 2) == Some(Said::Conflict { original: index })
            && said_at(&cluster, new, 3)
                == Some(Said::Written {
                    index,
                    replayed: true
                })
            && said_at(&cluster, new, 4) == Some(Said::Conflict { original: index }),
        "rule: a command already in the new leader's log commits ahead of one proposed there \
         later, so it stays the original; the later one is request_id_conflict naming it, a \
         retry of the original replays it, and the attempt at the deposed leader answers \
         unknown_outcome or its own result, never a conflict (§7.8, F-6, A-5); original \
         ({index}, {value:?}); answers {:?}",
        [
            first,
            said_at(&cluster, new, 2),
            said_at(&cluster, new, 3),
            said_at(&cluster, new, 4)
        ]
    );
}

#[test]
fn a_command_lost_with_its_leader_yields_to_the_one_proposed_at_the_next() {
    let mut cluster = Cluster::elected("three-voters");
    let old = cluster.leader().expect("fixture: a leader");
    let old_id = cluster.members[old].id;
    // "a" is appended at the isolated old leader alone; "b" waits behind it there.
    cluster.isolate(old_id);
    cluster.send(old, 1, a());
    cluster.send(old, 2, b());
    cluster.until(300, "a new leader", |cluster| {
        cluster.leader_other_than(old).is_some()
    });
    let new = cluster
        .leader_other_than(old)
        .expect("fixture: a new leader");
    // The client of "b" retries at the new leader.
    cluster.send(new, 3, b());
    cluster.until(300, "both attempts at the old leader answered", |cluster| {
        cluster.answer(old, 1).is_some() && cluster.answer(old, 2).is_some()
    });
    cluster.heal();
    cluster.settle(request(1));
    cluster.send(new, 4, a());
    let (index, value) = original(&cluster);
    let lost = (said_at(&cluster, old, 1), said_at(&cluster, old, 2));
    assert!(
        value == "b"
            && lost.0 == Some(Said::Refused(Refusal::UnknownOutcome))
            && never_submitted(lost.1)
            && said_at(&cluster, new, 3)
                == Some(Said::Written {
                    index,
                    replayed: false
                })
            && said_at(&cluster, new, 4) == Some(Said::Conflict { original: index }),
        "rule: a command lost with its leader never commits, so the command proposed at the \
         next leader is the original and a retry of the lost one is request_id_conflict naming \
         it; at the old leader the submitted attempt answers unknown_outcome and the one behind \
         it, never proposed, a non-terminal refusal (busy, not_leader or unavailable), never a \
         conflict (§7.8, F-6, A-5); original ({index}, {value:?}); old leader answered {lost:?}; \
         new leader answered {:?}",
        [said_at(&cluster, new, 3), said_at(&cluster, new, 4)]
    );
}

#[test]
fn a_waiter_behind_a_stalled_proposal_times_out_busy_and_never_sees_a_conflict() {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    let mut node: Member<RequestLifecycle> = Member::fresh_lifecycle(&config, id);
    node.elect(100);
    let deadline = config.write_deadline().get();
    // "a" is submitted and its sync is held, so it cannot commit; "b" waits behind it.
    for (attempt, op) in [(1, a()), (2, b())] {
        node.step(Input::Client {
            attempt: types::AttemptId::new(attempt),
            op,
        });
    }
    for _ in 0..deadline {
        node.step(Input::Tick);
    }
    let at_deadline = (
        answered(&node, 1).as_ref().map(said),
        answered(&node, 2).as_ref().map(said),
    );
    while node.complete_sync() {}
    for (attempt, op) in [(3, b()), (4, a())] {
        node.step(Input::Client {
            attempt: types::AttemptId::new(attempt),
            op,
        });
    }
    let index = node
        .ctl
        .state()
        .history()
        .get(&request(1))
        .map(|record| record.first_index().get())
        .expect("fixture: \"a\" applied once its sync completed");
    assert_eq!(
        (
            at_deadline,
            answered(&node, 3).as_ref().map(said),
            answered(&node, 4).as_ref().map(said)
        ),
        (
            (
                Some(Said::Refused(Refusal::UnknownOutcome)),
                Some(Said::Refused(Refusal::Busy))
            ),
            Some(Said::Conflict { original: index }),
            Some(Said::Written {
                index,
                replayed: true
            }),
        ),
        "rule: at its deadline the submitted attempt answers unknown_outcome and the \
         different-command attempt waiting behind it, never proposed, busy: neither answers a \
         conflict while the original is unknown; once \"a\" commits it is the original, so a \
         retry of \"b\" is request_id_conflict and one of \"a\" replays it (§7.8, A-5)"
    );
}

/// The `SplitMix64` stream the sweep draws its schedules from (P4: seeded, no RNG crate).
struct SplitMix64(u64);

impl SplitMix64 {
    const fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
    }

    fn rounds_below(&mut self, bound: u32) -> u32 {
        u32::try_from(self.next() % u64::from(bound)).unwrap()
    }
}

/// A client of the sweep: its command, where and when it sends next, the attempt it waits
/// on, and its terminal answer.
#[derive(Clone, Debug)]
struct Client {
    op: ClientOp,
    value: &'static str,
    target: usize,
    next_round: u32,
    waiting: Option<(usize, u64)>,
    done: Option<Said>,
}

/// What the sweep reached, so a pass is not vacuous (P10).
#[derive(Debug, Default)]
struct Reached {
    winners: BTreeMap<&'static str, u32>,
    leader_isolated: u32,
    unknown_outcomes: u32,
    redirects: u32,
    retries: u32,
}

/// One seeded race: both clients send and retry until each has a terminal answer, while the
/// leader of a chosen round may be isolated for a while. Returns the clients' terminal
/// answers by value, with the agreed original.
fn race(seed: u64, reached: &mut Reached) -> ((u64, String), Vec<(&'static str, Said)>) {
    let mut rng = SplitMix64(seed);
    let mut cluster = Cluster::elected("three-voters");
    let nodes = cluster.members.len();
    let mut clients = [(a(), "a"), (b(), "b")].map(|(op, value)| Client {
        op,
        value,
        target: rng.below(nodes),
        next_round: rng.rounds_below(4),
        waiting: None,
        done: None,
    });
    let fault = (rng.below(3) > 0).then(|| (rng.rounds_below(6), 5 + rng.rounds_below(40)));
    let mut attempt = 0;
    let mut round = 0;
    while clients.iter().any(|client| client.done.is_none()) {
        assert!(
            round < ROUNDS,
            "rule: both clients reach a terminal answer within {ROUNDS} rounds; seed {seed}: \
             clients {clients:?}; re-run: {RERUN}"
        );
        if let Some((start, length)) = fault {
            if round == start
                && let Some(leader) = cluster.leader()
            {
                let id = cluster.members[leader].id;
                cluster.isolate(id);
                reached.leader_isolated += 1;
            }
            if round == start + length {
                cluster.heal();
            }
        }
        for client in &mut clients {
            if client.done.is_none() && client.waiting.is_none() && round >= client.next_round {
                attempt += 1;
                cluster.send(client.target, attempt, client.op.clone());
                client.waiting = Some((client.target, attempt));
            }
        }
        cluster.round();
        for client in &mut clients {
            let Some((at, sent)) = client.waiting else {
                continue;
            };
            let Some(answer) = said_at(&cluster, at, sent) else {
                continue;
            };
            client.waiting = None;
            match answer {
                Said::Written { .. } | Said::Conflict { .. } => client.done = Some(answer),
                Said::Refused(Refusal::NotLeader { leader }) => {
                    reached.redirects += 1;
                    client.target = cluster.at(leader);
                    client.next_round = round + 1;
                }
                Said::Refused(
                    Refusal::UnknownOutcome
                    | Refusal::Busy
                    | Refusal::Unavailable
                    | Refusal::Recovering { .. }
                    | Refusal::HistoryFull { .. },
                )
                | Said::Read => {
                    if answer == Said::Refused(Refusal::UnknownOutcome) {
                        reached.unknown_outcomes += 1;
                    }
                    reached.retries += 1;
                    client.target = rng.below(nodes);
                    client.next_round = round + 1 + rng.rounds_below(3);
                }
            }
        }
        round += 1;
    }
    cluster.heal();
    cluster.settle(request(1));
    let agreed = original(&cluster);
    let answers = clients
        .iter()
        .map(|client| (client.value, client.done.expect("fixture: done")))
        .collect();
    (agreed, answers)
}

#[test]
fn every_seeded_same_id_race_resolves_to_one_original_and_one_conflict() {
    let mut reached = Reached::default();
    for seed in 0..SEEDS {
        let ((index, winner), answers) = race(seed, &mut reached);
        let resolved = answers.iter().all(|(value, said)| {
            if *value == winner {
                [
                    Said::Written {
                        index,
                        replayed: false,
                    },
                    Said::Written {
                        index,
                        replayed: true,
                    },
                ]
                .contains(said)
            } else {
                *said == Said::Conflict { original: index }
            }
        });
        assert!(
            resolved,
            "rule: when two commands race under one request ID, exactly one is the original, \
             answered with its own result (first or replayed), and the other is \
             request_id_conflict naming it, on every node (§7.8, F-6, C-6); seed {seed}: \
             original ({index}, {winner:?}), answers {answers:?}; re-run: {RERUN}"
        );
        *reached
            .winners
            .entry(if winner == "a" { "a" } else { "b" })
            .or_insert(0) += 1;
    }
    // Floors at about a quarter of what the sweep reached when they were set: a 35 and b 25
    // wins, 25 isolated leaders, 11 unknown outcomes, 85 redirects and 19 retries.
    let wins = |value| reached.winners.get(value).copied().unwrap_or(0);
    assert!(
        wins("a") >= 8
            && wins("b") >= 6
            && reached.leader_isolated >= 6
            && reached.unknown_outcomes >= 2
            && reached.redirects >= 20
            && reached.retries >= 4,
        "fixture: the sweep reaches both winners, isolated leaders, unknown outcomes, \
         redirects and retries, each at least its floor, so it is not vacuous (P10); reached \
         {reached:?}; re-run: {RERUN}"
    );
}
