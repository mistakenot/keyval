//! W17.1's tests of the simulation composition root (PLAN.md §7.13, §10.2): five real
//! cores over the simulated disk, network, clock and entropy elect, commit and read,
//! crash with unsynced bytes and restart from what their disks kept, ignore a completion
//! from an earlier incarnation, and give the same trace for the same seed however the run
//! is polled. The god view's mirror is audited against the disks, and the trace's client
//! history and first executions are checked line by line.
#![cfg(test)]

// check: SIM-CLUSTER

use std::collections::BTreeSet;
use std::path::Path;

use types::{
    ClientOp, Config, Key, KeyValueLimits, Lifecycle, Mutation, NodeId, RequestId, Response, Role,
    Value,
};

use crate::cluster::{Answer, Cluster, ClusterError};
use crate::disk::{Crash, Fill};
use crate::entropy::MasterSeed;
use crate::net::{LinkProfile, NetConfig, Ppm, QueueBound};
use crate::scheduler::{EventBudget, ScheduleError, VirtualTime};
use crate::trace::ClientId;

const LOCAL: &str = include_str!("../../../config/local.toml");

/// One virtual second, in microseconds.
const SECOND: u64 = 1_000_000;

fn config() -> Config {
    Config::parse(LOCAL, Path::new("config")).unwrap()
}

fn net(peer: LinkProfile) -> NetConfig {
    NetConfig {
        peer,
        client: LinkProfile::reliable(200),
        queue: QueueBound::of(&config()),
        max_in_flight: EventBudget::DEFAULT,
    }
}

fn cluster(seed: u64) -> Cluster {
    Cluster::new(
        config(),
        MasterSeed::new(seed),
        net(LinkProfile::reliable(500)),
        EventBudget::DEFAULT,
    )
    .unwrap()
}

fn after(cluster: &Cluster, micros: u64) -> VirtualTime {
    VirtualTime::from_micros(cluster.now().as_micros() + micros)
}

fn request(n: u8) -> RequestId {
    RequestId::from_bytes([n; 16])
}

fn key(text: &str) -> Key {
    Key::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
}

fn value(text: &str) -> Value {
    Value::parse(text.to_owned(), KeyValueLimits::MAXIMA).unwrap()
}

fn put(n: u8, k: &str, v: &str) -> ClientOp {
    ClientOp::Write {
        request_id: request(n),
        mutation: Mutation::Put {
            key: key(k),
            value: value(v),
        },
    }
}

/// Runs until one running node leads and every running node knows it, at most `within`
/// virtual microseconds.
fn elect(cluster: &mut Cluster, within: u64) -> NodeId {
    let deadline = cluster.now().as_micros() + within;
    while cluster.now().as_micros() < deadline {
        let next = after(cluster, 100_000);
        cluster.run_until(next).unwrap();
        if let Some(leader) = cluster.leader() {
            let agreed = cluster.nodes().all(|id| {
                cluster
                    .controller(id)
                    .is_none_or(|ctl| ctl.status().leader == Some(leader))
            });
            if agreed {
                return leader;
            }
        }
    }
    panic!(
        "fixture: no agreed leader within {within} µs; leader {:?}",
        cluster.leader()
    );
}

/// The answers to `attempt`.
fn answers_to(cluster: &Cluster, attempt: types::AttemptId) -> Vec<&Answer> {
    cluster
        .answers()
        .filter(|answer| answer.attempt == attempt)
        .collect()
}

/// The canonical trace's lines after its first `events` event lines.
fn after_events(trace: &str, events: u64) -> String {
    let mut seen = 0;
    trace
        .lines()
        .skip_while(|line| {
            if line.starts_with("E ") {
                seen += 1;
            }
            seen <= events
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The canonical trace's record lines of `kind`: the fields after the kind.
fn records<'a>(trace: &'a str, kind: &str) -> Vec<Vec<&'a str>> {
    trace
        .lines()
        .map(|line| line.split(' ').collect::<Vec<_>>())
        .filter(|fields| fields.len() > 3 && fields[0] == "R" && fields[2] == kind)
        .map(|fields| fields[3..].to_vec())
        .collect()
}

/// Whether `response` is a first execution of a PUT (not a replay).
fn written(response: &Response) -> bool {
    match response {
        Response::Written { replayed, .. } => !replayed,
        Response::Conflict { .. }
        | Response::Found { .. }
        | Response::NotFound { .. }
        | Response::Page(_)
        | Response::Status { .. }
        | Response::Refused(_) => false,
    }
}

/// The value `node` holds for `k`, if it runs and holds one.
fn held(cluster: &Cluster, node: NodeId, k: &str) -> Option<Value> {
    cluster
        .controller(node)?
        .state()
        .get(&key(k))
        .map(|stored| stored.value().clone())
}

/// Whether `node`'s journal holds bytes its disk has not made durable yet.
fn unsynced(cluster: &Cluster, node: NodeId) -> bool {
    let disk = cluster.disk(node).unwrap();
    disk.journal().map_or(0, |bytes| bytes.len()) > disk.durable().map_or(0, |bytes| bytes.len())
}

/// Whether `node` waits for a sync that is already durable: its completion is pending.
fn durable_but_unacknowledged(cluster: &Cluster, node: NodeId) -> bool {
    cluster
        .controller(node)
        .is_some_and(|ctl| ctl.outstanding_sync().is_some())
        && !unsynced(cluster, node)
}

/// Runs, 50 µs at a time, until `done` holds, at most one virtual second.
fn until(cluster: &mut Cluster, what: &str, done: impl Fn(&Cluster) -> bool) {
    for _ in 0..20_000 {
        if done(cluster) {
            return;
        }
        let next = after(cluster, 50);
        cluster.run_until(next).unwrap();
    }
    panic!("fixture: {what} did not happen within a virtual second");
}

#[test]
fn five_real_cores_elect_one_leader_that_every_node_knows() {
    let mut cluster = cluster(42);
    let leader = elect(&mut cluster, 5 * SECOND);
    let roles: Vec<(NodeId, Role, Lifecycle)> = cluster
        .nodes()
        .map(|id| {
            let ctl = cluster.controller(id).unwrap();
            (id, ctl.status().role, ctl.lifecycle())
        })
        .collect();
    assert_eq!(
        (
            roles.len(),
            roles
                .iter()
                .filter(|(_, role, _)| *role == Role::Leader)
                .count(),
            roles
                .iter()
                .all(|(_, _, lifecycle)| *lifecycle == Lifecycle::Ready),
        ),
        (5, 1, true),
        "rule: five real cores over the simulated adapters elect exactly one leader, node \
         {leader}, and every node is Ready (§10.2 baseline, T-2); roles={roles:?}"
    );
}

#[test]
fn a_put_through_the_leader_commits_on_every_node_and_a_get_reads_it_back() {
    let mut cluster = cluster(42);
    let leader = elect(&mut cluster, 5 * SECOND);
    let client = ClientId::new(1);
    let at = after(&cluster, 1_000);
    let put_attempt = cluster
        .submit(at, client, leader, put(1, "k", "v"))
        .unwrap();
    let next = after(&cluster, SECOND);
    cluster.run_until(next).unwrap();
    let get_at = after(&cluster, 1_000);
    let get_attempt = cluster
        .submit(get_at, client, leader, ClientOp::Get { key: key("k") })
        .unwrap();
    let next = after(&cluster, SECOND);
    cluster.run_until(next).unwrap();
    let put_answers = answers_to(&cluster, put_attempt);
    let found = answers_to(&cluster, get_attempt).iter().any(
        |answer| matches!(&answer.response, Response::Found { value: v, .. } if *v == value("v")),
    );
    let holders = cluster
        .nodes()
        .filter(|id| held(&cluster, *id, "k") == Some(value("v")))
        .count();
    assert_eq!(
        (
            put_answers.len(),
            put_answers.iter().all(|answer| written(&answer.response)),
            found,
            holders
        ),
        (1, true, true, 5),
        "rule: a PUT sent to leader {leader} is answered once as a first execution, applies \
         on all five nodes, and a GET from the leader reads it back (F-1, F-2, §10.2); \
         answers={:?}",
        cluster.answers().collect::<Vec<_>>()
    );
}

#[test]
fn a_crashed_follower_restarts_from_the_bytes_its_disk_kept_and_catches_up() {
    let mut cluster = cluster(7);
    let leader = elect(&mut cluster, 5 * SECOND);
    let follower = cluster.nodes().find(|id| *id != leader).unwrap();
    let client = ClientId::new(1);
    let at = after(&cluster, 1_000);
    cluster
        .submit(at, client, leader, put(1, "a", "1"))
        .unwrap();
    until(&mut cluster, "unsynced bytes on the follower", |c| {
        unsynced(c, follower)
    });
    let crash_at = cluster.now();
    cluster.crash_at(crash_at, follower).unwrap();
    let next = after(&cluster, 1);
    cluster.run_until(next).unwrap();
    assert!(
        cluster.controller(follower).is_none(),
        "fixture: node {follower} is down after its crash"
    );
    for n in 2..=4u8 {
        let at = after(&cluster, 10_000 * u64::from(n));
        cluster
            .submit(at, client, leader, put(n, &format!("k{n}"), "x"))
            .unwrap();
    }
    let restart_at = after(&cluster, 300_000);
    cluster.restart_at(restart_at, follower).unwrap();
    let next = after(&cluster, 3 * SECOND);
    cluster.run_until(next).unwrap();
    cluster.audit_mirrors().unwrap();
    let trace = cluster.trace().canonical();
    let firsts = records(&trace, "first");
    let distinct: BTreeSet<&str> = firsts.iter().map(|fields| fields[0]).collect();
    let ctl = cluster.controller(follower).unwrap();
    let leader_ctl = cluster.controller(leader).unwrap();
    let region = match cluster.crashes().last() {
        Some((_, _, Crash::Kept { region, .. })) => region.get(),
        Some((_, _, Crash::NoJournal | Crash::FileLost)) | None => 0,
    };
    assert_eq!(
        (
            region > 0,
            cluster.incarnation(follower).map(types::Incarnation::get),
            ctl.lifecycle(),
            ctl.state().applied_index(),
            ctl.state().digest(),
            ["a", "k2", "k3", "k4"]
                .iter()
                .all(|k| held(&cluster, follower, k).is_some()),
            (firsts.len(), distinct.len()),
        ),
        (
            true,
            Some(1),
            Lifecycle::Ready,
            leader_ctl.state().applied_index(),
            leader_ctl.state().digest(),
            true,
            (4, 4),
        ),
        "rule: a follower crashed with unsynced bytes in its journal restarts as incarnation 1 \
         from the bytes its disk kept, recovers, replays and catches up to leader {leader}: \
         same applied index and state digest, every write present (DUR-3, DUR-9, §7.7); each \
         of the 4 requests has one first execution in the trace, the replay adding none \
         (§10.2); crashes={:?} firsts={firsts:?}",
        cluster.crashes().collect::<Vec<_>>()
    );
}

#[test]
fn the_leader_crashes_the_others_elect_a_new_one_and_it_rejoins_as_a_follower() {
    let mut cluster = cluster(11);
    let first = elect(&mut cluster, 5 * SECOND);
    let first_term = cluster.controller(first).unwrap().status().term;
    let crash_at = after(&cluster, 1_000);
    cluster.crash_at(crash_at, first).unwrap();
    let next = after(&cluster, 2_000);
    cluster.run_until(next).unwrap();
    let second = elect(&mut cluster, 10 * SECOND);
    let client = ClientId::new(2);
    let at = after(&cluster, 1_000);
    cluster
        .submit(at, client, second, put(9, "after", "crash"))
        .unwrap();
    let restart_at = after(&cluster, 200_000);
    cluster.restart_at(restart_at, first).unwrap();
    let next = after(&cluster, 3 * SECOND);
    cluster.run_until(next).unwrap();
    cluster.audit_mirrors().unwrap();
    let rejoined = cluster.controller(first).unwrap();
    assert_eq!(
        (
            second != first,
            cluster.controller(second).unwrap().status().term > first_term,
            rejoined.status().role,
            rejoined.status().leader,
            held(&cluster, first, "after"),
        ),
        (
            true,
            true,
            Role::Follower,
            Some(second),
            Some(value("crash"))
        ),
        "rule: after leader {first} (term {}) crashes, the others elect node {second} in a \
         later term, a write commits, and the old leader restarts as a follower of the new \
         one holding that write (DUR-3, §10.2)",
        first_term.get()
    );
}

#[test]
fn a_sync_completion_of_an_earlier_incarnation_is_ignored_by_the_next() {
    let mut cluster = cluster(3);
    let leader = elect(&mut cluster, 5 * SECOND);
    let node = cluster.nodes().find(|id| *id != leader).unwrap();
    let client = ClientId::new(1);
    let at = after(&cluster, 1_000);
    cluster
        .submit(at, client, leader, put(1, "s", "1"))
        .unwrap();
    until(&mut cluster, "a durable but unacknowledged sync", |c| {
        durable_but_unacknowledged(c, node)
    });
    let crash_at = cluster.now();
    cluster.crash_at(crash_at, node).unwrap();
    let restart_at = after(&cluster, 1);
    cluster.restart_at(restart_at, node).unwrap();
    let next = after(&cluster, 2 * SECOND);
    cluster.run_until(next).unwrap();
    cluster.audit_mirrors().unwrap();
    let ctl = cluster.controller(node).unwrap();
    assert!(
        ctl.counters().stale_completions >= 1 && ctl.lifecycle() == Lifecycle::Ready,
        "rule: a sync completion scheduled for incarnation 0, delivered after node {node} \
         restarted as incarnation {:?}, is ignored and counted (§7.2, §10.2, B-17); \
         counters={:?}",
        cluster.incarnation(node),
        ctl.counters()
    );
}

#[test]
fn a_lossy_duplicating_network_still_elects_and_commits_on_every_node() {
    let peer = LinkProfile::new(
        200,
        3_000,
        Ppm::new(50_000).unwrap(),
        Ppm::new(50_000).unwrap(),
    )
    .unwrap();
    let mut cluster = Cluster::new(
        config(),
        MasterSeed::new(5),
        net(peer),
        EventBudget::DEFAULT,
    )
    .unwrap();
    let leader = elect(&mut cluster, 20 * SECOND);
    let at = after(&cluster, 1_000);
    cluster
        .submit(at, ClientId::new(1), leader, put(1, "lossy", "ok"))
        .unwrap();
    let next = after(&cluster, 5 * SECOND);
    cluster.run_until(next).unwrap();
    cluster.audit_mirrors().unwrap();
    let counts = cluster.net().counts();
    let holders = cluster
        .nodes()
        .filter(|id| held(&cluster, *id, "lossy") == Some(value("ok")))
        .count();
    assert!(
        holders == 5 && counts.lost > 0 && counts.duplicated > 0,
        "rule: with 5% loss and 5% duplication on peer links, the cores still elect leader \
         {leader} and the write applies on all five nodes; the network lost and duplicated \
         packets on the way (§9.1); holders={holders} counts={counts:?}"
    );
}

/// A scripted run: elect, a PUT, a follower crash with a sync outstanding, a restart.
fn scripted(seed: u64) -> String {
    let mut cluster = cluster(seed);
    let leader = elect(&mut cluster, 5 * SECOND);
    let follower = cluster.nodes().find(|id| *id != leader).unwrap();
    let at = after(&cluster, 1_000);
    cluster
        .submit(at, ClientId::new(1), leader, put(1, "k", "v"))
        .unwrap();
    until(&mut cluster, "unsynced bytes on the follower", |c| {
        unsynced(c, follower)
    });
    let crash_at = cluster.now();
    cluster.crash_at(crash_at, follower).unwrap();
    let restart_at = after(&cluster, 100_000);
    cluster.restart_at(restart_at, follower).unwrap();
    let next = after(&cluster, 2 * SECOND);
    cluster.run_until(next).unwrap();
    cluster.trace().canonical()
}

#[test]
fn the_same_seed_gives_the_same_trace_and_another_seed_another() {
    let first = scripted(7);
    let again = scripted(7);
    let other = scripted(8);
    assert!(
        first == again && first != other && !first.is_empty(),
        "rule: the same seed and build give byte-identical canonical traces, and another seed \
         another execution (T-3, P4); lengths {} {} {}",
        first.len(),
        again.len(),
        other.len()
    );
}

#[test]
fn every_step_records_packets_disk_operations_clients_and_what_the_god_view_saw() {
    let trace = scripted(7);
    let kinds = [
        "packet", "disk", "role", "log", "commit", "applied", "journal", "invoke", "respond",
        "first",
    ];
    let missing: Vec<&str> = kinds
        .iter()
        .copied()
        .filter(|kind| {
            !trace
                .lines()
                .any(|line| line.starts_with("R ") && line.split(' ').nth(2) == Some(*kind))
        })
        .collect();
    let events = [
        "tick", "persist", "complete", "arrive", "request", "crash", "restart",
    ];
    let missing_events: Vec<&str> = events
        .iter()
        .copied()
        .filter(|kind| {
            !trace
                .lines()
                .any(|line| line.starts_with("E ") && line.ends_with(&format!(" {kind}")))
        })
        .collect();
    assert!(
        missing.is_empty() && missing_events.is_empty(),
        "rule: the canonical trace records every event kind and every observation kind the \
         slice produces: packets, disk operations, client invocations and responses, first \
         executions, and the god view's role, log, commit, applied and journal changes \
         (§10.2); missing records {missing:?}, missing events {missing_events:?}"
    );
}

#[test]
fn across_seeds_crashes_lose_unsynced_bytes_and_every_node_still_recovers_and_catches_up() {
    let mut lost = Vec::new();
    for seed in 1..=12u64 {
        let mut cluster = cluster(seed);
        let leader = elect(&mut cluster, 5 * SECOND);
        let follower = cluster.nodes().find(|id| *id != leader).unwrap();
        let at = after(&cluster, 1_000);
        cluster
            .submit(at, ClientId::new(1), leader, put(1, "k", "v"))
            .unwrap();
        until(&mut cluster, "unsynced bytes on the follower", |c| {
            unsynced(c, follower)
        });
        let crash_at = cluster.now();
        cluster.crash_at(crash_at, follower).unwrap();
        let restart_at = after(&cluster, 100_000);
        cluster.restart_at(restart_at, follower).unwrap();
        let next = after(&cluster, 2 * SECOND);
        cluster.run_until(next).unwrap();
        cluster.audit_mirrors().unwrap();
        if let Some((
            _,
            _,
            Crash::Kept {
                prefix,
                region,
                fill,
                ..
            },
        )) = cluster.crashes().last()
            && (prefix < region || *fill != Fill::None)
        {
            lost.push(seed);
        }
        let ctl = cluster.controller(follower).unwrap();
        let leader_ctl = cluster.controller(cluster.leader().unwrap()).unwrap();
        assert_eq!(
            (
                ctl.lifecycle(),
                ctl.state().applied_index(),
                ctl.state().digest(),
                held(&cluster, follower, "k")
            ),
            (
                Lifecycle::Ready,
                leader_ctl.state().applied_index(),
                leader_ctl.state().digest(),
                Some(value("v"))
            ),
            "rule: whatever the crash kept, the follower recovers from it and catches up \
             (DUR-3, DUR-9); seed {seed}; crashes={:?}",
            cluster.crashes().collect::<Vec<_>>()
        );
    }
    assert!(
        !lost.is_empty(),
        "rule: the crash model is exercised, not vacuous: on some seed a crash lost part of \
         the unsynced bytes or left a damaged tail (DUR-9, §10.2); lost on seeds {lost:?}"
    );
}

#[test]
fn a_deposed_leaders_overwritten_suffix_is_never_called_durable_before_its_sync() {
    let mut cluster = cluster(13);
    let old = elect(&mut cluster, 5 * SECOND);
    let others: Vec<NodeId> = cluster.nodes().filter(|id| *id != old).collect();
    cluster
        .net_mut()
        .links_mut()
        .partition(&[&[old], others.as_slice()]);
    let before = cluster.controller(old).unwrap().status().last;
    let at = after(&cluster, 1_000);
    cluster
        .submit(at, ClientId::new(1), old, put(1, "lost", "x"))
        .unwrap();
    until(&mut cluster, "the isolated leader appends the write", |c| {
        c.controller(old).unwrap().status().last > before
    });
    let isolated = cluster.controller(old).unwrap().status().last;
    let deadline = cluster.now().as_micros() + 10 * SECOND;
    let new = loop {
        let next = after(&cluster, 10_000);
        cluster.run_until(next).unwrap();
        if let Some(leader) = cluster.leader().filter(|leader| *leader != old) {
            break leader;
        }
        assert!(
            cluster.now().as_micros() < deadline,
            "fixture: the majority elects no new leader within 10 s"
        );
    };
    let at = after(&cluster, 1_000);
    cluster
        .submit(at, ClientId::new(2), new, put(2, "kept", "y"))
        .unwrap();
    until(&mut cluster, "the new leader applies its write", |c| {
        held(c, new, "kept").is_some()
    });
    cluster.net_mut().links_mut().heal_all();
    let healed = cluster.events().get();
    let mut unsynced_polls = 0;
    for _ in 0..50_000 {
        if held(&cluster, old, "kept").is_some() {
            break;
        }
        let next = after(&cluster, 20);
        cluster.run_until(next).unwrap();
        cluster.audit_mirror(old).unwrap_or_else(|error| {
            panic!(
                "rule: the god view never calls an entry of node {old} synced that its \
                 durable journal does not hold (§10.2 journal positions, K-8); {error}"
            )
        });
        if unsynced(&cluster, old) {
            unsynced_polls += 1;
        }
    }
    let next = after(&cluster, SECOND);
    cluster.run_until(next).unwrap();
    cluster.audit_mirrors().unwrap();
    let old_ctl = cluster.controller(old).unwrap();
    let new_ctl = cluster.controller(new).unwrap();
    let lost_anywhere = cluster
        .nodes()
        .any(|id| held(&cluster, id, "lost").is_some());
    let tail = after_events(&cluster.trace().canonical(), healed);
    let journal = records(&tail, "journal");
    let tag = format!("n{old}/");
    let replaced = journal.iter().any(|fields| {
        let appended: u64 = fields[1].parse().unwrap();
        let synced: u64 = fields[2].parse().unwrap();
        fields[0].starts_with(&tag) && synced < isolated.get() && isolated.get() <= appended
    });
    assert_eq!(
        (
            replaced,
            old_ctl.state().applied_index().get() >= isolated.get(),
            old_ctl.state().digest(),
            held(&cluster, old, "kept"),
            lost_anywhere,
        ),
        (
            true,
            true,
            new_ctl.state().digest(),
            Some(value("y")),
            false
        ),
        "rule: leader {old}, cut off with an uncommitted write at index {}, rejoins leader \
         {new}: a journal record after the heal shows the replacing frame appended past that \
         index with synced below it, it applies past that index without the write, and its \
         mirror, audited against its disk every 20 µs ({unsynced_polls} polls with unsynced \
         bytes), never called a replaced entry durable (§10.2, K-8); journal records after \
         the heal {journal:?}",
        isolated.get()
    );
}

/// A fixed script on absolute virtual times, run to its end by `run_until` in steps of
/// `chunk` µs: its canonical trace. Every node is sent writes once the cluster has had
/// three seconds to elect, so whichever leads commits them; one node crashes and restarts.
fn polled(seed: u64, chunk: u64) -> String {
    let mut cluster = cluster(seed);
    let at = VirtualTime::from_micros;
    let start = 3 * SECOND;
    let nodes: Vec<NodeId> = cluster.nodes().collect();
    for (n, node) in (1u8..).zip(&nodes) {
        let when = at(start + u64::from(n) * 1_000);
        cluster
            .submit(when, ClientId::new(1), *node, put(n, "k", "v"))
            .unwrap();
    }
    let second = nodes[1];
    cluster.crash_at(at(start + 2_500), second).unwrap();
    cluster.restart_at(at(start + 200_000), second).unwrap();
    let end = start + SECOND;
    let mut now = 0;
    while now < end {
        now = (now + chunk).min(end);
        cluster.run_until(at(now)).unwrap();
    }
    cluster.trace().canonical()
}

#[test]
fn how_often_the_driver_polls_never_changes_the_run() {
    let whole = polled(7, 4 * SECOND);
    let event = |kind: &str| {
        whole
            .lines()
            .any(|line| line.starts_with("E ") && line.ends_with(&format!(" {kind}")))
    };
    assert!(
        event("crash") && event("restart") && !records(&whole, "first").is_empty(),
        "fixture: the script crashes and restarts a node and commits a write; trace of {} \
         lines",
        whole.lines().count()
    );
    let runs = [("997 µs", polled(7, 997)), ("150 ms", polled(7, 150_001))];
    for (chunk, trace) in &runs {
        let first_difference = whole.lines().zip(trace.lines()).position(|(a, b)| a != b);
        assert!(
            *trace == whole,
            "rule: polling in steps of {chunk} gives the run one call gives, line for line \
             (§10.2: observing a run never perturbs it; T-3); lines {} and {}, first \
             difference at line {first_difference:?}",
            whole.lines().count(),
            trace.lines().count()
        );
    }
}

#[test]
fn nothing_is_scheduled_before_the_time_the_driver_ran_to() {
    let mut cluster = cluster(42);
    let target = VirtualTime::from_micros(SECOND + 1);
    cluster.run_until(target).unwrap();
    let node = NodeId::new(1).unwrap();
    let early = VirtualTime::from_micros(SECOND);
    let submitted = cluster
        .submit(early, ClientId::new(1), node, put(1, "k", "v"))
        .map(|_| ());
    let crashed = cluster.crash_at(early, node);
    let restarted = cluster.restart_at(early, node);
    let refused = |result: &Result<_, ClusterError>| {
        matches!(
            result,
            Err(ClusterError::Schedule(ScheduleError::InThePast { now, at }))
                if *now == target && *at == early
        )
    };
    assert!(
        cluster.now() == target
            && refused(&submitted)
            && refused(&crashed)
            && refused(&restarted)
            && cluster.crash_at(target, node).is_ok(),
        "rule: after run_until({target:?}) the cluster's time is that target, though no event \
         fell on it, and nothing can be scheduled before it, where its handling would read \
         a later time than its own (§10.2 virtual time); now={:?} submit={submitted:?} \
         crash={crashed:?} restart={restarted:?}",
        cluster.now()
    );
}

#[test]
fn every_attempt_is_invoked_once_answered_once_and_reads_join_no_logical_write() {
    let duplicating = LinkProfile::new(200, 200, Ppm::NEVER, Ppm::ALWAYS).unwrap();
    let mut cluster = Cluster::new(
        config(),
        MasterSeed::new(21),
        NetConfig {
            client: duplicating,
            ..net(LinkProfile::reliable(500))
        },
        EventBudget::DEFAULT,
    )
    .unwrap();
    let leader = elect(&mut cluster, 5 * SECOND);
    let down = cluster.nodes().find(|id| *id != leader).unwrap();
    let crash_at = after(&cluster, 1);
    cluster.crash_at(crash_at, down).unwrap();
    let client = ClientId::new(1);
    let at = after(&cluster, 1_000);
    let write = cluster
        .submit(at, client, leader, put(1, "r", "1"))
        .unwrap();
    let next = after(&cluster, SECOND);
    cluster.run_until(next).unwrap();
    let at = after(&cluster, 1_000);
    let read = cluster
        .submit(at, client, leader, ClientOp::Get { key: key("r") })
        .unwrap();
    let lookup = cluster
        .submit(
            at,
            client,
            leader,
            ClientOp::Status {
                request_id: request(1),
            },
        )
        .unwrap();
    let lost = cluster
        .submit(at, client, down, ClientOp::Get { key: key("r") })
        .unwrap();
    let next = after(&cluster, SECOND);
    cluster.run_until(next).unwrap();
    let trace = cluster.trace().canonical();
    let invocations = records(&trace, "invoke");
    let responses = records(&trace, "respond");
    let row = |attempt: types::AttemptId, rows: &[Vec<&str>]| -> Vec<String> {
        let tag = format!("a{}", attempt.get());
        rows.iter()
            .filter(|fields| fields[1] == tag)
            .map(|fields| fields[..fields.len() - 1].join(" "))
            .collect()
    };
    let write_id = request(1).to_string();
    let expected = [
        (write, leader, write_id.as_str(), 1),
        (read, leader, "-", 1),
        (lookup, leader, "-", 1),
        (lost, down, "-", 0),
    ];
    for (attempt, node, id, answered) in expected {
        let invoked = vec![format!("{client} a{} n{node} {id}", attempt.get())];
        let responded = vec![format!("{client} a{} {id}", attempt.get()); answered];
        assert_eq!(
            (row(attempt, &invocations), row(attempt, &responses)),
            (invoked, responded),
            "rule: attempt {} is invoked once at its target node, under the write's request \
             ID only if it is a write, and its response is recorded once, from the first copy \
             (§10.3 history, K-3, K-10); copies received {}",
            attempt.get(),
            answers_to(&cluster, attempt).len()
        );
    }
    assert_eq!(
        answers_to(&cluster, write).len(),
        2,
        "fixture: every client response is duplicated, so the write's answer arrives twice"
    );
}
