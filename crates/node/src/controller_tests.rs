//! W14.1's tests of the serial controller (PLAN.md §7.2, §7.4, §7.7, §7.10–§7.12): start
//! from the recovered journal, admission at the queue bound, turns, ticks and deadlines,
//! incarnations, failure, and a cluster whose messages all travel sealed.
#![cfg(test)]

use std::collections::BTreeMap;

use statemachine::Applied;
use types::{
    AttemptId, ClientOp, Config, DroppedInput, Effect, HardState, Input, Lifecycle, LogEvent,
    LogIndex, NodeId, Refusal, Response, Role, StorageOp, SyncSeq, Term, Tick,
};

use crate::test_support::{
    FIRST_SYNC, Handed, INCARNATION, Member, config, genesis_journal, key, put, put_entry, value,
    write_frame,
};
use crate::{Failure, TICKS_PER_TURN};

fn lone() -> (Config, NodeId) {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    (config, id)
}

fn get(k: &str) -> ClientOp {
    ClientOp::Get { key: key(k) }
}

fn dropped_peer() -> Effect {
    Effect::Log(LogEvent::Dropped {
        input: DroppedInput::PeerMessage,
    })
}

/// A lone voter whose loop waits for a sync: its campaign's term and vote are unsynced.
fn syncing_lone() -> Member {
    let (config, id) = lone();
    let mut member = Member::fresh(&config, id);
    member.tick_until_syncing(100);
    member
}

#[test]
fn start_replays_the_committed_log_and_is_ready() {
    let (config, id) = lone();
    let journal = genesis_journal(&config, id);
    write_frame(
        &config,
        id,
        &journal,
        vec![
            put_entry(1, 1, 1, "a", "x"),
            put_entry(2, 1, 2, "b", "y"),
            put_entry(3, 1, 3, "c", "z"),
        ],
        HardState {
            term: Term::new(1).unwrap(),
            vote: Some(id),
            commit: LogIndex::new(2).unwrap(),
        },
    );
    let member = Member::start(&config, id, journal, 7);
    let state = member.ctl.state();
    assert_eq!(
        (
            state.applied_index(),
            state.get(&key("a")).map(|stored| stored.value().clone()),
            state.get(&key("b")).map(|stored| stored.value().clone()),
            state.get(&key("c")).is_some(),
        ),
        (
            LogIndex::new(2).unwrap(),
            Some(value("x")),
            Some(value("y")),
            false
        ),
        "rule: start replays exactly the committed entries 1..=commit into the state machine \
         (§7.7 step 5); entry 3 is not committed"
    );
    assert_eq!(
        (member.ctl.lifecycle(), member.logs()[..2].to_vec()),
        (
            Lifecycle::Ready,
            vec![
                LogEvent::LifecycleChanged {
                    from: Lifecycle::Starting,
                    to: Lifecycle::Recovering,
                },
                LogEvent::LifecycleChanged {
                    from: Lifecycle::Recovering,
                    to: Lifecycle::Ready,
                },
            ]
        ),
        "rule: the node is Ready once replayed, and says so (§7.7 step 7, O-3)"
    );
    assert!(
        member.ctl.requests().handed.is_empty(),
        "rule: replay goes to the state machine only; no attempt waits on it"
    );
}

#[test]
fn a_torn_tail_is_truncated_and_synced_before_the_first_append() {
    let (config, id) = lone();
    let journal = genesis_journal(&config, id);
    let genesis_len = journal.byte_len();
    journal.push(&[0xde, 0xad, 0xbe, 0xef]);
    let mut member = Member::start(&config, id, journal, 7);
    let storage: Vec<&Effect> = member
        .effects
        .iter()
        .filter(|effect| effect.storage_op().is_some())
        .collect();
    assert_eq!(
        storage[..2].to_vec(),
        vec![
            &Effect::Truncate {
                len: types::ByteCount::new(genesis_len)
            },
            &Effect::Sync {
                seq: FIRST_SYNC,
                incarnation: INCARNATION
            },
        ],
        "rule: a torn tail is truncated and synced before anything else is written \
         (DUR-6, D-50)"
    );
    assert!(
        member.complete_sync(),
        "fixture: the truncation's sync completes"
    );
    assert_eq!(
        (member.ctl.lifecycle(), member.journal.byte_len()),
        (Lifecycle::Ready, genesis_len),
        "rule: the controller takes its own sync's completion; the loop never sees it"
    );
    member.tick_until_syncing(100);
    assert_eq!(
        member.ctl.outstanding_sync(),
        FIRST_SYNC.checked_next(),
        "rule: the loop's syncs follow the start-up sync's number"
    );
}

#[test]
fn at_the_queue_bound_peers_are_dropped_and_clients_get_busy_but_completions_and_ticks_get_in() {
    let mut member = syncing_lone();
    let bound = 1_024;
    let stranger = NodeId::new(2).unwrap();
    for n in 0..bound {
        let effects = member.step(Input::Peer {
            from: stranger,
            bytes: vec![u8::try_from(n % 251).unwrap()],
        });
        assert!(
            effects.is_empty(),
            "rule: while the sync is outstanding, envelope {n} waits in the queue"
        );
    }
    assert_eq!(member.ctl.queued(), bound, "fixture: the queue is full");
    assert_eq!(
        member.step(Input::Peer {
            from: stranger,
            bytes: vec![1],
        }),
        vec![dropped_peer()],
        "rule: past the bound a peer envelope is dropped, and the drop is logged (§7.11)"
    );
    let attempt = AttemptId::new(77);
    assert_eq!(
        member.step(Input::Client {
            attempt,
            op: get("k"),
        }),
        vec![Effect::Respond {
            attempt,
            response: Response::Refused(Refusal::Busy),
        }],
        "rule: past the bound a client attempt is answered 429 busy, not submitted (§7.11)"
    );
    let before = member.ctl.now();
    member.step(Input::Tick);
    assert_eq!(
        (
            member.ctl.now(),
            member.ctl.raft_ticks_owed(),
            member.ctl.queued()
        ),
        (before.checked_add(Tick::new(1)).unwrap(), 1, bound),
        "rule: a tick is never dropped: the clock advances and the library's tick is owed \
         until the sync completes (§7.11, §7.12)"
    );
    let counters = member.ctl.counters();
    assert_eq!(
        (counters.peers_queue_full, counters.clients_queue_full),
        (1, 1),
        "rule: each refusal is counted under its reason"
    );
    assert!(
        member.ctl.requests().attempts().is_empty(),
        "rule: a refused attempt never reaches the request lifecycle"
    );
    while member.complete_sync() {}
    assert_eq!(
        (
            member.ctl.queued(),
            member.ctl.raft_ticks_owed(),
            member.ctl.envelope_drops().total(),
            member.ctl.status().role
        ),
        (0, 0, u64::try_from(bound).unwrap(), Role::Leader),
        "rule: the SyncDone gets in although the queue is full; then the queued envelopes are \
         opened (each from a non-voter, so each is dropped by the codec and counted, §8.2) \
         and the owed tick is fed"
    );
}

#[test]
fn client_attempts_are_handed_over_while_a_sync_is_outstanding_and_proposals_wait() {
    let (config, id) = lone();
    let mut member = Member::fresh(&config, id);
    member.elect(100);
    let first = AttemptId::new(1);
    member.step(Input::Client {
        attempt: first,
        op: put(1, "a", "x"),
    });
    let syncing = member.ctl.outstanding_sync();
    assert!(
        syncing.is_some(),
        "fixture: the first write was proposed, and its entry waits for its sync"
    );
    let second = AttemptId::new(2);
    let read = AttemptId::new(3);
    member.step(Input::Client {
        attempt: second,
        op: put(2, "b", "y"),
    });
    member.step(Input::Client {
        attempt: read,
        op: get("a"),
    });
    assert_eq!(
        (
            member.ctl.requests().attempts(),
            member.ctl.requests().waiting.len(),
            member.ctl.outstanding_sync()
        ),
        (vec![first, second, read], 1, syncing),
        "rule: attempts reach the request lifecycle while the sync is outstanding (reads are \
         answered then, §7.9); a proposal waits until the library can take it (§7.4 step 3)"
    );
    while member.complete_sync() {}
    let executed: Vec<types::RequestId> = member
        .ctl
        .requests()
        .applied()
        .into_iter()
        .filter_map(|(_, applied)| match applied {
            Applied::Executed { request_id, .. } => Some(request_id),
            Applied::Empty | Applied::Replayed { .. } | Applied::Conflict { .. } => None,
        })
        .collect();
    let proposals = member
        .ctl
        .requests()
        .handed
        .iter()
        .filter(|handed| matches_proposed(handed))
        .count();
    assert_eq!(
        (proposals, executed),
        (
            2,
            vec![
                crate::test_support::request(1),
                crate::test_support::request(2)
            ]
        ),
        "rule: once the sync completed, the waiting proposal was handed to the library as a \
         turn's input, and both writes were executed in order"
    );
}

fn matches_proposed(handed: &Handed) -> bool {
    match handed {
        Handed::Proposed { result, .. } => result.is_ok(),
        Handed::Attempt { .. }
        | Handed::Applied { .. }
        | Handed::Expired { .. }
        | Handed::StatusChanged { .. }
        | Handed::Shutdown => false,
    }
}

#[test]
fn deadlines_fire_on_ticks_even_while_a_sync_is_outstanding() {
    let (config, id) = lone();
    let mut member = Member::fresh(&config, id);
    member.elect(100);
    let attempt = AttemptId::new(9);
    member.step(Input::Client {
        attempt,
        op: put(9, "k", "v"),
    });
    let admitted = member.ctl.now();
    let deadline = admitted.checked_add(config.write_deadline()).unwrap();
    assert!(
        member.ctl.outstanding_sync().is_some(),
        "fixture: the write's sync is outstanding and stays so"
    );
    for _ in 0..config.write_deadline().get() - 1 {
        member.step(Input::Tick);
    }
    assert!(
        member.ctl.requests().expired().is_empty(),
        "rule: nothing fires before the deadline tick"
    );
    member.step(Input::Tick);
    assert_eq!(
        (
            member.ctl.requests().expired(),
            member.ctl.raft_ticks_owed(),
            member.ctl.deadlines()
        ),
        (vec![(attempt, deadline)], config.write_deadline().get(), 0),
        "rule: the write deadline fires at admission + write_deadline_ticks although the sync \
         never completed; the library's ticks are owed meanwhile (§7.8, §7.12)"
    );
    while member.complete_sync() {}
    assert_eq!(
        member.ctl.raft_ticks_owed(),
        0,
        "rule: owed ticks reach the library once the loop can take them"
    );
}

#[test]
fn a_burst_of_ticks_is_processed_eight_per_turn_between_queued_inputs() {
    let (config, id) = lone();
    let mut member = Member::fresh(&config, id);
    member.elect(100);
    let start = member.ctl.now().get();
    let mut admitted = Vec::new();
    for _ in 0..20 {
        admitted.extend(member.ctl.admit(Input::Tick));
    }
    for n in 1..=2 {
        admitted.extend(member.ctl.admit(Input::Client {
            attempt: AttemptId::new(n),
            op: get("k"),
        }));
    }
    assert_eq!(
        (admitted, member.ctl.pending_ticks(), member.ctl.queued()),
        (Vec::new(), 20, 2),
        "fixture: admit only queues and merges; no turn runs"
    );
    member.step(Input::Tick);
    let handed_at: Vec<u64> = member
        .ctl
        .requests()
        .handed
        .iter()
        .filter_map(|handed| match handed {
            Handed::Attempt { now, .. } => Some(now.get() - start),
            Handed::Proposed { .. }
            | Handed::Applied { .. }
            | Handed::Expired { .. }
            | Handed::StatusChanged { .. }
            | Handed::Shutdown => None,
        })
        .collect();
    assert_eq!(
        (
            handed_at,
            member.ctl.now().get() - start,
            member.ctl.pending_ticks()
        ),
        (vec![TICKS_PER_TURN, 2 * TICKS_PER_TURN], 21, 0),
        "rule: overdue ticks are processed at most {TICKS_PER_TURN} per turn, each batch \
         followed by one queued input, so a pause never starves other work (§7.10)"
    );
}

#[test]
fn a_completion_from_another_incarnation_is_ignored_and_counted() {
    let mut member = syncing_lone();
    let seq = member.ctl.outstanding_sync().unwrap();
    let old = types::Incarnation::new(INCARNATION.get() - 1);
    for input in [
        Input::SyncDone {
            seq,
            incarnation: old,
        },
        Input::StorageFailed {
            op: StorageOp::Sync(seq),
            incarnation: old,
        },
    ] {
        assert_eq!(
            member.step(input.clone()),
            Vec::<Effect>::new(),
            "rule: a completion from a previous incarnation is ignored (§7.2, B-17); \
             input={input:?}"
        );
    }
    assert_eq!(
        (
            member.ctl.outstanding_sync(),
            member.ctl.lifecycle(),
            member.ctl.counters().stale_completions
        ),
        (Some(seq), Lifecycle::Ready, 2),
        "rule: it changes nothing, and it is counted"
    );
}

#[test]
fn a_failed_storage_effect_fails_the_node_which_stops_feeding_raft() {
    let mut member = syncing_lone();
    let effects = member.step(Input::StorageFailed {
        op: StorageOp::Append,
        incarnation: INCARNATION,
    });
    assert_eq!(
        (
            effects,
            member.ctl.lifecycle(),
            member.ctl.failure().cloned()
        ),
        (
            vec![Effect::Log(LogEvent::LifecycleChanged {
                from: Lifecycle::Ready,
                to: Lifecycle::Failed,
            })],
            Lifecycle::Failed,
            Some(Failure::Storage(StorageOp::Append))
        ),
        "rule: an unexpected I/O error moves the node to failed, visibly (§7.11, DM-7)"
    );
    let seq = member.ctl.outstanding_sync();
    let now = member.ctl.now();
    let peer_effects = member.step(Input::Peer {
        from: NodeId::new(2).unwrap(),
        bytes: vec![1],
    });
    member.step(Input::Tick);
    member.step(Input::Client {
        attempt: AttemptId::new(5),
        op: get("k"),
    });
    if let Some(seq) = seq {
        member.step(Input::SyncDone {
            seq,
            incarnation: INCARNATION,
        });
    }
    assert_eq!(
        (
            peer_effects,
            member.ctl.counters().peers_failed,
            member.ctl.now(),
            member.ctl.raft_ticks_owed(),
            member.ctl.requests().handed.last().cloned(),
        ),
        (
            vec![dropped_peer()],
            1,
            now.checked_add(Tick::new(1)).unwrap(),
            0,
            Some(Handed::Attempt {
                attempt: AttemptId::new(5),
                now: now.checked_add(Tick::new(1)).unwrap(),
                lifecycle: Lifecycle::Failed,
            }),
        ),
        "rule: a failed node drops and counts peer messages and feeds the library nothing, \
         but its clock still runs and attempts still reach the request lifecycle, which \
         answers recovering (DM-7, A-2)"
    );
}

#[test]
fn shutdown_is_handed_to_the_request_lifecycle() {
    let (config, id) = lone();
    let mut member = Member::fresh(&config, id);
    member.step(Input::Shutdown);
    assert_eq!(
        member.ctl.requests().handed,
        vec![Handed::Shutdown],
        "rule: only the request lifecycle knows which attempts were submitted, so it answers \
         them (§7.8, A-5)"
    );
}

/// Three members exchanging every message through the controllers' own envelopes.
struct Cluster {
    members: BTreeMap<NodeId, Member>,
}

impl Cluster {
    fn new(config: &Config) -> Self {
        Self {
            members: config
                .voters()
                .map(|id| (id, Member::fresh(config, id)))
                .collect(),
        }
    }

    /// Completes every sync and delivers every envelope until the cluster is quiet.
    fn settle(&mut self) {
        loop {
            let mut moved = false;
            let ids: Vec<NodeId> = self.members.keys().copied().collect();
            for id in &ids {
                let member = self.members.get_mut(id).unwrap();
                while member.complete_sync() {
                    moved = true;
                }
                let outbox: Vec<(NodeId, Vec<u8>)> = member.outbox.drain(..).collect();
                for (to, bytes) in outbox {
                    assert_eq!(
                        &bytes[..4],
                        b"KVPE",
                        "rule: every message leaves the core sealed in an envelope (§8.2)"
                    );
                    moved = true;
                    self.members
                        .get_mut(&to)
                        .unwrap()
                        .step(Input::Peer { from: *id, bytes });
                }
            }
            if !moved {
                return;
            }
        }
    }

    fn leader(&self) -> Option<NodeId> {
        self.members
            .iter()
            .find(|(_, member)| member.ctl.status().role == Role::Leader)
            .map(|(id, _)| *id)
    }
}

#[test]
fn three_voters_elect_and_commit_with_every_message_sealed_and_none_dropped() {
    let config = config("three-voters");
    let mut cluster = Cluster::new(&config);
    let mut ticks = 0;
    while cluster.leader().is_none() {
        assert!(ticks < 200, "fixture: no leader within 200 ticks");
        for member in cluster.members.values_mut() {
            member.step(Input::Tick);
        }
        cluster.settle();
        ticks += 1;
    }
    let leader = cluster.leader().unwrap();
    cluster
        .members
        .get_mut(&leader)
        .unwrap()
        .step(Input::Client {
            attempt: AttemptId::new(1),
            op: put(1, "k", "v"),
        });
    cluster.settle();
    for _ in 0..4 {
        for member in cluster.members.values_mut() {
            member.step(Input::Tick);
        }
        cluster.settle();
    }
    for (id, member) in &cluster.members {
        assert_eq!(
            (
                member
                    .ctl
                    .state()
                    .get(&key("k"))
                    .map(|stored| stored.value().clone()),
                member.ctl.envelope_drops().total(),
                member.ctl.counters(),
                member.ctl.lifecycle()
            ),
            (
                Some(value("v")),
                0,
                crate::Counters::default(),
                Lifecycle::Ready
            ),
            "rule: the write commits and applies on every voter, through envelopes the \
             controllers sealed and opened themselves, with nothing dropped (§8.2, C-1); \
             node={id}"
        );
    }
}

#[test]
fn a_journal_that_cannot_be_read_back_refuses_to_start() {
    let (config, id) = lone();
    let journal = genesis_journal(&config, id);
    write_frame(
        &config,
        id,
        &journal,
        vec![put_entry(1, 1, 1, "a", "x")],
        HardState {
            term: Term::new(1).unwrap(),
            vote: Some(id),
            commit: LogIndex::new(1).unwrap(),
        },
    );
    // A 16-byte entry cache keeps no entry, so replay must read entry 1 back.
    let recovered = journal::recover(
        &journal,
        types::ByteCount::new(journal.byte_len()),
        &config,
        id,
        journal::LogBounds::new(types::ByteCount::new(16), config.max_journal_bytes()),
    )
    .unwrap();
    journal.fail_reads();
    let started = crate::Controller::start(crate::Boot {
        config: &config,
        node: id,
        recovered,
        reader: journal,
        entropy: crate::test_support::Seeded::new(1),
        incarnation: INCARNATION,
        first_sync: SyncSeq::new(1),
        requests: crate::test_support::Recorder::default(),
    });
    let refused = started.err();
    assert!(
        matches_replay(refused.as_ref()),
        "rule: a committed entry that cannot be read back refuses the start, naming the \
         entry (§7.7 step 5); got {refused:?}"
    );
}

fn matches_replay(error: Option<&crate::StartError>) -> bool {
    match error {
        Some(crate::StartError::Replay { index, .. }) => *index == 1,
        Some(
            crate::StartError::NotAVoter { .. }
            | crate::StartError::Construct(_)
            | crate::StartError::SyncSeqExhausted,
        )
        | None => false,
    }
}

#[test]
fn the_same_inputs_give_the_same_effects() {
    let run = || {
        let (config, id) = lone();
        let mut member = Member::fresh(&config, id);
        member.elect(100);
        for n in 0..5 {
            member.step(Input::Client {
                attempt: AttemptId::new(n),
                op: put(u8::try_from(n).unwrap(), "k", "v"),
            });
            member.step(Input::Tick);
            while member.complete_sync() {}
        }
        member.effects
    };
    assert_eq!(
        run(),
        run(),
        "rule: the controller is deterministic: the same inputs give the same effects in the \
         same order (T-3)"
    );
}
