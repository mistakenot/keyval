//! A seeded three-node raft-rs cluster in one process: virtual ticks, a seeded in-memory
//! network, scheduled crash-restarts and client writes. Spike-local storage only
//! (`MemStorage`, persisted on each Ready), not keyval's journal.
//!
//! Every source of variation is derived from the run's seed: the network (latency, drops),
//! the crash schedule, and each node's entropy per incarnation (the W02.3 `Entropy` patch).
//! Iteration is always in node-ID or (delivery tick, sequence) order. The same seed and the
//! same build should therefore give a byte-identical trace in every process, which is the
//! property W02.4 measures.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use protobuf::Message as _;
use raft::eraftpb::{ConfState, Entry, EntryType, Message};
use raft::storage::MemStorage;
use raft::{Config, Entropy, RawNode, SplitMix64, StateRole};

use crate::trace::{Trace, hex};

/// The voters.
pub const NODES: [u64; 3] = [1, 2, 3];

/// Virtual ticks per run.
pub const TICKS: u64 = 400;

/// Where each node's election-timeout entropy comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntropyMode {
    /// Derived from the run's seed, the node and its incarnation (the property under test).
    Seeded,
    /// Derived from the wall clock: the negative control, which must make runs diverge.
    Clock,
}

/// What a run exercised; the non-vacuity floor (P10) is checked against these.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Transitions into the leader role.
    pub leaderships: u64,
    /// Leader transitions after the first crash: re-elections.
    pub reelections: u64,
    /// Crashes.
    pub crashes: u64,
    /// Restarts.
    pub restarts: u64,
    /// Client writes applied, summed over nodes.
    pub applied_writes: u64,
    /// Messages sent.
    pub sent: u64,
    /// Messages dropped by the network.
    pub dropped: u64,
}

impl Stats {
    /// Why this run proves nothing, if it doesn't: it must elect, crash, restart,
    /// re-elect and apply client writes.
    pub fn vacuity(&self) -> Option<String> {
        let missing: Vec<&str> = [
            (self.leaderships >= 2, "two leaderships"),
            (self.reelections >= 1, "a re-election after a crash"),
            (
                self.crashes >= 2 && self.restarts >= 2,
                "two crash-restarts",
            ),
            (
                self.applied_writes >= 3,
                "applied client writes on every node",
            ),
        ]
        .into_iter()
        .filter(|(met, _)| !met)
        .map(|(_, what)| what)
        .collect();
        (!missing.is_empty()).then(|| format!("vacuous run: missing {}", missing.join(", ")))
    }
}

/// A finished run.
#[derive(Debug)]
pub struct Outcome {
    /// The canonical trace.
    pub trace: Vec<u8>,
    /// What the run exercised.
    pub stats: Stats,
}

struct Node {
    id: u64,
    incarnation: u64,
    raw: Option<RawNode<MemStorage>>,
    /// Survives crashes: the node's persisted log and hard state.
    disk: MemStorage,
    /// The applied index, treated as durable with the state machine.
    applied: u64,
    role: StateRole,
}

struct InFlight {
    deliver_at: u64,
    seq: u64,
    msg: Message,
}

struct Net {
    rng: SplitMix64,
    seq: u64,
    queue: Vec<InFlight>,
}

struct Schedule {
    crash: [(u64, u64); 2],
    restart: [u64; 2],
}

/// SplitMix64-style mixing of the seed with a domain, node and incarnation, so every stream
/// is independent of every other.
fn stream(seed: u64, domain: u64, node: u64, incarnation: u64) -> u64 {
    let mut g = SplitMix64::new(
        seed ^ domain.rotate_left(17) ^ node.rotate_left(33) ^ incarnation.rotate_left(49),
    );
    g.next_u64()
}

fn entropy(mode: EntropyMode, seed: u64, node: u64, incarnation: u64) -> SplitMix64 {
    match mode {
        EntropyMode::Seeded => SplitMix64::new(stream(seed, 0xE1, node, incarnation)),
        EntropyMode::Clock => {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| u64::from(d.subsec_nanos()) ^ d.as_secs());
            SplitMix64::new(nanos ^ node)
        }
    }
}

fn config(id: u64, applied: u64) -> Config {
    Config {
        id,
        election_tick: 10,
        heartbeat_tick: 2,
        applied,
        max_size_per_msg: 1 << 20,
        max_inflight_msgs: 64,
        max_uncommitted_size: 1 << 22,
        max_committed_size_per_ready: 1 << 20,
        pre_vote: true,
        check_quorum: true,
        ..Config::default()
    }
}

fn start(node: &mut Node, mode: EntropyMode, seed: u64) -> Result<(), String> {
    let logger = slog::Logger::root(slog::Discard, slog::o!());
    let raw = RawNode::new(
        &config(node.id, node.applied),
        node.disk.clone(),
        entropy(mode, seed, node.id, node.incarnation),
        &logger,
    )
    .map_err(|e| {
        format!(
            "node {} incarnation {}: RawNode::new: {e}",
            node.id, node.incarnation
        )
    })?;
    node.raw = Some(raw);
    node.role = StateRole::Follower;
    Ok(())
}

/// Brings a crashed node back as a new incarnation, from its persisted storage only.
fn restart(
    node: &mut Node,
    now: u64,
    mode: EntropyMode,
    seed: u64,
    trace: &mut Trace,
    stats: &mut Stats,
) -> Result<(), String> {
    node.incarnation += 1;
    start(node, mode, seed)?;
    stats.restarts += 1;
    trace.event(
        now,
        node.id,
        node.incarnation,
        &format!("RESTART applied={}", node.applied),
    );
    Ok(())
}

/// Runs the cluster for `seed` and returns its trace.
pub fn run(seed: u64, mode: EntropyMode) -> Result<Outcome, String> {
    let mut sched_rng = SplitMix64::new(stream(seed, 0x5C, 0, 0));
    let crash_at = [
        120 + sched_rng.gen_range(0, 40) as u64,
        250 + sched_rng.gen_range(0, 40) as u64,
    ];
    let schedule = Schedule {
        crash: [(crash_at[0], 0), (crash_at[1], 1)],
        restart: [
            crash_at[0] + 25 + sched_rng.gen_range(0, 20) as u64,
            crash_at[1] + 20 + sched_rng.gen_range(0, 20) as u64,
        ],
    };
    let mut nodes: BTreeMap<u64, Node> = BTreeMap::new();
    for id in NODES {
        let disk = MemStorage::new_with_conf_state(ConfState::from((NODES.to_vec(), vec![])));
        let mut node = Node {
            id,
            incarnation: 0,
            raw: None,
            disk,
            applied: 0,
            role: StateRole::Follower,
        };
        start(&mut node, mode, seed)?;
        nodes.insert(id, node);
    }
    let mut net = Net {
        rng: SplitMix64::new(stream(seed, 0x4E, 0, 0)),
        seq: 0,
        queue: Vec::new(),
    };
    let mut trace = Trace::default();
    let mut stats = Stats::default();
    let mut crashed: [Option<u64>; 2] = [None, None];
    trace.note(&format!(
        "SPIKE seed={seed} nodes=3 ticks={TICKS} entropy={mode:?}"
    ));

    for now in 0..TICKS {
        for (k, &(at, which)) in schedule.crash.iter().enumerate() {
            if now == at {
                let victim = pick_victim(&nodes, which, &mut sched_rng);
                if let Some(node) = nodes.get_mut(&victim) {
                    node.raw = None;
                    stats.crashes += 1;
                    trace.event(now, victim, node.incarnation, "CRASH");
                    crashed[k] = Some(victim);
                }
            }
            if now == schedule.restart[k]
                && let Some(node) = crashed[k].and_then(|id| nodes.get_mut(&id))
            {
                restart(node, now, mode, seed, &mut trace, &mut stats)?;
            }
        }
        if now >= 20 && now % 5 == 0 {
            propose(&mut nodes, now, &mut trace);
        }
        for node in nodes.values_mut() {
            if let Some(raw) = node.raw.as_mut() {
                raw.tick();
            }
        }
        let first_crash = schedule.crash[0].0;
        for node in nodes.values_mut() {
            on_ready(
                node,
                now,
                now > first_crash,
                &mut net,
                &mut trace,
                &mut stats,
            )?;
        }
        deliver(&mut nodes, now, &mut net, &mut trace, &mut stats)?;
        for node in nodes.values_mut() {
            on_ready(
                node,
                now,
                now > first_crash,
                &mut net,
                &mut trace,
                &mut stats,
            )?;
        }
    }
    trace.note(&format!(
        "END leaderships={} reelections={} crashes={} restarts={} applied_writes={} sent={} dropped={}",
        stats.leaderships, stats.reelections, stats.crashes, stats.restarts, stats.applied_writes, stats.sent, stats.dropped
    ));
    Ok(Outcome {
        trace: trace.into_bytes(),
        stats,
    })
}

/// The first crash takes the current leader (so a re-election must follow); the second, a
/// seeded choice among live nodes.
fn pick_victim(nodes: &BTreeMap<u64, Node>, which: u64, rng: &mut SplitMix64) -> u64 {
    let live: Vec<u64> = nodes
        .values()
        .filter(|n| n.raw.is_some())
        .map(|n| n.id)
        .collect();
    let leader = nodes
        .values()
        .find(|n| n.raw.is_some() && n.role == StateRole::Leader)
        .map(|n| n.id);
    match (which, leader) {
        (0, Some(id)) => id,
        _ if live.is_empty() => NODES[0],
        _ => live
            .get(rng.gen_range(0, live.len()))
            .copied()
            .unwrap_or(NODES[0]),
    }
}

/// A client write to the lowest-ID live leader, if any.
fn propose(nodes: &mut BTreeMap<u64, Node>, now: u64, trace: &mut Trace) {
    let leader = nodes
        .values_mut()
        .find(|n| n.raw.is_some() && n.role == StateRole::Leader);
    let Some(node) = leader else {
        trace.note(&format!("t={now:04} NOLEADER"));
        return;
    };
    let data = format!("w{now}").into_bytes();
    let result = node
        .raw
        .as_mut()
        .map(|raw| raw.propose(vec![], data.clone()));
    let verdict = match result {
        Some(Ok(())) => "ok".to_owned(),
        Some(Err(e)) => format!("refused {e}"),
        None => "dead".to_owned(),
    };
    trace.event(
        now,
        node.id,
        node.incarnation,
        &format!("PROPOSE {} {verdict}", hex(&data)),
    );
}

fn send(
    from: &Node,
    msgs: Vec<Message>,
    now: u64,
    net: &mut Net,
    trace: &mut Trace,
    stats: &mut Stats,
) -> Result<(), String> {
    for msg in msgs {
        let bytes = msg.write_to_bytes().map_err(|e| format!("encode: {e}"))?;
        stats.sent += 1;
        let dropped = net.rng.gen_range(0, 100) < 2;
        let latency = 1 + net.rng.gen_range(0, 3) as u64;
        trace.event(
            now,
            from.id,
            from.incarnation,
            &format!(
                "SEND to={} type={:?} {}{}",
                msg.to,
                msg.get_msg_type(),
                if dropped { "DROP " } else { "" },
                hex(&bytes)
            ),
        );
        if dropped {
            stats.dropped += 1;
            continue;
        }
        net.seq += 1;
        net.queue.push(InFlight {
            deliver_at: now + latency,
            seq: net.seq,
            msg,
        });
    }
    Ok(())
}

/// Delivers every message due at `now`, in (delivery tick, send sequence) order. A message to
/// a crashed node is lost; one sent by a node that has since crashed is still delivered, like
/// a packet already on the wire.
fn deliver(
    nodes: &mut BTreeMap<u64, Node>,
    now: u64,
    net: &mut Net,
    trace: &mut Trace,
    stats: &mut Stats,
) -> Result<(), String> {
    net.queue.sort_by_key(|f| (f.deliver_at, f.seq));
    let due: Vec<InFlight> = {
        let split = net.queue.partition_point(|f| f.deliver_at <= now);
        net.queue.drain(..split).collect()
    };
    for flight in due {
        let to = flight.msg.to;
        let Some(node) = nodes.get_mut(&to) else {
            return Err(format!("message to unknown node {to}"));
        };
        match node.raw.as_mut() {
            None => {
                stats.dropped += 1;
                trace.event(
                    now,
                    to,
                    node.incarnation,
                    &format!("LOST seq={}", flight.seq),
                );
            }
            Some(raw) => {
                if let Err(e) = raw.step(flight.msg) {
                    trace.event(
                        now,
                        to,
                        node.incarnation,
                        &format!("STEP-ERR seq={} {e}", flight.seq),
                    );
                }
            }
        }
    }
    Ok(())
}

fn apply(node: &mut Node, entries: Vec<Entry>, now: u64, trace: &mut Trace, stats: &mut Stats) {
    for entry in entries {
        node.applied = entry.index;
        let write = entry.get_entry_type() == EntryType::EntryNormal && !entry.data.is_empty();
        if write {
            stats.applied_writes += 1;
        }
        trace.event(
            now,
            node.id,
            node.incarnation,
            &format!(
                "APPLY index={} term={} type={:?} {}",
                entry.index,
                entry.term,
                entry.get_entry_type(),
                hex(&entry.data)
            ),
        );
    }
}

/// One Ready cycle, in raft-rs 0.7's documented order (`examples/single_mem_node`).
fn on_ready(
    node: &mut Node,
    now: u64,
    after_first_crash: bool,
    net: &mut Net,
    trace: &mut Trace,
    stats: &mut Stats,
) -> Result<(), String> {
    let Some(raw) = node.raw.as_mut() else {
        return Ok(());
    };
    if !raw.has_ready() {
        return Ok(());
    }
    let store = raw.raft.raft_log.store.clone();
    let mut ready = raw.ready();
    let role_change = ready.ss().map(|ss| (ss.raft_state, ss.leader_id));
    let early = ready.take_messages();
    if !ready.snapshot().is_empty() {
        return Err(format!("node {}: unexpected snapshot Ready", node.id));
    }
    let committed = ready.take_committed_entries();
    if !ready.entries().is_empty() {
        store
            .wl()
            .append(ready.entries())
            .map_err(|e| format!("append: {e}"))?;
    }
    let hard = ready.hs().cloned();
    if let Some(hs) = hard.as_ref() {
        store.wl().set_hardstate(hs.clone());
    }
    let persisted = ready.take_persisted_messages();
    let mut light = raw.advance(ready);
    if let Some(commit) = light.commit_index() {
        store.wl().mut_hard_state().set_commit(commit);
    }
    let late = light.take_messages();
    let light_committed = light.take_committed_entries();
    raw.advance_apply();

    if let Some((role, leader)) = role_change {
        if role != node.role {
            if role == StateRole::Leader {
                stats.leaderships += 1;
                if after_first_crash {
                    stats.reelections += 1;
                }
            }
            node.role = role;
        }
        trace.event(
            now,
            node.id,
            node.incarnation,
            &format!("ROLE {role:?} leader={leader}"),
        );
    }
    if let Some(hs) = hard {
        trace.event(
            now,
            node.id,
            node.incarnation,
            &format!(
                "HARD term={} vote={} commit={}",
                hs.term, hs.vote, hs.commit
            ),
        );
    }
    apply(node, committed, now, trace, stats);
    send(node, early, now, net, trace, stats)?;
    send(node, persisted, now, net, trace, stats)?;
    send(node, late, now, net, trace, stats)?;
    apply(node, light_committed, now, trace, stats);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_seed_is_identical_twice_in_process_and_not_vacuous() {
        let a = run(7, EntropyMode::Seeded).unwrap();
        let b = run(7, EntropyMode::Seeded).unwrap();
        assert_eq!(
            a.trace, b.trace,
            "rule: same seed, same build, same trace (P4)"
        );
        assert_eq!(a.stats.vacuity(), None, "stats={:?}", a.stats);
    }

    #[test]
    fn different_seeds_give_different_traces() {
        let a = run(1, EntropyMode::Seeded).unwrap();
        let b = run(2, EntropyMode::Seeded).unwrap();
        assert_ne!(a.trace, b.trace, "rule: the seed drives the run");
    }
}
