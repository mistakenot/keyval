//! br-vsxx: Ready-loop runs past term 1 (PLAN.md §7.4, DUR-2). Message loss and leader
//! isolation force later-term elections, leader changes, rejected votes and conflicting
//! suffixes replaced through the journal's logical truncation. Every message is still judged
//! against its sender's synced journal. Seeded runs are evidence, not proof (§10.15).
#![cfg(test)]

use std::collections::BTreeMap;

use journal::RaftBatch;
use raft::eraftpb::MessageType;
use types::{Entropy, EntropyStream, EntryPayload, NodeId, Role};

use crate::convert::decode_message;
use crate::ready_loop::{Fed, RaftInput};
use crate::ready_loop_tests::{Cluster, Event, sends_wait_for_syncs};
use crate::test_support::{Journal, Seeded, config};

/// Frames in `journal` whose first entry is at or below the last index before them: a
/// conflicting suffix replaced through the journal's logical truncation.
fn suffix_overwrites(journal: &Journal) -> usize {
    let bytes = journal.bytes();
    let (mut offset, mut last, mut overwrites) = (0, 0_u64, 0);
    while offset < bytes.len() {
        let frame = journal::decode(&bytes[offset..]).unwrap();
        let (entries, _) = RaftBatch::decode(frame.payload()).unwrap().into_parts();
        if let Some(first) = entries.first() {
            if first.index.get() <= last {
                overwrites += 1;
            }
            last = first.index.get() - 1 + u64::try_from(entries.len()).unwrap();
        }
        offset += frame.header().frame_len();
    }
    overwrites
}

/// Vote responses that refuse, sent anywhere in the run.
fn rejected_votes(cluster: &Cluster) -> usize {
    cluster
        .events
        .iter()
        .filter(|event| match event {
            Event::Effect(_, effect) => match effect {
                types::Effect::Send { bytes, .. } => {
                    let message = decode_message(bytes).unwrap();
                    [
                        MessageType::MsgRequestVoteResponse,
                        MessageType::MsgRequestPreVoteResponse,
                    ]
                    .contains(&message.get_msg_type())
                        && message.reject
                }
                types::Effect::Append { .. }
                | types::Effect::Sync { .. }
                | types::Effect::Truncate { .. }
                | types::Effect::CreateJournal
                | types::Effect::SyncDir { .. }
                | types::Effect::Respond { .. }
                | types::Effect::Log(_)
                | types::Effect::Observe(_) => false,
            },
            Event::SyncDone(..) => false,
        })
        .count()
}

/// The applied commands of `id`, in order.
fn applied_commands(cluster: &Cluster, id: NodeId) -> Vec<Vec<u8>> {
    cluster
        .get(id)
        .recorder
        .applied
        .iter()
        .filter_map(|(entry, _)| match &entry.payload {
            EntryPayload::Command(bytes) => Some(bytes.clone()),
            EntryPayload::Empty => None,
        })
        .collect()
}

/// Leaders seen in a run, by term: the oracle "at most one leader per term" and the counters.
#[derive(Default)]
struct Leaders(BTreeMap<u64, NodeId>);

impl Leaders {
    /// Records every node that currently leads; fails on a second leader in one term.
    fn observe(&mut self, cluster: &Cluster, context: &str) {
        for id in cluster.ids() {
            let status = cluster.get(id).lp.status();
            if status.role == Role::Leader {
                let leader = *self.0.entry(status.term.get()).or_insert(id);
                assert_eq!(
                    leader,
                    id,
                    "rule: at most one leader per term (Raft election safety); term {} {context}",
                    status.term.get()
                );
            }
        }
    }
}

/// One fault-injected run over `three-voters`: random ticks, deliveries, losses, sync
/// completions, proposals at whichever node leads, and isolation of the current leader for a
/// while, then a healed phase that lets the cluster converge.
fn faulty_run(seed: u64) -> (Cluster, Leaders) {
    let config = config("three-voters");
    let mut cluster = Cluster::new(&config, seed);
    let ids = cluster.ids();
    let mut rng = Seeded::new(seed ^ 0x0fa1_7e5e);
    let mut draw = |below: usize| {
        let raw = rng.next_u64(EntropyStream::ElectionTimeout);
        usize::try_from(raw % u64::try_from(below).unwrap()).unwrap()
    };
    let mut leaders = Leaders::default();
    let mut isolated: Option<(NodeId, usize)> = None;
    let mut proposals = 0_u32;
    for step in 0..2_500 {
        if isolated.is_some_and(|(_, until)| step >= until) {
            isolated = None;
        }
        match draw(40) {
            0..=13 => {
                let node = ids[draw(ids.len())];
                cluster.feed(node, RaftInput::Tick);
            }
            14..=25 if !cluster.network.is_empty() => {
                let at = draw(cluster.network.len());
                let (from, to, _) = cluster.network[at];
                let cut = isolated.is_some_and(|(node, _)| node == from || node == to);
                if cut || draw(10) == 0 {
                    // Lost: a cut link, or ordinary loss.
                    cluster.network.remove(at);
                } else {
                    cluster.deliver(at);
                }
            }
            26..=34 if !cluster.syncs.is_empty() => {
                let pending: Vec<NodeId> = cluster.syncs.keys().copied().collect();
                cluster.complete_sync(pending[draw(pending.len())]);
            }
            35..=37 => {
                if let Some(leader) = ids
                    .iter()
                    .copied()
                    .find(|&id| cluster.get(id).lp.status().role == Role::Leader)
                {
                    proposals += 1;
                    let command = format!("c-{seed}-{proposals}").into_bytes();
                    cluster.feed(leader, RaftInput::Propose(command));
                }
            }
            _ => {
                if isolated.is_none()
                    && let Some(leader) = ids
                        .iter()
                        .copied()
                        .find(|&id| cluster.get(id).lp.status().role == Role::Leader)
                {
                    isolated = Some((leader, step + 150 + draw(250)));
                }
            }
        }
        leaders.observe(&cluster, &format!("seed={seed} step={step}"));
    }
    // Heal: every link works again; tick everyone until the cluster has one settled leader.
    for round in 0..400 {
        for &id in &ids {
            cluster.feed(id, RaftInput::Tick);
        }
        cluster.settle();
        leaders.observe(&cluster, &format!("seed={seed} heal round {round}"));
        if round > 40
            && ids
                .iter()
                .any(|&id| cluster.get(id).lp.status().role == Role::Leader)
        {
            break;
        }
    }
    (cluster, leaders)
}

#[test]
fn runs_past_term_one_never_release_a_message_on_unsynced_state() {
    let (mut max_term, mut leader_changes, mut rejected, mut overwrites) = (0, 0, 0, 0);
    for seed in 0..12 {
        let (cluster, leaders) = faulty_run(seed);
        assert_eq!(
            cluster.violations,
            Vec::<String>::new(),
            "rule: every message, in every term, is covered by its sender's synced term, vote \
             and log (DUR-2); seed={seed}"
        );
        assert_eq!(
            sends_wait_for_syncs(&cluster.events),
            Ok(()),
            "rule: no node sends while its sync is outstanding (§7.4 step 4); seed={seed}"
        );
        let applied: Vec<Vec<Vec<u8>>> = cluster
            .ids()
            .iter()
            .map(|&id| applied_commands(&cluster, id))
            .collect();
        for (a, b) in applied.iter().zip(applied.iter().skip(1)) {
            let common = a.len().min(b.len());
            assert_eq!(
                a[..common],
                b[..common],
                "rule: across leader changes, nodes apply one committed prefix (C-1); seed={seed}"
            );
        }
        max_term = max_term.max(leaders.0.keys().copied().max().unwrap_or(0));
        leader_changes += leaders.0.len().saturating_sub(1);
        rejected += rejected_votes(&cluster);
        overwrites += cluster
            .ids()
            .iter()
            .map(|&id| suffix_overwrites(&cluster.get(id).journal))
            .sum::<usize>();
    }
    assert!(
        max_term > 1 && leader_changes > 0 && rejected > 0 && overwrites > 0,
        "rule: the runs reach what term 1 never shows (br-vsxx): later-term elections, leader \
         changes, rejected votes and suffix overwrites; max term {max_term}, leader changes \
         {leader_changes}, rejected votes {rejected}, suffix overwrites {overwrites}"
    );
}

/// Ticks `candidates` and settles, dropping every message to or from `cut`, until one of
/// them leads in a term above `above`; returns it.
fn elect_without(cluster: &mut Cluster, candidates: &[NodeId], cut: NodeId, above: u64) -> NodeId {
    for _ in 0..2_000 {
        for &id in candidates {
            cluster.feed(id, RaftInput::Tick);
        }
        for _ in 0..1_000 {
            cluster
                .network
                .retain(|(from, to, _)| *from != cut && *to != cut);
            let ready = (0..cluster.network.len())
                .find(|&at| !cluster.syncs.contains_key(&cluster.network[at].1));
            if let Some(at) = ready {
                cluster.deliver(at);
            } else if let Some(&node) = cluster.syncs.keys().find(|&&id| id != cut) {
                cluster.complete_sync(node);
            } else {
                break;
            }
        }
        if let Some(&leader) = candidates.iter().find(|&&id| {
            let status = cluster.get(id).lp.status();
            status.role == Role::Leader && status.term.get() > above
        }) {
            return leader;
        }
    }
    panic!("no candidate won a term above {above}");
}

/// Elects `leader` and commits a base entry on every node; returns its term.
fn lead_with_base(cluster: &mut Cluster, leader: NodeId) -> u64 {
    while cluster.get(leader).lp.status().role != Role::Leader {
        cluster.feed(leader, RaftInput::Tick);
        cluster.settle();
    }
    assert_eq!(
        cluster.feed(leader, RaftInput::Propose(b"base".to_vec())),
        Fed::Proposed(Ok(()))
    );
    cluster.settle();
    cluster.get(leader).lp.status().term.get()
}

/// Cuts `node` off and has it append `commands` that nobody else receives, each frame synced.
fn append_cut_off(cluster: &mut Cluster, node: NodeId, commands: &[&[u8]]) {
    for command in commands {
        cluster.feed(node, RaftInput::Propose(command.to_vec()));
        cluster
            .network
            .retain(|(from, to, _)| *from != node && *to != node);
        cluster.complete_sync(node);
        cluster
            .network
            .retain(|(from, to, _)| *from != node && *to != node);
    }
}

#[test]
fn a_cut_off_leaders_suffix_is_replaced_and_its_acknowledgement_waits_for_the_sync() {
    let config = config("three-voters");
    let mut cluster = Cluster::new(&config, 9);
    let ids = cluster.ids();
    let old = ids[0];
    let others: Vec<NodeId> = ids[1..].to_vec();
    // A leads and commits a base entry everywhere; then, cut off, it appends two entries
    // nobody else receives.
    let old_term = lead_with_base(&mut cluster, old);
    append_cut_off(&mut cluster, old, &[b"lost-1", b"lost-2"]);
    let old_last = cluster.get(old).lp.node().log().last_index().get();
    assert_eq!(
        suffix_overwrites(&cluster.get(old).journal),
        0,
        "fixture: no overwrite yet"
    );
    // The others elect a new leader in a later term and commit a different entry.
    let new = elect_without(&mut cluster, &others, old, old_term);
    assert_eq!(
        cluster.feed(new, RaftInput::Propose(b"won".to_vec())),
        Fed::Proposed(Ok(()))
    );
    let new_term = cluster.get(new).lp.status().term.get();
    // Heal: A hears from the new leader, steps down, and replaces its suffix.
    let mark = cluster.events.len();
    for _ in 0..50 {
        cluster.feed(new, RaftInput::Tick);
        cluster.settle();
    }
    assert_eq!(
        suffix_overwrites(&cluster.get(old).journal),
        1,
        "rule: the cut-off leader's conflicting suffix is replaced by one frame whose first \
         entry is at or below its previous last index {old_last} (logical truncation, §7.6)"
    );
    let old_sends: Vec<(MessageType, bool, u64)> = cluster.events[mark..]
        .iter()
        .filter_map(|event| match event {
            Event::Effect(from, types::Effect::Send { bytes, .. }) if *from == old => {
                let m = decode_message(bytes).unwrap();
                Some((m.get_msg_type(), m.reject, m.index))
            }
            Event::Effect(..) | Event::SyncDone(..) => None,
        })
        .collect();
    assert!(
        old_sends
            .iter()
            .any(|(kind, reject, _)| *kind == MessageType::MsgAppendResponse && !*reject),
        "fixture: the replaced suffix is acknowledged; sends {old_sends:?}"
    );
    assert_eq!(
        cluster.violations,
        Vec::<String>::new(),
        "rule: the acknowledgement of the replaced suffix waits for the frame that holds it, in \
         the new term (DUR-2)"
    );
    assert_eq!(sends_wait_for_syncs(&cluster.events), Ok(()));
    let (old_log, new_log) = (
        cluster.get(old).lp.node().log(),
        cluster.get(new).lp.node().log(),
    );
    let last = new_log.last_index().get();
    for index in 1..=last {
        let index = types::EntryIndex::new(index).unwrap();
        assert_eq!(
            old_log.term(index),
            new_log.term(index),
            "rule: after the sync, the storage view serves the new leader's entries and terms; \
             index {index}"
        );
    }
    drop((old_log, new_log));
    for id in cluster.ids() {
        let commands = applied_commands(&cluster, id);
        assert_eq!(
            commands,
            vec![b"base".to_vec(), b"won".to_vec()],
            "rule: only committed entries apply: the cut-off suffix never does, and every node \
             applies the same prefix (C-1, C-2); node={id}"
        );
        let status = cluster.get(id).lp.status();
        assert!(
            cluster.get(id).lp.applied() <= status.commit && status.term.get() >= new_term,
            "rule: applied stays within commit, in the new term; node={id} status={status:?}"
        );
    }
}
