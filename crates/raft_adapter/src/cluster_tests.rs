//! W13.5's scripted in-memory clusters (PLAN.md §7.4, D-45): one and three voters, each node a
//! `ReadyLoop` over its own in-memory journal, elect, commit and apply. Every node applies the
//! same committed entries in log order (C-1), and committed entries survive restarts (C-2).
//! Scripted runs are evidence, not proof (§10.15); the simulator's checkers judge these
//! requirements at scale.
#![cfg(test)]

// check: RAFT-CLUSTER

use types::{ByteCount, Config, EntryPayload, LogIndex, NodeId, Role};

use crate::ready_loop::{Fed, RaftInput};
use crate::ready_loop_tests::{Cluster, Member, member_over};
use crate::test_support::{BOUNDS, config, journal_with_genesis};

/// An applied entry: index, term and payload.
type Applied = (u64, u64, EntryPayload);

/// What `member` applied, in order.
fn applied(member: &Member) -> Vec<Applied> {
    member
        .recorder
        .applied
        .iter()
        .map(|(entry, _)| (entry.index.get(), entry.term.get(), entry.payload.clone()))
        .collect()
}

/// The commands among `entries`, in order.
fn commands(entries: &[Applied]) -> Vec<Vec<u8>> {
    entries
        .iter()
        .filter_map(|(_, _, payload)| match payload {
            EntryPayload::Command(bytes) => Some(bytes.clone()),
            EntryPayload::Empty => None,
        })
        .collect()
}

/// The cluster's leaders, by their own account.
fn leaders(cluster: &Cluster) -> Vec<NodeId> {
    cluster
        .ids()
        .into_iter()
        .filter(|&id| cluster.get(id).lp.status().role == Role::Leader)
        .collect()
}

/// Ticks `candidate` (settling the cluster after every tick) until the cluster has a leader;
/// returns it. Asserts there is never more than one.
fn elect(cluster: &mut Cluster, candidate: NodeId) -> NodeId {
    for _ in 0..2_000 {
        cluster.feed(candidate, RaftInput::Tick);
        cluster.settle();
        let leaders = leaders(cluster);
        assert!(
            leaders.len() <= 1,
            "rule: at most one node leads at a time; leaders={leaders:?}"
        );
        if let Some(&leader) = leaders.first() {
            return leader;
        }
    }
    panic!("no leader emerged");
}

/// Proposes each of `commands` at `leader` and settles the cluster after each.
fn commit_all(cluster: &mut Cluster, leader: NodeId, commands: &[&[u8]]) {
    for command in commands {
        assert_eq!(
            cluster.feed(leader, RaftInput::Propose(command.to_vec())),
            Fed::Proposed(Ok(())),
            "fixture: the leader takes the proposal"
        );
        cluster.settle();
    }
}

#[test]
fn a_lone_voter_commits_and_applies_in_log_order_and_again_after_a_restart() {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    let (journal, log) = journal_with_genesis(&config, id);
    let mut first = member_over(&config, id, 7, log, journal.clone());
    first.elect();
    for command in [b"one".as_slice(), b"two", b"three"] {
        first.commit(command);
    }
    let before = applied(&first);
    assert_eq!(
        (
            before
                .iter()
                .map(|(index, _, _)| *index)
                .collect::<Vec<_>>(),
            commands(&before)
        ),
        (
            vec![1, 2, 3, 4],
            vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        ),
        "rule: a lone voter commits and applies its no-op and every command once, in log order \
         (C-1)"
    );
    let bytes = journal.bytes();
    let (_, recovered) = journal::recover(
        &journal,
        ByteCount::new(u64::try_from(bytes.len()).unwrap()),
        &config,
        id,
        BOUNDS,
    )
    .unwrap()
    .into_parts();
    let mut restarted = member_over(&config, id, 8, recovered, journal);
    restarted.poll().unwrap();
    restarted.elect();
    let after = applied(&restarted);
    assert_eq!(
        after[..before.len()],
        before[..],
        "rule: after a restart from its journal, the node applies the same committed entries at \
         the same indices; nothing acknowledged is lost (C-2)"
    );
    assert_eq!(
        restarted.lp.applied().get(),
        u64::try_from(after.len()).unwrap(),
        "rule: the restarted node applied its new term's no-op too"
    );
}

#[test]
fn three_voters_elect_one_leader_and_apply_the_same_entries_in_log_order() {
    let config: Config = config("three-voters");
    let mut cluster = Cluster::new(&config, 1);
    let first = cluster.ids()[0];
    let leader = elect(&mut cluster, first);
    commit_all(&mut cluster, leader, &[b"a", b"b", b"c", b"d", b"e"]);
    let reference = applied(cluster.get(leader));
    assert_eq!(
        commands(&reference),
        ["a", "b", "c", "d", "e"]
            .map(|c| c.as_bytes().to_vec())
            .to_vec(),
        "rule: the leader commits and applies every command once, in proposal order (C-1)"
    );
    for id in cluster.ids() {
        assert_eq!(
            applied(cluster.get(id)),
            reference,
            "rule: every voter applies the same committed entries, at the same indices and \
             terms, in log order (C-1); node={id}"
        );
        for (entry, commit) in &cluster.get(id).recorder.applied {
            assert!(
                entry.index.index() <= *commit,
                "rule: an entry applies only once committed; node={id} entry={} commit={commit}",
                entry.index
            );
        }
    }
    assert_eq!(
        cluster.violations,
        Vec::<String>::new(),
        "rule: no message left on unsynced state (DUR-2)"
    );
}

#[test]
fn committed_entries_survive_a_restart_of_a_majority() {
    let config = config("three-voters");
    let mut cluster = Cluster::new(&config, 2);
    let first = cluster.ids()[0];
    let leader = elect(&mut cluster, first);
    commit_all(&mut cluster, leader, &[b"x1", b"x2", b"x3"]);
    let acknowledged = applied(cluster.get(leader));
    let survivor = cluster.ids().into_iter().find(|&id| id != leader).unwrap();
    let restarted: Vec<NodeId> = cluster
        .ids()
        .into_iter()
        .filter(|&id| id != survivor)
        .collect();
    assert!(
        cluster.network.is_empty() && cluster.syncs.is_empty(),
        "fixture: the cluster is settled, so every frame is synced and the logs are durable"
    );
    // A crash of the leader and one follower: each restarts from its durable log and journal,
    // with nothing applied.
    for (n, &id) in restarted.iter().enumerate() {
        let member = cluster.get(id);
        let log = member.lp.node().log().clone();
        let journal = member.journal.clone();
        let seed = 100 + u64::try_from(n).unwrap();
        let mut fresh = member_over(&config, id, seed, log, journal);
        fresh.poll().unwrap();
        cluster.members.insert(id, fresh);
    }
    let candidate = restarted[0];
    let new_leader = elect(&mut cluster, candidate);
    commit_all(&mut cluster, new_leader, &[b"after"]);
    for id in cluster.ids() {
        let entries = applied(cluster.get(id));
        assert_eq!(
            entries[..acknowledged.len()],
            acknowledged[..],
            "rule: every entry committed before the restart is still applied, at the same index \
             and term, on every node (C-2); node={id} restarted={}",
            restarted.contains(&id)
        );
        assert_eq!(
            commands(&entries).last(),
            Some(&b"after".to_vec()),
            "rule: the restarted cluster keeps committing; node={id}"
        );
    }
    assert!(
        cluster
            .ids()
            .iter()
            .all(|&id| cluster.get(id).lp.status().commit >= LogIndex::new(5).unwrap()),
        "rule: every node learned the new commit index"
    );
    assert_eq!(
        cluster.violations,
        Vec::<String>::new(),
        "rule: no message left on unsynced state (DUR-2)"
    );
}
