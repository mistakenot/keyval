//! W14.4's attempt-table tests (PLAN.md §7.8, §8.4, F-7, A-2, A-5; W14): every PUT and GET
//! outcome path of the §7.8 attempt table, driven through core inputs only. Each test hands
//! `Input`s to controllers that run the node's real request lifecycle, and reads the
//! answers from their effects; none calls the write path directly.
//!
//! The paths: `unavailable` with no leader known or while stopping, `not_leader` naming the
//! believed leader, `busy` at each local bound (waiting attempts, waiters per request,
//! uncommitted bytes, the inbound queue), `recovering` once failed, the short-circuit on any
//! Ready node, attach, the same-ID race, `unknown_outcome` on a deadline, a leadership loss,
//! a storage failure, the journal's hard stop or shutdown, and F-7's precedence.
#![cfg(test)]

// check: NODE-ATTEMPT-TABLE

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use raft_adapter::LoopError;
use statemachine::codec::{self, Command};
use types::{
    AttemptId, ByteCount, ClientOp, Config, Decision, Effect, EntryIndex, HistoryLimit, Input,
    Lifecycle, Limit, Mutation, NodeId, NotReadyLifecycle, Refusal, RequestId, Response, Role,
    ServedBy, StorageOp, TerminalResult, Version,
};

use crate::test_support::{
    FIRST_SYNC, INCARNATION, Member, Seeded, config, genesis_journal, key, put, request, value,
};
use crate::{Boot, Controller, Failure, RequestLifecycle};

/// A node running the real request lifecycle.
type Node = Member<RequestLifecycle>;

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

/// A lone voter that leads.
fn lone_leader() -> (Config, Node) {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    let mut node = Member::fresh_lifecycle(&config, id);
    node.elect(100);
    (config, node)
}

/// A request ID numbered past [`request`]'s 256, for tests that need more distinct IDs.
fn wide_request(n: u64) -> RequestId {
    let mut bytes = [5_u8; 16];
    bytes[..8].copy_from_slice(&n.to_be_bytes());
    RequestId::from_bytes(bytes)
}

/// A PUT of `k = v` under `request_id`.
fn put_as(request_id: RequestId, k: &str, v: &str) -> ClientOp {
    ClientOp::Write {
        request_id,
        mutation: Mutation::Put {
            key: key(k),
            value: value(v),
        },
    }
}

fn get(k: &str) -> ClientOp {
    ClientOp::Get { key: key(k) }
}

/// The canonical command's length, in bytes, of the write `op`.
fn command_len(op: &ClientOp) -> u64 {
    let ClientOp::Write {
        request_id,
        mutation,
    } = op.clone()
    else {
        panic!("fixture: a write");
    };
    u64::try_from(
        codec::encode(&Command {
            request_id,
            mutation,
        })
        .len(),
    )
    .unwrap()
}

/// Hands client attempt `n`, asking `op`, to `node`.
fn send(node: &mut Node, n: u64, op: ClientOp) {
    node.step(Input::Client {
        attempt: AttemptId::new(n),
        op,
    });
}

/// Steps `ticks` ticks into `node`.
fn tick(node: &mut Node, ticks: u64) {
    for _ in 0..ticks {
        node.step(Input::Tick);
    }
}

/// Completes every sync `node` asks for, until it asks for none.
fn sync_all(node: &mut Node) {
    while node.complete_sync() {}
}

/// The response attempt `n` got, if any.
///
/// Panics if it got more than one: an attempt is answered exactly once (§7.8).
fn answer(node: &Node, n: u64) -> Option<Response> {
    let all: Vec<Response> = node
        .effects
        .iter()
        .filter_map(|effect| match effect {
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
        .collect();
    assert!(
        all.len() <= 1,
        "rule: an attempt is answered once (§7.8); node {} answered attempt {n} {all:?}",
        node.id
    );
    all.into_iter().next()
}

/// The answers of attempts `from..=to`, in order.
fn answers(node: &Node, from: u64, to: u64) -> Vec<Option<Response>> {
    (from..=to).map(|n| answer(node, n)).collect()
}

fn refused(refusal: Refusal) -> Response {
    Response::Refused(refusal)
}

/// What a node answers a PUT first executed at `index`, served by `served_by`.
fn written_at(index: EntryIndex, replayed: bool, served_by: ServedBy) -> Response {
    Response::Written {
        result: TerminalResult::executed_at(index, Decision::Set),
        replayed,
        served_by,
    }
}

/// What a leader answers when the PUT it proposed applies at `index`: served at `index`.
fn applied_at(node: &Node, index: EntryIndex) -> Response {
    written_at(
        index,
        false,
        ServedBy {
            node: node.id,
            applied: index.index(),
        },
    )
}

fn conflict_at(index: EntryIndex) -> Response {
    Response::Conflict {
        original_index: index,
    }
}

/// Where `request_id` first executed on `node`, if it has.
fn executed_at(node: &Node, request_id: &RequestId) -> Option<EntryIndex> {
    node.ctl
        .state()
        .history()
        .get(request_id)
        .map(statemachine::Record::first_index)
}

/// The node and the applied index a read on `node` is served at now.
fn served(node: &Node) -> ServedBy {
    ServedBy {
        node: node.id,
        applied: node.ctl.state().applied_index(),
    }
}

/// The attempts waiting in `node`'s write path.
fn waiting(node: &Node) -> usize {
    node.ctl.requests().writes().waiting()
}

/// The proposals pending in `node`'s write path.
fn pending(node: &Node) -> usize {
    node.ctl.requests().writes().pending()
}

fn recovering() -> Response {
    refused(Refusal::Recovering {
        lifecycle: NotReadyLifecycle::Failed,
    })
}

// ---------------------------------------------------------------------------------------
// A cluster on a network the test controls
// ---------------------------------------------------------------------------------------

/// Voters running the real request lifecycle, on a network the test controls.
/// - A cut node neither sends nor receives: envelopes from or to it are lost. It is not
///   ticked by [`Cluster::tick`] either, so an isolated leader keeps leading until a test
///   ticks it itself.
/// - A held node's syncs complete only when a test completes them, so its library takes no
///   input meanwhile: proposals queue, and peer envelopes wait (§7.4 step 3).
struct Cluster {
    nodes: BTreeMap<NodeId, Node>,
    cut: BTreeSet<NodeId>,
    held: BTreeSet<NodeId>,
}

impl Cluster {
    fn new(config: &Config) -> Self {
        Self {
            nodes: config
                .voters()
                .map(|id| (id, Member::fresh_lifecycle(config, id)))
                .collect(),
            cut: BTreeSet::new(),
            held: BTreeSet::new(),
        }
    }

    fn node(&mut self, id: NodeId) -> &mut Node {
        self.nodes.get_mut(&id).unwrap()
    }

    fn get(&self, id: NodeId) -> &Node {
        &self.nodes[&id]
    }

    /// Completes the syncs of every node not held, and delivers every envelope between
    /// nodes not cut, until nothing moves.
    fn settle(&mut self) {
        for _ in 0..10_000 {
            let mut moved = false;
            let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
            for id in ids {
                if !self.held.contains(&id) {
                    while self.node(id).complete_sync() {
                        moved = true;
                    }
                }
                let outbox: Vec<(NodeId, Vec<u8>)> = self.node(id).outbox.drain(..).collect();
                for (to, bytes) in outbox {
                    if self.cut.contains(&id) || self.cut.contains(&to) {
                        continue;
                    }
                    moved = true;
                    self.node(to).step(Input::Peer { from: id, bytes });
                }
            }
            if !moved {
                return;
            }
        }
        panic!("fixture: the cluster did not settle in 10,000 rounds");
    }

    /// One tick on each of `ids`, then settles.
    fn tick_only(&mut self, ids: &[NodeId]) {
        for id in ids {
            self.node(*id).step(Input::Tick);
        }
        self.settle();
    }

    /// One tick on every node not cut, then settles.
    fn tick(&mut self) {
        let up: Vec<NodeId> = self
            .nodes
            .keys()
            .copied()
            .filter(|id| !self.cut.contains(id))
            .collect();
        self.tick_only(&up);
    }

    /// Ticks until `done` holds, at most `limit` ticks.
    fn tick_until(&mut self, limit: u64, what: &str, done: impl Fn(&Self) -> bool) {
        for _ in 0..limit {
            if done(self) {
                return;
            }
            self.tick();
        }
        assert!(done(self), "fixture: {what} within {limit} ticks");
    }

    /// Ticks until a node not cut leads and every node not cut believes in it.
    fn elect(&mut self) -> NodeId {
        for _ in 0..400 {
            self.tick();
            let up: Vec<&Node> = self
                .nodes
                .values()
                .filter(|node| !self.cut.contains(&node.id))
                .collect();
            let agreed = up
                .iter()
                .find(|node| node.ctl.status().role == Role::Leader)
                .map(|node| node.id)
                .filter(|leader| {
                    up.iter()
                        .all(|node| node.ctl.status().leader == Some(*leader))
                });
            if let Some(leader) = agreed {
                return leader;
            }
        }
        panic!("fixture: no agreed leader in 400 ticks");
    }
}

// ---------------------------------------------------------------------------------------
// NotSubmitted: unavailable, not_leader, busy, recovering
// ---------------------------------------------------------------------------------------

#[test]
fn a_new_write_is_unavailable_with_no_leader_known_and_redirected_to_the_believed_leader() {
    let config = config("three-voters");
    let mut cluster = Cluster::new(&config);
    let first = config.voters().next().unwrap();
    send(cluster.node(first), 1, put(1, "k", "v"));
    send(cluster.node(first), 2, get("k"));
    let node = cluster.get(first);
    assert_eq!(
        (answer(node, 1), answer(node, 2), waiting(node)),
        (
            Some(refused(Refusal::Unavailable)),
            Some(Response::NotFound {
                served_by: served(node)
            }),
            0
        ),
        "rule: with no leader known, a new write answers unavailable (not_submitted) and \
         waits nowhere; a GET is served locally all the same (F-7, §7.9, §8.4)"
    );
    let leader = cluster.elect();
    let follower = config.voters().find(|id| *id != leader).unwrap();
    let last = cluster.get(follower).ctl.status().last;
    send(cluster.node(follower), 3, put(3, "k", "v"));
    let node = cluster.get(follower);
    assert_eq!(
        (answer(node, 3), waiting(node), node.ctl.status().last),
        (Some(refused(Refusal::NotLeader { leader })), 0, last),
        "rule: a follower answers a new write not_leader naming the leader it believes in \
         (the 307's Location), proposing nothing (F-7, §8.4)"
    );
    cluster.cut.insert(leader);
    let next = cluster.elect();
    let other = config
        .voters()
        .find(|id| *id != leader && *id != next)
        .unwrap();
    send(cluster.node(other), 4, put(4, "k", "v"));
    assert_eq!(
        answer(cluster.get(other), 4),
        Some(refused(Refusal::NotLeader { leader: next })),
        "rule: the redirect follows the leader the node believes in now: {next}, elected \
         after {leader} was cut off (F-7)"
    );
}

#[test]
fn the_attempt_count_bound_answers_busy_before_the_history_and_never_holds_a_read() {
    let (config, mut node) = lone_leader();
    let bound = config.limit(Limit::PendingAttempts);
    let known = put_as(wide_request(0), "k", "v");
    send(&mut node, 1, known.clone());
    sync_all(&mut node);
    let index = executed_at(&node, &wide_request(0)).unwrap();
    // Attempt 2 is submitted and its sync is held; every later write queues behind it.
    for n in 0..bound {
        send(&mut node, 2 + n, put_as(wide_request(1 + n), "k", "v"));
    }
    let full = waiting(&node);
    let past = 2 + bound;
    send(&mut node, past, put_as(wide_request(1 + bound), "k", "w"));
    send(&mut node, past + 1, known);
    send(&mut node, past + 2, get("k"));
    let found = Some(Response::Found {
        value: value("v"),
        version: Version::at(index.index()).unwrap(),
        served_by: served(&node),
    });
    assert_eq!(
        (full, answers(&node, past, past + 2)),
        (
            usize::try_from(bound).unwrap(),
            vec![
                Some(refused(Refusal::Busy)),
                Some(refused(Refusal::Busy)),
                found
            ]
        ),
        "rule: with pending_attempts ({bound}) writes waiting, a new write answers busy, and \
         so does a retry of a known request ID: the local admission bounds come before the \
         short-circuit (F-7 step 3, §7.11); a GET is never held by the write table (A-2)"
    );
    sync_all(&mut node);
    let first_executions = (2..past)
        .filter(|n| {
            answer(&node, *n).is_some_and(|response| match response {
                Response::Written { replayed, .. } => !replayed,
                Response::Conflict { .. }
                | Response::Found { .. }
                | Response::NotFound { .. }
                | Response::Page(_)
                | Response::Status { .. }
                | Response::Refused(_) => false,
            })
        })
        .count();
    assert_eq!(
        (
            first_executions,
            executed_at(&node, &wide_request(1 + bound))
        ),
        (usize::try_from(bound).unwrap(), None),
        "rule: every waiting write commits once its turn comes, and the write refused busy \
         was never submitted, so nothing executed it (§8.4 not_submitted)"
    );
}

#[test]
fn the_uncommitted_bytes_bound_is_busy_one_byte_past_its_boundary_and_reopens_as_entries_apply() {
    let config = config("small-limits");
    let max = config.max_uncommitted_bytes().get();
    // Commands of exactly 64 bytes: `max` is a multiple of 64.
    let pad = 64 - (command_len(&put(1, "k", "x")) - 1);
    let fits = "x".repeat(usize::try_from(pad).unwrap());
    let longer = "x".repeat(usize::try_from(pad + 1).unwrap());
    assert_eq!(
        (command_len(&put(1, "k", &fits)), max % 64),
        (64, 0),
        "fixture: 64-byte commands fill max_uncommitted_bytes exactly"
    );
    let count = max / 64;
    let mut cluster = Cluster::new(&config);
    let leader = cluster.elect();
    cluster
        .cut
        .extend(config.voters().filter(|id| *id != leader));
    for n in 1..count {
        send(
            cluster.node(leader),
            n,
            put(u8::try_from(n).unwrap(), "k", &fits),
        );
        cluster.settle();
    }
    // `count - 1` commands in flight: one byte more than fits answers busy, an exact fit
    // is admitted, and after that nothing is.
    send(cluster.node(leader), 100, put(100, "k", &longer));
    send(
        cluster.node(leader),
        count,
        put(u8::try_from(count).unwrap(), "k", &fits),
    );
    cluster.settle();
    send(cluster.node(leader), 101, put(101, "k", &fits));
    let node = cluster.get(leader);
    assert_eq!(
        (
            answer(node, 100),
            node.ctl.requests().writes().in_flight_bytes(),
            answer(node, 101),
            answers(node, 1, count).iter().flatten().count()
        ),
        (
            Some(refused(Refusal::Busy)),
            max,
            Some(refused(Refusal::Busy)),
            0
        ),
        "rule: the commands proposed this leadership and not yet applied may total exactly \
         max_uncommitted_bytes ({max}); a command that would pass it answers busy before it \
         is submitted; nothing is answered before its entry applies (F-7 step 8, §7.11)"
    );
    cluster.cut.clear();
    cluster.tick_until(300, "every write in flight applies", |cluster| {
        answers(cluster.get(leader), 1, count)
            .iter()
            .all(Option::is_some)
    });
    let drained = (
        cluster
            .get(leader)
            .ctl
            .requests()
            .writes()
            .in_flight_bytes(),
        pending(cluster.get(leader)),
    );
    send(cluster.node(leader), 102, put(101, "k", &fits));
    cluster.tick_until(100, "the retry applies", |cluster| {
        answer(cluster.get(leader), 102).is_some()
    });
    let node = cluster.get(leader);
    let index = executed_at(node, &request(101)).unwrap();
    assert_eq!(
        (drained, answer(node, 102)),
        ((0, 0), Some(applied_at(node, index))),
        "rule: the bound reopens as the entries apply, and the write refused busy was never \
         submitted, so its retry executes it for the first time (§7.11, A-5)"
    );
}

#[test]
fn a_full_inbound_queue_answers_reads_and_writes_busy_until_it_drains() {
    let (config, mut node) = lone_leader();
    let bound = config.limit(Limit::ControllerQueueInputs);
    send(&mut node, 1, put(1, "k", "v"));
    // Attempt 1's sync is held, so peer envelopes wait in the queue.
    let peer = NodeId::new(2).unwrap();
    for _ in 0..bound {
        node.step(Input::Peer {
            from: peer,
            bytes: vec![0; 8],
        });
    }
    send(&mut node, 2, get("k"));
    send(&mut node, 3, put(3, "k", "w"));
    let full = (node.ctl.queued(), answers(&node, 2, 3));
    sync_all(&mut node);
    send(&mut node, 4, get("k"));
    let index = executed_at(&node, &request(1)).unwrap();
    assert_eq!(
        (full, answer(&node, 4), executed_at(&node, &request(3))),
        (
            (
                usize::try_from(bound).unwrap(),
                vec![Some(refused(Refusal::Busy)), Some(refused(Refusal::Busy))]
            ),
            Some(Response::Found {
                value: value("v"),
                version: Version::at(index.index()).unwrap(),
                served_by: served(&node),
            }),
            None
        ),
        "rule: with controller_queue_inputs ({bound}) inputs queued, a GET and a write both \
         answer busy (A-2, §7.11): the write is not submitted; once the queue drains a GET \
         is served again"
    );
}

#[test]
fn a_storage_failure_answers_submitted_writes_unknown_outcome_and_everything_else_recovering() {
    let (_, mut node) = lone_leader();
    send(&mut node, 1, put(1, "known", "v"));
    sync_all(&mut node);
    send(&mut node, 2, put(2, "k", "v"));
    let sync = node.ctl.outstanding_sync().unwrap();
    send(&mut node, 3, put(3, "k", "v"));
    send(&mut node, 4, put(2, "k", "v"));
    send(&mut node, 5, put(2, "k", "other"));
    let quiet = answers(&node, 2, 5);
    node.step(Input::StorageFailed {
        op: StorageOp::Sync(sync),
        incarnation: INCARNATION,
    });
    assert_eq!(
        (quiet, answers(&node, 2, 5), node.ctl.lifecycle()),
        (
            vec![None; 4],
            vec![
                Some(refused(Refusal::UnknownOutcome)),
                Some(recovering()),
                Some(refused(Refusal::UnknownOutcome)),
                Some(recovering())
            ],
            Lifecycle::Failed
        ),
        "rule: when the node fails, a submitted write (2) and a retry attached to it (4) \
         answer unknown_outcome (A-5); a write queued behind the sync (3) and one waiting \
         behind another command (5) were never submitted: they enter F-7 again and answer \
         recovering (§7.8, A-2)"
    );
    send(&mut node, 6, put(6, "k", "v"));
    send(&mut node, 7, put(1, "known", "v"));
    send(&mut node, 8, get("known"));
    assert_eq!(
        (answers(&node, 6, 8), waiting(&node)),
        (vec![Some(recovering()); 3], 0),
        "rule: a failed node answers recovering to a new write, to a retry of a known \
         request ID and to a GET: the lifecycle comes before the short-circuit (F-7 step 2, \
         A-2)"
    );
}

#[test]
fn a_hard_stop_while_the_loop_drains_a_proposal_answers_it_unknown_outcome() {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    // A first run measures the journal after the election, and after the write's append.
    let (elected, appended) = {
        let mut probe = Member::fresh_lifecycle(&config, id);
        probe.elect(100);
        let elected = probe.journal.byte_len();
        send(&mut probe, 1, put(1, "k", "v"));
        (elected, probe.journal.byte_len())
    };
    // The same node again, whose hard stop admits the election's frames but not the write's.
    let journal = genesis_journal(&config, id);
    let limit = appended - 1;
    let recovered = journal::recover(
        &journal,
        ByteCount::new(journal.byte_len()),
        &config,
        id,
        journal::LogBounds::new(
            journal::LogBounds::of(&config).cache_bytes(),
            ByteCount::new(limit),
        ),
    )
    .unwrap();
    let (ctl, effects) = Controller::start(Boot {
        config: &config,
        node: id,
        recovered,
        reader: journal.clone(),
        entropy: Seeded::new(id.get()),
        incarnation: INCARNATION,
        first_sync: FIRST_SYNC,
        requests: RequestLifecycle::new(&config, id),
    })
    .unwrap();
    let mut node = Member {
        id,
        ctl,
        journal,
        effects: Vec::new(),
        syncs: VecDeque::new(),
        outbox: VecDeque::new(),
    };
    node.execute(effects);
    node.elect(100);
    assert_eq!(
        (node.journal.byte_len(), appended > elected),
        (elected, true),
        "fixture: the same election, frame for frame, and the write appends a frame"
    );
    send(&mut node, 1, put(1, "k", "v"));
    let stopped = match node.ctl.failure() {
        Some(Failure::Loop(LoopError::HardStop { .. })) => true,
        Some(
            Failure::Loop(LoopError::StorageFailed(_) | LoopError::Stopped) | Failure::Storage(_),
        )
        | None => false,
    };
    assert_eq!(
        (answer(&node, 1), stopped, node.journal.byte_len() <= limit),
        (Some(refused(Refusal::UnknownOutcome)), true, true),
        "rule: the write was handed to the library before its frame hit the journal's hard \
         stop, so it answers unknown_outcome, never busy (A-5, DM-7, §7.8); failure={:?}",
        node.ctl.failure()
    );
    send(&mut node, 2, put(2, "k", "v"));
    assert_eq!(
        answer(&node, 2),
        Some(recovering()),
        "rule: a node at its hard stop is failed and answers recovering (DM-7, A-2)"
    );
}

#[test]
fn shutdown_answers_submitted_writes_unknown_outcome_and_everything_else_unavailable() {
    let (_, mut node) = lone_leader();
    send(&mut node, 1, put(1, "known", "v"));
    sync_all(&mut node);
    send(&mut node, 2, put(2, "k", "v"));
    send(&mut node, 3, put(3, "k", "v"));
    send(&mut node, 4, put(2, "k", "v"));
    send(&mut node, 5, put(2, "k", "other"));
    node.step(Input::Shutdown);
    let stopping = answers(&node, 2, 5);
    send(&mut node, 6, put(6, "k", "v"));
    send(&mut node, 7, put(1, "known", "v"));
    send(&mut node, 8, get("known"));
    assert_eq!(
        (
            stopping,
            answers(&node, 6, 8),
            waiting(&node),
            node.ctl.deadlines()
        ),
        (
            vec![
                Some(refused(Refusal::UnknownOutcome)),
                Some(refused(Refusal::Unavailable)),
                Some(refused(Refusal::UnknownOutcome)),
                Some(refused(Refusal::Unavailable))
            ],
            vec![Some(refused(Refusal::Unavailable)); 3],
            0,
            0
        ),
        "rule: at shutdown a submitted write (2) and a retry attached to it (4) answer \
         unknown_outcome; a queued write (3) and one behind another command (5) answer \
         unavailable; from then on every attempt, a known retry and a GET included, answers \
         unavailable, and no attempt or deadline is left (§7.8, §8.4)"
    );
}

#[test]
fn at_the_history_capacity_the_leader_answers_history_full_and_a_follower_redirects() {
    let config = config("small-limits");
    let entries = config.max_log_entries();
    let mut cluster = Cluster::new(&config);
    let leader = cluster.elect();
    let follower = config.voters().find(|id| *id != leader).unwrap();
    let mut n: u8 = 0;
    while cluster.get(leader).ctl.status().commit < entries {
        n += 1;
        send(cluster.node(leader), u64::from(n), put(n, "k", "v"));
        cluster.tick_until(50, "the write applies", |cluster| {
            answer(cluster.get(leader), u64::from(n)).is_some()
        });
    }
    cluster.tick_until(50, "the follower applies the last write", |cluster| {
        executed_at(cluster.get(follower), &request(n)).is_some()
    });
    let index = executed_at(cluster.get(leader), &request(1)).unwrap();
    for (node, at) in [(leader, 1_000), (follower, 2_000)] {
        send(cluster.node(node), at, put(1, "k", "v"));
        send(cluster.node(node), at + 1, put(200, "k", "v"));
    }
    send(cluster.node(leader), 1_002, get("k"));
    let leader_answers = answers(cluster.get(leader), 1_000, 1_001);
    let follower_answers = answers(cluster.get(follower), 2_001, 2_001);
    let replayed = |id: NodeId, at: u64| {
        answer(cluster.get(id), at).is_some_and(|response| match response {
            Response::Written {
                result, replayed, ..
            } => replayed && result == TerminalResult::executed_at(index, Decision::Set),
            Response::Conflict { .. }
            | Response::Found { .. }
            | Response::NotFound { .. }
            | Response::Page(_)
            | Response::Status { .. }
            | Response::Refused(_) => false,
        })
    };
    assert_eq!(
        (
            leader_answers[1].clone(),
            follower_answers,
            replayed(leader, 1_000),
            replayed(follower, 2_000),
            answer(cluster.get(leader), 1_002).is_some_and(|response| match response {
                Response::Found { .. } => true,
                Response::Written { .. }
                | Response::Conflict { .. }
                | Response::NotFound { .. }
                | Response::Page(_)
                | Response::Status { .. }
                | Response::Refused(_) => false,
            })
        ),
        (
            Some(refused(Refusal::HistoryFull {
                limit: HistoryLimit::LogEntries
            })),
            vec![Some(refused(Refusal::NotLeader { leader }))],
            true,
            true,
            true
        ),
        "rule: with the commit index at max_log_entries ({entries}), the leader answers a new \
         write history_full (F-7 step 7, DM-7) while a follower redirects it: the leader \
         check comes first (step 6); a retry of a known request ID is replayed on both: the \
         short-circuit comes before both (step 4); reads keep working (DM-7); leader={leader} \
         follower={follower}"
    );
}

// ---------------------------------------------------------------------------------------
// The short-circuit and attach
// ---------------------------------------------------------------------------------------

#[test]
fn a_known_request_id_is_answered_from_the_history_on_any_ready_node_and_never_redirected() {
    let config = config("three-voters");
    let mut cluster = Cluster::new(&config);
    let leader = cluster.elect();
    let follower = config.voters().find(|id| *id != leader).unwrap();
    send(cluster.node(leader), 1, put(1, "k", "v"));
    cluster.tick_until(50, "the follower applies the write", |cluster| {
        executed_at(cluster.get(follower), &request(1)).is_some()
    });
    let index = executed_at(cluster.get(leader), &request(1)).unwrap();
    let node = cluster.node(follower);
    let served_by = served(node);
    send(node, 2, put(1, "k", "v"));
    send(node, 3, put(1, "k", "other"));
    send(node, 4, put(4, "k", "v"));
    assert_eq!(
        (
            answer(cluster.get(leader), 1),
            answers(cluster.get(follower), 2, 4)
        ),
        (
            Some(applied_at(cluster.get(leader), index)),
            vec![
                Some(written_at(index, true, served_by)),
                Some(conflict_at(index)),
                Some(refused(Refusal::NotLeader { leader }))
            ]
        ),
        "rule: a follower answers a known request ID from its own applied history: the \
         original result (replayed) for the same command, request_id_conflict for another; \
         it never redirects a known retry, only a new write (F-6, F-7 step 4, §7.8)"
    );
}

#[test]
fn a_same_command_retry_attaches_to_the_pending_proposal_without_a_second_entry() {
    let (_, mut node) = lone_leader();
    send(&mut node, 1, put(1, "k", "v"));
    let last = node.ctl.status().last;
    send(&mut node, 2, put(1, "k", "v"));
    let attached = (answer(&node, 2), waiting(&node), pending(&node));
    sync_all(&mut node);
    let index = executed_at(&node, &request(1)).unwrap();
    assert_eq!(
        (
            attached,
            answers(&node, 1, 2),
            node.ctl.status().last,
            index.index()
        ),
        (
            (None, 2, 1),
            vec![Some(applied_at(&node, index)); 2],
            last,
            last
        ),
        "rule: an attempt with the request ID and the command of a pending proposal attaches \
         to it, proposing no second entry, and gets the proposal's terminal result (§7.8 \
         attach, A-5)"
    );
}

#[test]
fn waiters_per_request_are_bounded_and_a_waiter_past_the_bound_is_busy() {
    let (config, mut node) = lone_leader();
    let bound = config.limit(Limit::WaitersPerRequest);
    send(&mut node, 1, put(1, "k", "v"));
    for n in 2..=bound {
        send(&mut node, n, put(1, "k", "v"));
    }
    send(&mut node, bound + 1, put(1, "k", "other"));
    send(&mut node, bound + 2, put(1, "k", "v"));
    send(&mut node, bound + 3, put(1, "k", "other"));
    let past = (answers(&node, bound + 2, bound + 3), waiting(&node));
    sync_all(&mut node);
    let index = executed_at(&node, &request(1)).unwrap();
    let mut expected = vec![Some(applied_at(&node, index)); usize::try_from(bound).unwrap()];
    expected.push(Some(conflict_at(index)));
    assert_eq!(
        (past, answers(&node, 1, bound + 1)),
        (
            (
                vec![Some(refused(Refusal::Busy)); 2],
                usize::try_from(bound + 1).unwrap()
            ),
            expected
        ),
        "rule: a pending proposal takes at most waiters_per_request ({bound}) waiters, attached \
         or behind; one more, of either kind, answers busy (§7.8, §7.11); the waiters within \
         the bound are answered on apply"
    );
}

#[test]
fn an_attached_retry_answers_unknown_outcome_at_its_own_deadline_while_the_proposal_commits() {
    let (config, mut node) = lone_leader();
    let deadline = config.write_deadline().get();
    // Attempt 1 is submitted and its sync is held; the deadlines still run.
    send(&mut node, 1, put(1, "k", "v"));
    tick(&mut node, deadline / 2);
    send(&mut node, 2, put(1, "k", "v"));
    tick(&mut node, deadline - deadline / 2);
    let at_first = answers(&node, 1, 2);
    tick(&mut node, deadline / 2);
    let at_second = answer(&node, 2);
    send(&mut node, 3, put(1, "k", "v"));
    let last = node.ctl.status().last;
    sync_all(&mut node);
    let index = executed_at(&node, &request(1)).unwrap();
    assert_eq!(
        (
            at_first,
            at_second,
            answer(&node, 3),
            node.ctl.status().last
        ),
        (
            vec![Some(refused(Refusal::UnknownOutcome)), None],
            Some(refused(Refusal::UnknownOutcome)),
            Some(applied_at(&node, index)),
            last
        ),
        "rule: the submitted write and a retry attached to it each answer unknown_outcome at \
         their own deadline, never busy: a proposal for their command was submitted (§7.8 \
         attach, A-5); the proposal stays in flight, a later retry attaches to it, and it \
         commits once"
    );
}

#[test]
fn a_different_command_with_a_pending_request_id_waits_then_gets_request_id_conflict() {
    let (_, mut node) = lone_leader();
    send(&mut node, 1, put(1, "k", "v"));
    send(&mut node, 2, put(1, "k", "other"));
    let waited = (answer(&node, 2), waiting(&node), pending(&node));
    sync_all(&mut node);
    let index = executed_at(&node, &request(1)).unwrap();
    assert_eq!(
        (
            waited,
            answers(&node, 1, 2),
            node.ctl
                .state()
                .get(&key("k"))
                .map(|stored| stored.value().clone())
        ),
        (
            (None, 2, 1),
            vec![Some(applied_at(&node, index)), Some(conflict_at(index))],
            Some(value("v"))
        ),
        "rule: a same-ID attempt with another command never attaches: it waits for the \
         pending proposal, then is answered from the committed history, request_id_conflict \
         naming the original, never with the other command's result (§7.8, §7.5 step 3)"
    );
}

#[test]
fn writes_never_handed_to_the_library_answer_busy_at_their_deadline() {
    let (config, mut node) = lone_leader();
    send(&mut node, 1, put(1, "k", "v"));
    send(&mut node, 2, put(1, "k", "other"));
    send(&mut node, 3, put(3, "k", "v"));
    tick(&mut node, config.write_deadline().get());
    let expired = answers(&node, 1, 3);
    sync_all(&mut node);
    assert_eq!(
        (
            expired,
            executed_at(&node, &request(1)).is_some(),
            executed_at(&node, &request(3))
        ),
        (
            vec![
                Some(refused(Refusal::UnknownOutcome)),
                Some(refused(Refusal::Busy)),
                Some(refused(Refusal::Busy))
            ],
            true,
            None
        ),
        "rule: at the deadline the submitted write answers unknown_outcome, and it does \
         commit; one waiting behind another command (2) and one queued behind the sync (3) \
         were never handed to the library: they answer busy, and 3 never executes (§7.8, \
         A-5, §8.4)"
    );
}

// ---------------------------------------------------------------------------------------
// Submitted: its own apply, or unknown_outcome on leadership loss
// ---------------------------------------------------------------------------------------

#[test]
fn a_submitted_write_is_answered_only_when_its_entry_commits_and_applies() {
    let config = config("three-voters");
    let mut cluster = Cluster::new(&config);
    let leader = cluster.elect();
    cluster
        .cut
        .extend(config.voters().filter(|id| *id != leader));
    send(cluster.node(leader), 1, put(1, "k", "v"));
    cluster.settle();
    let node = cluster.get(leader);
    let synced = (
        answer(node, 1),
        pending(node),
        node.ctl.status().commit < node.ctl.status().last,
    );
    cluster.cut.clear();
    cluster.tick_until(50, "the write applies", |cluster| {
        answer(cluster.get(leader), 1).is_some()
    });
    let node = cluster.get(leader);
    let index = executed_at(node, &request(1)).unwrap();
    assert_eq!(
        (synced, answer(node, 1), pending(node)),
        ((None, 1, true), Some(applied_at(node, index)), 0),
        "rule: a submitted write is answered when its own entry commits on a quorum and \
         applies, not when the leader's own sync completes (DUR-1, §7.8)"
    );
}

#[test]
fn losing_the_leadership_answers_submitted_writes_unknown_outcome_and_redirects_the_rest() {
    let config = config("three-voters");
    let mut cluster = Cluster::new(&config);
    let old = cluster.elect();
    let others: Vec<NodeId> = config.voters().filter(|id| *id != old).collect();
    cluster.cut.insert(old);
    send(cluster.node(old), 1, put(1, "k", "v"));
    cluster.settle();
    cluster.held.insert(old);
    send(cluster.node(old), 2, put(2, "k", "v"));
    send(cluster.node(old), 3, put(3, "k", "v"));
    send(cluster.node(old), 4, put(1, "k", "v"));
    send(cluster.node(old), 5, put(1, "k", "other"));
    let admitted = cluster.get(old).ctl.now();
    let quiet = answers(cluster.get(old), 1, 5);
    let new = cluster.elect();
    // The new leader's next message waits at the old one, behind its held sync.
    cluster.cut.remove(&old);
    for _ in 0..10 {
        if cluster.get(old).ctl.queued() > 0 {
            break;
        }
        cluster.tick_only(&others);
    }
    assert!(
        cluster.get(old).ctl.queued() > 0,
        "fixture: a message from the new leader waits at the old one"
    );
    cluster.node(old).complete_sync();
    let node = cluster.get(old);
    let stepped_down = (node.ctl.status().role, node.ctl.now());
    let lost = answers(node, 1, 5);
    let redirect = Some(refused(Refusal::NotLeader { leader: new }));
    assert_eq!(
        (quiet, stepped_down, lost),
        (
            vec![None; 5],
            (Role::Follower, admitted),
            vec![
                Some(refused(Refusal::UnknownOutcome)),
                Some(refused(Refusal::UnknownOutcome)),
                redirect.clone(),
                Some(refused(Refusal::UnknownOutcome)),
                redirect
            ]
        ),
        "rule: when the leader learns of a later term it steps down, and through its status \
         change (not a deadline: no tick passed) its submitted writes (1, 2) and the retry \
         attached to one (4) answer unknown_outcome (§7.8, A-5); the write queued behind its \
         sync (3) and the one behind another command (5) enter F-7 again and are redirected \
         to the new leader {new}"
    );
    cluster.held.clear();
    cluster.settle();
    send(cluster.node(new), 6, put(1, "k", "v"));
    cluster.tick_until(50, "the retry applies on the new leader", |cluster| {
        answer(cluster.get(new), 6).is_some()
    });
    let node = cluster.get(new);
    let index = executed_at(node, &request(1)).unwrap();
    assert_eq!(
        answer(node, 6),
        Some(applied_at(node, index)),
        "rule: unknown_outcome was honest: the old leader's entry never committed, so the \
         retry at the new leader is the request's first execution (A-5, K-12)"
    );
}

#[test]
fn an_isolated_leader_steps_down_and_answers_its_submitted_write_before_the_deadline() {
    let config = config("three-voters");
    let deadline = config.write_deadline().get();
    let mut cluster = Cluster::new(&config);
    let leader = cluster.elect();
    cluster.cut.insert(leader);
    send(cluster.node(leader), 1, put(1, "k", "v"));
    cluster.settle();
    let mut ticks = 0;
    while cluster.get(leader).ctl.status().role == Role::Leader && ticks < deadline {
        cluster.tick_only(&[leader]);
        ticks += 1;
    }
    let node = cluster.get(leader);
    assert_eq!(
        (
            answer(node, 1),
            node.ctl.status().role == Role::Leader,
            ticks < deadline
        ),
        (Some(refused(Refusal::UnknownOutcome)), false, true),
        "rule: a leader that no longer hears from a quorum steps down (check_quorum, §7.10), \
         and its submitted write answers unknown_outcome then, {ticks} ticks after admission, \
         before its deadline of {deadline} (§7.8, A-5)"
    );
}
