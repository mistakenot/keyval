//! W37.1's tests of the §9.1 fault vocabulary: one focused scenario per fault, each run on
//! the real five-node cluster, each asserting that its fault happened (it is in
//! [`Cluster::faults`]) and what the system under test did about it. Simulation evidence,
//! not proof.
#![cfg(test)]

// check: SIM-FAULTS

use std::path::Path;

use types::{
    AttemptId, ClientOp, Config, Key, KeyValueLimits, Mutation, NodeId, RequestId, Response, Role,
    Value,
};

use crate::cluster::{Cluster, ClusterError, Observed};
use crate::disk::{Crash, Fill, Survival};
use crate::entropy::MasterSeed;
use crate::nemesis::{CrashPoint, Fault, MAX_LOST, NemesisError, Partition};
use crate::net::{LinkProfile, NetConfig, Ppm, QueueBound};
use crate::scheduler::{EventBudget, VirtualTime};
use crate::trace::ClientId;

const LOCAL: &str = include_str!("../../../config/local.toml");

/// One virtual second, in microseconds.
const SECOND: u64 = 1_000_000;

fn config() -> Config {
    Config::parse(LOCAL, Path::new("config")).unwrap()
}

fn net(peer: LinkProfile, client: LinkProfile) -> NetConfig {
    NetConfig {
        peer,
        client,
        queue: QueueBound::of(&config()),
        max_in_flight: EventBudget::DEFAULT,
    }
}

fn cluster_with(seed: u64, peer: LinkProfile, client: LinkProfile) -> Cluster {
    Cluster::new(
        config(),
        MasterSeed::new(seed),
        net(peer, client),
        EventBudget::DEFAULT,
    )
    .unwrap()
}

fn cluster(seed: u64) -> Cluster {
    cluster_with(seed, LinkProfile::reliable(500), LinkProfile::reliable(200))
}

fn node(n: u8) -> NodeId {
    NodeId::new(u64::from(n)).unwrap()
}

fn after(cluster: &Cluster, micros: u64) -> VirtualTime {
    VirtualTime::from_micros(cluster.now().as_micros() + micros)
}

fn run_for(cluster: &mut Cluster, micros: u64) {
    let to = after(cluster, micros);
    cluster.run_until(to).unwrap();
}

fn put(n: u8, k: &str, v: &str) -> ClientOp {
    ClientOp::Write {
        request_id: RequestId::from_bytes([n; 16]),
        mutation: Mutation::Put {
            key: Key::parse(k.to_owned(), KeyValueLimits::MAXIMA).unwrap(),
            value: Value::parse(v.to_owned(), KeyValueLimits::MAXIMA).unwrap(),
        },
    }
}

fn held(cluster: &Cluster, at: NodeId, k: &str) -> Option<Value> {
    cluster
        .controller(at)?
        .state()
        .get(&Key::parse(k.to_owned(), KeyValueLimits::MAXIMA).unwrap())
        .map(|stored| stored.value().clone())
}

/// Runs until one running node leads and every running node knows it.
fn elect(cluster: &mut Cluster, within: u64) -> NodeId {
    let deadline = cluster.now().as_micros() + within;
    while cluster.now().as_micros() < deadline {
        run_for(cluster, 100_000);
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

fn submit(cluster: &mut Cluster, to: NodeId, op: ClientOp) -> AttemptId {
    let at = after(cluster, 1_000);
    cluster.submit(at, ClientId::new(1), to, op).unwrap()
}

fn answered(cluster: &Cluster, attempt: AttemptId) -> usize {
    cluster
        .answers()
        .filter(|answer| answer.attempt == attempt)
        .count()
}

fn unsynced(cluster: &Cluster, at: NodeId) -> bool {
    let disk = cluster.disk(at).unwrap();
    disk.journal().map_or(0, |bytes| bytes.len()) > disk.durable().map_or(0, |bytes| bytes.len())
}

fn until(cluster: &mut Cluster, what: &str, done: impl Fn(&Cluster) -> bool) {
    for _ in 0..20_000 {
        if done(cluster) {
            return;
        }
        run_for(cluster, 50);
    }
    panic!("fixture: {what} did not happen within a virtual second");
}

fn happened(cluster: &Cluster, wanted: &Fault) -> bool {
    cluster.faults().any(|(_, fault)| fault == wanted)
}

/// A cluster with a leader and a PUT in flight, stopped while a follower holds unsynced
/// bytes. Returns the cluster, the leader and that follower.
fn follower_with_unsynced_bytes(seed: u64) -> (Cluster, NodeId, NodeId) {
    let mut cluster = cluster(seed);
    let leader = elect(&mut cluster, 5 * SECOND);
    let follower = cluster.nodes().find(|id| *id != leader).unwrap();
    submit(&mut cluster, leader, put(1, "a", "1"));
    until(&mut cluster, "unsynced bytes on the follower", |c| {
        unsynced(c, follower)
    });
    (cluster, leader, follower)
}

/// Crashes `follower` now with `survival`, and returns what its disk kept.
fn crash_keeping(cluster: &mut Cluster, follower: NodeId, survival: Survival) -> Crash {
    let at = cluster.now();
    cluster
        .inject_at(
            at,
            Fault::Crash {
                node: follower,
                survival,
            },
        )
        .unwrap();
    run_for(cluster, 1);
    assert!(
        cluster.controller(follower).is_none(),
        "fixture: node {follower} is down after its crash"
    );
    cluster.crashes().last().unwrap().2.clone()
}

fn kept(crash: &Crash) -> (u64, u64, Fill) {
    match crash {
        Crash::Kept {
            prefix,
            region,
            fill,
            ..
        } => (prefix.get(), region.get(), *fill),
        Crash::NoJournal | Crash::FileLost => panic!("rule: the journal survives; got {crash:?}"),
    }
}

/// Restarts `follower`, lets the cluster settle and checks it caught up.
fn restart_and_converge(cluster: &mut Cluster, follower: NodeId) {
    let at = cluster.now();
    cluster
        .inject_at(at, Fault::Restart { node: follower })
        .unwrap();
    run_for(cluster, 3 * SECOND);
    cluster.audit_mirrors().unwrap();
    assert_eq!(
        held(cluster, follower, "a"),
        Some(Value::parse("1".to_owned(), KeyValueLimits::MAXIMA).unwrap()),
        "rule: a restarted follower catches up with the committed PUT (DUR-9, F-1)"
    );
}

#[test]
fn crash_keeping_nothing_loses_every_unsynced_byte_and_the_node_recovers() {
    let (mut cluster, _, follower) = follower_with_unsynced_bytes(11);
    let crash = crash_keeping(&mut cluster, follower, Survival::Nothing);
    let (prefix, region, _) = kept(&crash);
    assert_eq!(
        (prefix, region > 0),
        (0, true),
        "rule: Survival::Nothing keeps none of a non-empty unsynced region; {crash:?}"
    );
    restart_and_converge(&mut cluster, follower);
}

#[test]
fn crash_keeping_a_prefix_cuts_the_unsynced_region_mid_append() {
    let (mut cluster, _, follower) = follower_with_unsynced_bytes(12);
    let crash = crash_keeping(&mut cluster, follower, Survival::Partial);
    let (prefix, region, fill) = kept(&crash);
    assert!(
        region > 1 && 0 < prefix && prefix < region && fill == Fill::None,
        "rule: Survival::Partial keeps a strict non-empty prefix and nothing after it; {crash:?}"
    );
    restart_and_converge(&mut cluster, follower);
}

#[test]
fn crash_keeping_a_prefix_then_garbage_leaves_a_torn_tail_that_recovery_discards() {
    let (mut cluster, _, follower) = follower_with_unsynced_bytes(13);
    let crash = crash_keeping(&mut cluster, follower, Survival::PartialThenGarbage);
    let (prefix, region, fill) = kept(&crash);
    assert!(
        0 < prefix && prefix < region && fill == Fill::Garbage,
        "rule: a garbage tail follows the surviving prefix; {crash:?}"
    );
    restart_and_converge(&mut cluster, follower);
}

#[test]
fn crash_keeping_everything_loses_nothing() {
    let (mut cluster, _, follower) = follower_with_unsynced_bytes(14);
    let crash = crash_keeping(&mut cluster, follower, Survival::All);
    let (prefix, region, _) = kept(&crash);
    assert_eq!(
        prefix, region,
        "rule: Survival::All keeps the whole unsynced region; {crash:?}"
    );
    restart_and_converge(&mut cluster, follower);
}

#[test]
fn a_crash_armed_before_the_sync_loses_the_bytes_that_sync_would_have_made_durable() {
    let mut cluster = cluster(21);
    let leader = elect(&mut cluster, 5 * SECOND);
    let follower = cluster.nodes().find(|id| *id != leader).unwrap();
    cluster
        .arm(follower, CrashPoint::BeforeSync, Survival::Nothing)
        .unwrap();
    submit(&mut cluster, leader, put(1, "a", "1"));
    until(&mut cluster, "the armed crash", |c| {
        c.controller(follower).is_none()
    });
    let (prefix, region, _) = kept(&cluster.crashes().last().unwrap().2);
    assert!(
        happened(
            &cluster,
            &Fault::Crash {
                node: follower,
                survival: Survival::Nothing
            }
        ) && prefix == 0
            && region > 0,
        "rule: a BeforeSync crash fires with the append still unsynced; crashes={:?}",
        cluster.crashes().collect::<Vec<_>>()
    );
    restart_and_converge(&mut cluster, follower);
}

#[test]
fn a_crash_armed_after_the_sync_leaves_its_completion_to_arrive_at_the_next_incarnation() {
    let mut cluster = cluster(22);
    let leader = elect(&mut cluster, 5 * SECOND);
    let follower = cluster.nodes().find(|id| *id != leader).unwrap();
    cluster
        .arm(follower, CrashPoint::AfterSync, Survival::Nothing)
        .unwrap();
    submit(&mut cluster, leader, put(1, "a", "1"));
    until(&mut cluster, "the armed crash", |c| {
        c.controller(follower).is_none()
    });
    let (_, region, _) = kept(&cluster.crashes().last().unwrap().2);
    assert_eq!(
        region, 0,
        "rule: after its sync is durable no appended bytes are unsynced"
    );
    let first = cluster.incarnation(follower).unwrap();
    restart_and_converge(&mut cluster, follower);
    let trace = cluster.trace().canonical();
    let prefix = format!("disk n{follower}/{} sync-done ", first.get() + 1);
    let stale = trace
        .lines()
        .filter(|line| line.contains(&prefix) && line.ends_with(&format!(" {}", first.get())))
        .count();
    assert_eq!(
        (
            cluster.incarnation(follower).map(types::Incarnation::get),
            stale >= 1
        ),
        (Some(first.get() + 1), true),
        "rule: the restarted incarnation receives the earlier incarnation's sync completion as a \
         stale one, and the node is unharmed; looked for '{prefix}…{}'",
        first.get()
    );
}

#[test]
fn an_all_node_crash_and_restart_recovers_every_committed_write() {
    let mut cluster = cluster(31);
    let leader = elect(&mut cluster, 5 * SECOND);
    let attempt = submit(&mut cluster, leader, put(1, "a", "1"));
    run_for(&mut cluster, SECOND);
    assert_eq!(answered(&cluster, attempt), 1, "fixture: the PUT committed");
    let at = cluster.now();
    cluster
        .inject_at(
            at,
            Fault::CrashAll {
                survival: Survival::Seeded,
            },
        )
        .unwrap();
    run_for(&mut cluster, 1);
    assert!(
        cluster.nodes().all(|id| cluster.controller(id).is_none()),
        "rule: every node is down after an all-node crash"
    );
    let at = after(&cluster, 1_000);
    let ids: Vec<NodeId> = cluster.nodes().collect();
    for id in &ids {
        cluster.inject_at(at, Fault::Restart { node: *id }).unwrap();
    }
    elect(&mut cluster, 10 * SECOND);
    run_for(&mut cluster, SECOND);
    cluster.audit_mirrors().unwrap();
    let holders = cluster
        .nodes()
        .filter(|id| held(&cluster, *id, "a").is_some())
        .count();
    assert_eq!(
        holders, 5,
        "rule: a write answered before an all-node crash is on every node after it (DUR-9, F-1)"
    );
}

#[test]
fn losing_any_two_of_the_five_nodes_leaves_a_quorum_that_still_commits() {
    let ids: Vec<NodeId> = cluster(41).nodes().collect();
    let mut pairs = 0;
    for (i, first) in ids.iter().enumerate() {
        for second in ids.iter().skip(i + 1) {
            let mut cluster = cluster(41 + pairs);
            let first_leader = elect(&mut cluster, 5 * SECOND);
            let acknowledged = submit(&mut cluster, first_leader, put(9, "a", "1"));
            run_for(&mut cluster, SECOND);
            assert_eq!(
                answered(&cluster, acknowledged),
                1,
                "fixture: a write is acknowledged before the loss; pair {pairs}"
            );
            let at = after(&cluster, 1_000);
            cluster.inject_at(at, Fault::Lose { node: *first }).unwrap();
            cluster
                .inject_at(at, Fault::Lose { node: *second })
                .unwrap();
            run_for(&mut cluster, 1_000_000);
            // The loss is permanent: a restart does nothing.
            let at = after(&cluster, 1_000);
            cluster
                .inject_at(at, Fault::Restart { node: *first })
                .unwrap();
            run_for(&mut cluster, 10_000);
            let leader = elect(&mut cluster, 15 * SECOND);
            let attempt = submit(&mut cluster, leader, put(1, "b", "2"));
            run_for(&mut cluster, 2 * SECOND);
            let survivors_keep_history = cluster
                .nodes()
                .filter(|id| ![*first, *second].contains(id))
                .all(|id| held(&cluster, id, "a").is_some() && held(&cluster, id, "b").is_some());
            assert_eq!(
                (
                    survivors_keep_history,
                    cluster.is_lost(*first),
                    cluster.is_lost(*second),
                    cluster.controller(*first).is_none(),
                    cluster.controller(*second).is_none(),
                    [*first, *second].contains(&leader),
                    answered(&cluster, attempt)
                ),
                (true, true, true, true, true, false, 1),
                "rule: with nodes {first} and {second} lost for good the other three elect and \
                 commit (§9.1); pair {pairs}"
            );
            pairs += 1;
        }
    }
    assert_eq!(pairs, 10, "rule: all ten pairs of five nodes are covered");
}

#[test]
fn losing_a_third_node_is_refused_and_schedules_nothing() {
    let mut cluster = cluster(51);
    let ids: Vec<NodeId> = cluster.nodes().collect();
    let at = after(&cluster, 1_000);
    cluster.inject_at(at, Fault::Lose { node: ids[0] }).unwrap();
    cluster.inject_at(at, Fault::Lose { node: ids[1] }).unwrap();
    cluster.inject_at(at, Fault::Lose { node: ids[1] }).unwrap();
    let refused = cluster.inject_at(at, Fault::Lose { node: ids[2] });
    assert!(
        matches!(
            refused,
            Err(ClusterError::Fault(NemesisError::TooManyLost { node })) if node == ids[2]
        ),
        "rule: a third loss passes MAX_LOST={MAX_LOST}; got {refused:?}"
    );
    assert!(
        refused.is_err_and(|error| !error.is_finding()),
        "rule: a refused fault is a misdriven scenario, not a finding"
    );
    run_for(&mut cluster, SECOND);
    assert_eq!(
        cluster.nodes().filter(|id| cluster.is_lost(*id)).count(),
        2,
        "rule: exactly the two allowed losses happened"
    );
}

/// Elects, partitions with `groups`, and returns the cluster and the minority leader's side
/// after a PUT sent to every node during the split.
fn partitioned(seed: u64, groups: &[&[u8]]) -> (Cluster, Vec<AttemptId>) {
    let mut cluster = cluster(seed);
    elect(&mut cluster, 5 * SECOND);
    let groups: Vec<Vec<NodeId>> = groups
        .iter()
        .map(|group| group.iter().map(|n| node(*n)).collect())
        .collect();
    let refs: Vec<&[NodeId]> = groups.iter().map(Vec::as_slice).collect();
    let at = after(&cluster, 1_000);
    cluster
        .inject_at(at, Fault::Partition(Partition::symmetric(&refs).unwrap()))
        .unwrap();
    run_for(&mut cluster, 10 * SECOND);
    let mut attempts = Vec::new();
    for (n, id) in cluster.nodes().collect::<Vec<_>>().into_iter().enumerate() {
        let n = u8::try_from(n).unwrap() + 1;
        attempts.push(submit(&mut cluster, id, put(n, &format!("k{n}"), "x")));
    }
    run_for(&mut cluster, 5 * SECOND);
    assert!(
        cluster
            .faults()
            .any(|(_, fault)| matches!(fault, Fault::Partition(_))),
        "fixture: the partition fault happened; faults={:?}",
        cluster.faults().collect::<Vec<_>>()
    );
    (cluster, attempts)
}

fn commits(cluster: &Cluster) -> usize {
    cluster
        .observed()
        .filter(|seen| match seen {
            Observed::Generated { response, .. } => matches!(
                response,
                Response::Written {
                    replayed: false,
                    ..
                }
            ),
            Observed::Invoked { .. } | Observed::Delivered { .. } => false,
        })
        .count()
}

#[test]
fn a_symmetric_3_2_partition_commits_only_on_the_majority_side_and_heals() {
    let (mut cluster, attempts) = partitioned(61, &[&[1, 2, 3], &[4, 5]]);
    let minority_commits = [node(4), node(5)]
        .iter()
        .filter(|id| held(&cluster, **id, "k1").is_some())
        .count();
    let minority_writes: Vec<(usize, bool)> = [3_usize, 4]
        .iter()
        .map(|i| {
            let id = if *i == 3 { node(4) } else { node(5) };
            (
                cluster
                    .answers()
                    .filter(|a| {
                        a.attempt == attempts[*i] && matches!(a.response, Response::Written { .. })
                    })
                    .count(),
                held(&cluster, id, &format!("k{}", i + 1)).is_some(),
            )
        })
        .collect();
    assert_eq!(
        (
            commits(&cluster) >= 1,
            minority_commits,
            minority_writes.clone()
        ),
        (true, 0, vec![(0, false), (0, false)]),
        "rule: the three-node side keeps a quorum and the two-node side applies and answers \
         nothing from the split (§9.1); attempts={attempts:?} minority={minority_writes:?}"
    );
    let at = after(&cluster, 1_000);
    cluster.inject_at(at, Fault::Heal).unwrap();
    run_for(&mut cluster, 10 * SECOND);
    cluster.audit_mirrors().unwrap();
    let digests: std::collections::BTreeSet<_> = cluster
        .nodes()
        .map(|id| cluster.controller(id).unwrap().state().digest())
        .collect();
    assert_eq!(
        digests.len(),
        1,
        "rule: after healing every node converges on one state (A-5); digests={digests:?}"
    );
}

#[test]
fn a_symmetric_4_1_partition_isolates_one_node_that_cannot_commit() {
    let (cluster, _) = partitioned(62, &[&[1, 2, 3, 4], &[5]]);
    let isolated = node(5);
    let applied: Vec<bool> = (1..=5)
        .map(|n| held(&cluster, isolated, &format!("k{n}")).is_some())
        .collect();
    assert_eq!(
        (commits(&cluster) >= 1, applied.iter().any(|a| *a)),
        (true, false),
        "rule: the four-node side commits, the isolated node applies none of the split's \
         writes; applied={applied:?}"
    );
}

#[test]
fn a_symmetric_2_2_1_partition_has_no_quorum_and_commits_nothing() {
    let (mut cluster, attempts) = partitioned(63, &[&[1, 2], &[3, 4], &[5]]);
    assert_eq!(
        (
            commits(&cluster),
            cluster
                .answers()
                .filter(|answer| matches!(answer.response, Response::Written { .. }))
                .count()
        ),
        (0, 0),
        "rule: no group of a 2/2/1 split holds three voters, so nothing commits and no write \
         is answered as written (C-1); attempts={attempts:?}"
    );
    let at = after(&cluster, 1_000);
    cluster.inject_at(at, Fault::Heal).unwrap();
    elect(&mut cluster, 15 * SECOND);
    cluster.audit_mirrors().unwrap();
}

#[test]
fn a_directed_cycle_partition_keeps_the_cluster_safe_and_it_recovers_after_healing() {
    let mut cluster = cluster(64);
    elect(&mut cluster, 5 * SECOND);
    let ring: Vec<NodeId> = cluster.nodes().collect();
    let cycle = Partition::directed_cycle(&ring).unwrap();
    assert!(
        matches!(&cycle, Partition::Cuts(cuts) if cuts.len() == 15),
        "rule: of 20 directed links among five nodes only the 5 on the cycle stay open; {cycle:?}"
    );
    let at = after(&cluster, 1_000);
    cluster.inject_at(at, Fault::Partition(cycle)).unwrap();
    run_for(&mut cluster, 10 * SECOND);
    cluster.audit_mirrors().unwrap();
    let leaders = cluster
        .nodes()
        .filter(|id| {
            cluster
                .controller(*id)
                .is_some_and(|ctl| ctl.status().role == Role::Leader)
        })
        .count();
    assert!(
        leaders <= 5,
        "fixture: roles are readable under the cycle; leaders={leaders}"
    );
    let at = after(&cluster, 1_000);
    cluster.inject_at(at, Fault::Heal).unwrap();
    let leader = elect(&mut cluster, 20 * SECOND);
    let attempt = submit(&mut cluster, leader, put(1, "a", "1"));
    run_for(&mut cluster, 2 * SECOND);
    assert_eq!(
        answered(&cluster, attempt),
        1,
        "rule: after healing the cycle the cluster commits again (§9.1)"
    );
}

#[test]
fn an_asymmetric_cut_stops_one_direction_only() {
    let mut cluster = cluster(65);
    let leader = elect(&mut cluster, 5 * SECOND);
    let follower = cluster.nodes().find(|id| *id != leader).unwrap();
    let at = after(&cluster, 1_000);
    cluster
        .inject_at(
            at,
            Fault::Cut {
                from: leader,
                to: follower,
            },
        )
        .unwrap();
    run_for(&mut cluster, 100_000);
    let (allows_out, allows_back) = (
        cluster.net().links().allows(leader, follower),
        cluster.net().links().allows(follower, leader),
    );
    assert_eq!(
        (allows_out, allows_back),
        (false, true),
        "rule: Cut severs only the named direction"
    );
    let at = after(&cluster, 1_000);
    cluster
        .inject_at(
            at,
            Fault::Mend {
                from: leader,
                to: follower,
            },
        )
        .unwrap();
    run_for(&mut cluster, 1_000);
    assert!(
        cluster.net().links().allows(leader, follower),
        "rule: Mend restores the cut direction"
    );
}

#[test]
fn lost_client_responses_leave_the_write_applied_and_the_attempt_unanswered() {
    let client = LinkProfile::new(200, 200, Ppm::ALWAYS, Ppm::NEVER).unwrap();
    let mut cluster = cluster_with(71, LinkProfile::reliable(500), LinkProfile::reliable(200));
    let leader = elect(&mut cluster, 5 * SECOND);
    let at = after(&cluster, 1_000);
    cluster
        .inject_at(
            at,
            Fault::Profiles {
                peer: LinkProfile::reliable(500),
                client,
            },
        )
        .unwrap();
    run_for(&mut cluster, 10_000);
    let attempt = submit(&mut cluster, leader, put(1, "a", "1"));
    run_for(&mut cluster, 2 * SECOND);
    let lost = cluster
        .observed()
        .any(|seen| matches!(seen, Observed::Delivered { delivered: false, attempt: a, .. } if *a == attempt));
    assert_eq!(
        (
            answered(&cluster, attempt),
            held(&cluster, leader, "a").is_some(),
            lost
        ),
        (0, true, true),
        "rule: a lost response means the client hears nothing though the write applied, and \
         the loss is observed (§9.1 loss incl. client responses)"
    );
}

#[test]
fn duplicated_client_responses_reach_the_client_twice_but_are_one_response() {
    let client = LinkProfile::new(200, 200, Ppm::NEVER, Ppm::ALWAYS).unwrap();
    let mut cluster = cluster_with(72, LinkProfile::reliable(500), client);
    let leader = elect(&mut cluster, 5 * SECOND);
    let attempt = submit(&mut cluster, leader, put(1, "a", "1"));
    run_for(&mut cluster, 2 * SECOND);
    let trace = cluster.trace().canonical();
    let responded = trace
        .lines()
        .filter(|line| line.starts_with("R ") && line.contains(" respond "))
        .count();
    assert_eq!(
        (
            answered(&cluster, attempt),
            responded,
            cluster.net().counts().duplicated > 0
        ),
        (2, 1, true),
        "rule: a duplicated response arrives twice and is listed twice but recorded once"
    );
}

#[test]
fn duplicated_peer_messages_apply_each_write_once() {
    let peer = LinkProfile::new(500, 500, Ppm::NEVER, Ppm::ALWAYS).unwrap();
    let mut cluster = cluster_with(73, peer, LinkProfile::reliable(200));
    let leader = elect(&mut cluster, 5 * SECOND);
    let attempt = submit(&mut cluster, leader, put(1, "a", "1"));
    run_for(&mut cluster, 3 * SECOND);
    cluster.audit_mirrors().unwrap();
    let holders = cluster
        .nodes()
        .filter(|id| held(&cluster, *id, "a").is_some())
        .count();
    assert_eq!(
        (
            answered(&cluster, attempt),
            holders,
            cluster.net().counts().duplicated > 0
        ),
        (1, 5, true),
        "rule: every duplicated peer message is harmless; the PUT answers once and applies on \
         all five (§9.1 duplication)"
    );
}

#[test]
fn lost_peer_messages_cost_time_but_not_safety() {
    let peer = LinkProfile::new(500, 500, Ppm::new(250_000).unwrap(), Ppm::NEVER).unwrap();
    let mut cluster = cluster_with(74, peer, LinkProfile::reliable(200));
    let leader = elect(&mut cluster, 30 * SECOND);
    let attempt = submit(&mut cluster, leader, put(1, "a", "1"));
    run_for(&mut cluster, 20 * SECOND);
    cluster.audit_mirrors().unwrap();
    assert!(
        cluster.net().counts().lost > 0 && answered(&cluster, attempt) <= 1,
        "rule: messages were lost and the attempt is answered at most once; counts={:?}",
        cluster.net().counts()
    );
    let at = after(&cluster, 1_000);
    cluster
        .inject_at(
            at,
            Fault::Profiles {
                peer: LinkProfile::reliable(500),
                client: LinkProfile::reliable(200),
            },
        )
        .unwrap();
    run_for(&mut cluster, 10 * SECOND);
    cluster.audit_mirrors().unwrap();
    assert_eq!(
        (
            answered(&cluster, attempt),
            cluster
                .nodes()
                .filter(|id| held(&cluster, *id, "a").is_some())
                .count()
        ),
        (1, 5),
        "rule: once the loss stops the write is answered once and every node holds it"
    );
}

#[test]
fn delayed_and_reordered_peer_messages_still_converge() {
    let peer = LinkProfile::new(100, 40_000, Ppm::NEVER, Ppm::NEVER).unwrap();
    let mut cluster = cluster_with(75, peer, LinkProfile::reliable(200));
    let leader = elect(&mut cluster, 30 * SECOND);
    let attempt = submit(&mut cluster, leader, put(1, "a", "1"));
    run_for(&mut cluster, 10 * SECOND);
    cluster.audit_mirrors().unwrap();
    let holders = cluster
        .nodes()
        .filter(|id| held(&cluster, *id, "a").is_some())
        .count();
    assert_eq!(
        (answered(&cluster, attempt), holders),
        (1, 5),
        "rule: a 100 µs to 40 ms delay range reorders messages and the cluster still commits \
         and converges (§9.1 delay/reorder)"
    );
}

#[test]
fn a_leader_paused_past_the_election_timeout_is_replaced_and_rejoins_as_a_follower() {
    let mut cluster = cluster(81);
    let old = elect(&mut cluster, 5 * SECOND);
    let term = cluster.controller(old).unwrap().status().term;
    let at = after(&cluster, 1_000);
    cluster.inject_at(at, Fault::Pause { node: old }).unwrap();
    run_for(&mut cluster, 2_000);
    assert!(
        cluster.is_paused(old),
        "rule: the node is paused after its Pause fault"
    );
    run_for(&mut cluster, 10 * SECOND);
    let replacement = cluster.leader().filter(|leader| *leader != old);
    assert!(
        replacement.is_some(),
        "rule: the other four elect a new leader while {old} is paused past its election \
         timeout (20 to 40 ticks); leader={:?}",
        cluster.leader()
    );
    let frozen = cluster.controller(old).unwrap().status().term;
    assert_eq!(
        frozen, term,
        "rule: a paused node processes no input, so its term stays {term:?}"
    );
    let at = after(&cluster, 1_000);
    cluster.inject_at(at, Fault::Resume { node: old }).unwrap();
    run_for(&mut cluster, 3 * SECOND);
    cluster.audit_mirrors().unwrap();
    let status = cluster.controller(old).unwrap().status();
    assert_eq!(
        (
            cluster.is_paused(old),
            status.role == Role::Leader,
            status.term > term
        ),
        (false, false, true),
        "rule: the resumed old leader takes its held input, sees the higher term and steps down"
    );
}

#[test]
fn a_crash_clears_a_pause() {
    let mut cluster = cluster(82);
    let leader = elect(&mut cluster, 5 * SECOND);
    let follower = cluster.nodes().find(|id| *id != leader).unwrap();
    let at = after(&cluster, 1_000);
    cluster
        .inject_at(at, Fault::Pause { node: follower })
        .unwrap();
    cluster
        .inject_at(
            after(&cluster, 2_000),
            Fault::Crash {
                node: follower,
                survival: Survival::All,
            },
        )
        .unwrap();
    cluster
        .inject_at(after(&cluster, 3_000), Fault::Restart { node: follower })
        .unwrap();
    run_for(&mut cluster, 1_500);
    assert!(
        cluster.is_paused(follower) && happened(&cluster, &Fault::Pause { node: follower }),
        "fixture: the follower is paused before its crash"
    );
    run_for(&mut cluster, SECOND);
    assert!(
        happened(
            &cluster,
            &Fault::Crash {
                node: follower,
                survival: Survival::All
            }
        ) && happened(&cluster, &Fault::Restart { node: follower }),
        "fixture: the crash and the restart happened"
    );
    assert_eq!(
        (
            cluster.is_paused(follower),
            cluster.controller(follower).is_some()
        ),
        (false, true),
        "rule: a node that crashes while paused restarts running, not paused"
    );
}

#[test]
fn a_fault_schedule_gives_the_same_trace_however_the_run_is_polled() {
    let run = |step: u64| {
        let mut cluster = cluster(91);
        let leader = elect(&mut cluster, 5 * SECOND);
        let follower = cluster.nodes().find(|id| *id != leader).unwrap();
        let base = cluster.now().as_micros();
        let plan = [
            (10_000, Fault::Pause { node: follower }),
            (900_000, Fault::Resume { node: follower }),
            (
                1_000_000,
                Fault::Crash {
                    node: follower,
                    survival: Survival::PartialThenGarbage,
                },
            ),
            (1_500_000, Fault::Restart { node: follower }),
        ];
        for (offset, fault) in plan {
            cluster
                .inject_at(VirtualTime::from_micros(base + offset), fault)
                .unwrap();
        }
        submit(&mut cluster, leader, put(1, "a", "1"));
        let end = base + 4 * SECOND;
        while cluster.now().as_micros() < end {
            let to = VirtualTime::from_micros((cluster.now().as_micros() + step).min(end));
            cluster.run_until(to).unwrap();
        }
        cluster.trace().canonical()
    };
    assert_eq!(
        run(1_000_000),
        run(7_000),
        "rule: the same seed and fault schedule give one trace, however the driver polls (P4)"
    );
}

#[test]
fn faults_naming_unknown_nodes_or_past_times_are_refused() {
    let mut cluster = cluster(92);
    run_for(&mut cluster, 1_000);
    let outside = node(99);
    let at = after(&cluster, 1_000);
    let unknown = cluster.inject_at(at, Fault::Pause { node: outside });
    let past = cluster.inject_at(VirtualTime::from_micros(1), Fault::Heal);
    assert!(
        matches!(
            unknown,
            Err(ClusterError::Fault(NemesisError::UnknownNode { node })) if node == outside
        ) && matches!(past, Err(ClusterError::Schedule(_))),
        "rule: a fault on a node outside the cluster, or in the past, is refused; \
         unknown={unknown:?} past={past:?}"
    );
    let armed = cluster.arm(outside, CrashPoint::BeforeSync, Survival::All);
    assert!(
        matches!(
            armed,
            Err(ClusterError::Fault(NemesisError::UnknownNode { .. }))
        ),
        "rule: arming a crash on an unknown node is refused; {armed:?}"
    );
}

#[test]
fn partition_values_refuse_what_cannot_be_a_partition() {
    let a = node(1);
    let b = node(2);
    let c = node(3);
    let cases = [
        (
            Partition::symmetric(&[&[a], &[]]),
            Some(NemesisError::EmptyGroup),
        ),
        (
            Partition::symmetric(&[&[a, b], &[b, c]]),
            Some(NemesisError::OverlappingGroups { node: b }),
        ),
        (
            Partition::directed_cycle(&[a, b]),
            Some(NemesisError::ShortCycle { len: 2 }),
        ),
        (
            Partition::directed_cycle(&[a, b, a]),
            Some(NemesisError::OverlappingGroups { node: a }),
        ),
        (
            Partition::cuts(&[(a, a)]),
            Some(NemesisError::SelfLink { node: a }),
        ),
        (Partition::symmetric(&[&[a, b], &[c]]), None),
    ];
    for (built, expected) in cases {
        assert_eq!(
            built.err(),
            expected,
            "rule: partition constructors refuse empty, overlapping, short and self-linked input"
        );
    }
}

#[test]
fn a_crash_armed_before_the_reply_leaves_the_write_committed_and_a_retry_replays_it() {
    let mut cluster = cluster(81);
    let leader = elect(&mut cluster, 5 * SECOND);
    cluster
        .arm(leader, CrashPoint::BeforeReply, Survival::All)
        .unwrap();
    let first = submit(&mut cluster, leader, put(7, "a", "1"));
    until(&mut cluster, "the leader's armed crash", |c| {
        c.controller(leader).is_none()
    });
    assert!(
        happened(
            &cluster,
            &Fault::Crash {
                node: leader,
                survival: Survival::All
            }
        ) && answered(&cluster, first) == 0,
        "rule: the leader crashes instead of replying, so the client hears nothing; \
         faults={:?}",
        cluster.faults().collect::<Vec<_>>()
    );
    let successor = elect(&mut cluster, 10 * SECOND);
    let committed = held(&cluster, successor, "a");
    let retry = submit(&mut cluster, successor, put(7, "a", "1"));
    run_for(&mut cluster, 2 * SECOND);
    let replayed = cluster.answers().any(|answer| {
        answer.attempt == retry
            && matches!(answer.response, Response::Written { replayed: true, .. })
    });
    assert_eq!(
        (committed.is_some(), replayed),
        (true, true),
        "rule: a write committed before its reply was lost is applied once, and the retry \
         with the same request ID gets the recorded result (replayed: true)"
    );
}

#[test]
fn an_idle_crash_before_any_write_loses_nothing_and_the_node_still_catches_up() {
    let mut cluster = cluster(82);
    let leader = elect(&mut cluster, 5 * SECOND);
    run_for(&mut cluster, SECOND);
    let victim = cluster.nodes().find(|id| *id != leader).unwrap();
    assert!(
        !unsynced(&cluster, victim),
        "fixture: the idle node holds no unsynced bytes"
    );
    let crash = crash_keeping(&mut cluster, victim, Survival::Nothing);
    assert_eq!(
        kept(&crash).0,
        0,
        "rule: an idle crash has nothing unsynced to keep; {crash:?}"
    );
    submit(&mut cluster, leader, put(1, "a", "1"));
    run_for(&mut cluster, SECOND);
    restart_and_converge(&mut cluster, victim);
}

#[test]
fn an_old_leader_cut_off_in_the_minority_never_acknowledges_and_follows_after_healing() {
    let mut cluster = cluster(66);
    let old = elect(&mut cluster, 5 * SECOND);
    let others: Vec<NodeId> = cluster.nodes().filter(|id| *id != old).collect();
    let partner = others[0];
    let majority = &others[1..];
    let minority = [old, partner];
    let groups: [&[NodeId]; 2] = [&minority, majority];
    let split = Partition::symmetric(&groups).unwrap();
    let at = after(&cluster, 1_000);
    cluster
        .inject_at(at, Fault::Partition(split.clone()))
        .unwrap();
    let stranded = submit(&mut cluster, old, put(1, "x", "old"));
    run_for(&mut cluster, 10 * SECOND);
    let successor = cluster
        .nodes()
        .find(|id| {
            majority.contains(id)
                && cluster
                    .controller(*id)
                    .is_some_and(|ctl| ctl.status().role == Role::Leader)
        })
        .expect("rule: the majority side elects its own leader");
    let fresh = submit(&mut cluster, successor, put(2, "y", "new"));
    run_for(&mut cluster, 3 * SECOND);
    let stranded_written = cluster
        .answers()
        .filter(|a| a.attempt == stranded && matches!(a.response, Response::Written { .. }))
        .count();
    assert_eq!(
        (
            happened(&cluster, &Fault::Partition(split)),
            stranded_written,
            answered(&cluster, fresh),
            held(&cluster, old, "y").is_some()
        ),
        (true, 0, 1, false),
        "rule: the old leader in the two-node minority acknowledges no write, the majority \
         commits, and the minority does not see the new write (§9.1)"
    );
    let at = after(&cluster, 1_000);
    cluster.inject_at(at, Fault::Heal).unwrap();
    run_for(&mut cluster, 10 * SECOND);
    cluster.audit_mirrors().unwrap();
    assert!(
        cluster
            .controller(old)
            .is_some_and(|ctl| ctl.status().role != Role::Leader)
            && held(&cluster, old, "y").is_some()
            && held(&cluster, old, "x").is_none(),
        "rule: after healing the old leader follows, has the new write, and its stranded write \
         never applied"
    );
}

#[test]
fn a_leader_that_sends_but_cannot_receive_acknowledges_nothing() {
    let mut cluster = cluster(67);
    let leader = elect(&mut cluster, 5 * SECOND);
    let others: Vec<NodeId> = cluster.nodes().filter(|id| *id != leader).collect();
    let at = after(&cluster, 1_000);
    for from in &others {
        cluster
            .inject_at(
                at,
                Fault::Cut {
                    from: *from,
                    to: leader,
                },
            )
            .unwrap();
    }
    let attempt = submit(&mut cluster, leader, put(1, "a", "1"));
    run_for(&mut cluster, 10 * SECOND);
    let acknowledged = cluster
        .answers()
        .filter(|a| a.attempt == attempt && matches!(a.response, Response::Written { .. }))
        .count();
    assert_eq!(
        (cluster.faults().count() >= others.len(), acknowledged),
        (true, 0),
        "rule: a leader that hears nobody cannot commit, so it acknowledges no write (§9.1)"
    );
    let at = after(&cluster, 1_000);
    cluster.inject_at(at, Fault::Heal).unwrap();
    run_for(&mut cluster, 10 * SECOND);
    cluster.audit_mirrors().unwrap();
}

#[test]
fn a_leader_that_receives_but_cannot_send_is_replaced() {
    let mut cluster = cluster(68);
    let leader = elect(&mut cluster, 5 * SECOND);
    let others: Vec<NodeId> = cluster.nodes().filter(|id| *id != leader).collect();
    let at = after(&cluster, 1_000);
    for to in &others {
        cluster
            .inject_at(
                at,
                Fault::Cut {
                    from: leader,
                    to: *to,
                },
            )
            .unwrap();
    }
    run_for(&mut cluster, 10 * SECOND);
    let successor = others
        .iter()
        .copied()
        .find(|id| {
            cluster
                .controller(*id)
                .is_some_and(|ctl| ctl.status().role == Role::Leader)
        })
        .expect("rule: followers that hear no heartbeats elect a new leader");
    let attempt = submit(&mut cluster, successor, put(1, "a", "1"));
    run_for(&mut cluster, 3 * SECOND);
    assert_eq!(
        (
            answered(&cluster, attempt),
            cluster.faults().count() >= others.len()
        ),
        (1, true),
        "rule: with the leader unable to send, the others elect and commit (§9.1)"
    );
    let at = after(&cluster, 1_000);
    cluster.inject_at(at, Fault::Heal).unwrap();
    run_for(&mut cluster, 10 * SECOND);
    cluster.audit_mirrors().unwrap();
}

#[test]
fn a_lost_client_response_is_answered_by_replay_when_the_client_retries() {
    let lossy = LinkProfile::new(200, 200, Ppm::ALWAYS, Ppm::NEVER).unwrap();
    let mut cluster = cluster_with(72, LinkProfile::reliable(500), LinkProfile::reliable(200));
    let leader = elect(&mut cluster, 5 * SECOND);
    let at = after(&cluster, 1_000);
    cluster
        .inject_at(
            at,
            Fault::Profiles {
                peer: LinkProfile::reliable(500),
                client: lossy,
            },
        )
        .unwrap();
    run_for(&mut cluster, 10_000);
    let first = submit(&mut cluster, leader, put(5, "a", "1"));
    run_for(&mut cluster, 2 * SECOND);
    let at = after(&cluster, 1_000);
    cluster
        .inject_at(
            at,
            Fault::Profiles {
                peer: LinkProfile::reliable(500),
                client: LinkProfile::reliable(200),
            },
        )
        .unwrap();
    run_for(&mut cluster, 10_000);
    let retry = submit(&mut cluster, leader, put(5, "a", "1"));
    run_for(&mut cluster, 2 * SECOND);
    let replayed = cluster.answers().any(|a| {
        a.attempt == retry && matches!(a.response, Response::Written { replayed: true, .. })
    });
    assert_eq!(
        (answered(&cluster, first), replayed),
        (0, true),
        "rule: the lost reply leaves the client unanswered; its retry with the same request ID \
         gets the recorded result (replayed: true)"
    );
}

#[test]
fn delayed_traffic_from_before_an_election_and_a_restart_does_no_harm() {
    let wide = LinkProfile::new(100, 60_000, Ppm::NEVER, Ppm::new(200_000).unwrap()).unwrap();
    let mut cluster = cluster_with(75, wide, LinkProfile::reliable(200));
    let old = elect(&mut cluster, 30 * SECOND);
    submit(&mut cluster, old, put(1, "a", "1"));
    run_for(&mut cluster, 2_000);
    let at = after(&cluster, 1_000);
    cluster
        .inject_at(
            at,
            Fault::Crash {
                node: old,
                survival: Survival::Seeded,
            },
        )
        .unwrap();
    cluster
        .inject_at(after(&cluster, 20_000), Fault::Restart { node: old })
        .unwrap();
    run_for(&mut cluster, 30 * SECOND);
    let leader = elect(&mut cluster, 60 * SECOND);
    let attempt = submit(&mut cluster, leader, put(2, "b", "2"));
    run_for(&mut cluster, 10 * SECOND);
    cluster.audit_mirrors().unwrap();
    let counts = cluster.net().counts();
    assert!(
        happened(
            &cluster,
            &Fault::Crash {
                node: old,
                survival: Survival::Seeded
            }
        ) && happened(&cluster, &Fault::Restart { node: old })
            && counts.duplicated > 0
            && answered(&cluster, attempt) == 1,
        "rule: with wide delays and duplicates, traffic from before a crash, restart and \
         election arrives late and the cluster still commits once and agrees; counts={counts:?}"
    );
}
