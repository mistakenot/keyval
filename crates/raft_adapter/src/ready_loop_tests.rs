//! W13.2's tests of the Ready loop (PLAN.md §7.4, DUR-2, D-28): persist then release, the
//! commit-only rule, one input per turn, drain before the next input, applied reporting.
#![cfg(test)]

// check: RAFT-READY-LOOP

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

use journal::{JournalError, LogBounds, LogicalLog, RaftBatch};
use protobuf::Message as _;
use raft::eraftpb::{self, MessageType};
use types::{
    ByteCount, Config, Consensus, Effect, Entry, EntryPayload, HardState, Incarnation, LogIndex,
    NodeId, Role, StepError, SyncSeq, Term,
};

use crate::admit::Inadmissible;
use crate::convert::decode_message;
use crate::node::{RaftNode, StepRefusal};
use crate::ready_loop::{Apply, Fed, LoopError, RaftInput, ReadyLoop, Synced};
use crate::test_support::{Journal, Seeded, config, empty_log};

/// The incarnation every test node runs in.
pub const INCARNATION: Incarnation = Incarnation::new(4);
/// The first sync number the controller leaves the loop (§7.7 took the ones before).
pub const FIRST_SYNC: SyncSeq = SyncSeq::new(10);

/// Records every applied entry with the commit index it was applied under.
#[derive(Debug, Default)]
pub struct Recorder {
    pub applied: Vec<(Entry, LogIndex)>,
}

impl Apply for Recorder {
    fn apply(&mut self, entry: &Entry, commit: LogIndex, _effects: &mut Vec<Effect>) {
        self.applied.push((entry.clone(), commit));
    }
}

/// One node of a test cluster.
pub struct Member {
    pub lp: ReadyLoop<Journal>,
    pub journal: Journal,
    pub recorder: Recorder,
    /// Every effect executed through [`Member::execute`].
    pub effects: Vec<Effect>,
    /// The journal's length up to the node's last completed sync: what it holds durably.
    /// Every applied entry is judged against it (br-78hc).
    pub synced_len: usize,
    /// The journal's length when this member's outstanding sync was asked for.
    pub syncing: Option<usize>,
}

pub fn member_over(
    config: &Config,
    id: NodeId,
    seed: u64,
    log: LogicalLog,
    journal: Journal,
) -> Member {
    let node = RaftNode::new(
        &config.consensus(id).unwrap(),
        config.voters(),
        log,
        journal.clone(),
        Seeded::new(seed),
        LogIndex::ZERO,
    )
    .unwrap();
    // A journal a node starts over is what recovery read back: durable.
    let synced_len = journal.bytes().len();
    Member {
        lp: ReadyLoop::new(node, INCARNATION, FIRST_SYNC),
        journal,
        recorder: Recorder::default(),
        effects: Vec::new(),
        synced_len,
        syncing: None,
    }
}

pub fn member(config: &Config, id: NodeId, seed: u64) -> Member {
    member_over(config, id, seed, empty_log(), Journal::default())
}

/// What happened in a run, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A node's loop emitted an effect.
    Effect(NodeId, Effect),
    /// A node's sync completed; the effects that follow it were emitted after it.
    SyncDone(NodeId, SyncSeq),
}

/// The DUR-2 rule over a run's events: a node never sends while one of its syncs is
/// outstanding, and has at most one sync outstanding (§7.4 steps 3 and 4).
pub fn sends_wait_for_syncs(events: &[Event]) -> Result<(), String> {
    let mut outstanding: BTreeMap<NodeId, SyncSeq> = BTreeMap::new();
    for (at, event) in events.iter().enumerate() {
        match event {
            Event::SyncDone(node, seq) => {
                if outstanding.remove(node) != Some(*seq) {
                    return Err(format!(
                        "event {at}: node {node} completed sync {seq:?} it never asked for"
                    ));
                }
            }
            Event::Effect(node, effect) => match effect {
                Effect::Sync { seq, .. } => {
                    if let Some(earlier) = outstanding.insert(*node, *seq) {
                        return Err(format!(
                            "event {at}: node {node} asked for sync {seq:?} with {earlier:?} outstanding"
                        ));
                    }
                }
                Effect::Send { to, bytes } => {
                    if let Some(seq) = outstanding.get(node) {
                        let kind = decode_message(bytes).map(|m| m.get_msg_type());
                        return Err(format!(
                            "event {at}: node {node} sent {kind:?} to {to} while sync {seq:?} was outstanding"
                        ));
                    }
                }
                Effect::Append { .. }
                | Effect::Truncate { .. }
                | Effect::CreateJournal
                | Effect::SyncDir { .. }
                | Effect::Respond { .. }
                | Effect::Log(_)
                | Effect::Observe(_) => {}
            },
        }
    }
    Ok(())
}

/// The commit-only rule over a journal: every frame carries new entries, or a hard state
/// whose term or vote changed. The commit index alone is never worth a frame (§7.4 step 3).
pub fn frames_never_persist_a_commit_alone(journal: &Journal) -> Result<usize, String> {
    let bytes = journal.bytes();
    let mut offset = 0;
    let mut persisted = HardState {
        term: Term::ZERO,
        vote: None,
        commit: LogIndex::ZERO,
    };
    let mut frames = 0;
    while offset < bytes.len() {
        let frame =
            journal::decode(&bytes[offset..]).map_err(|e| format!("frame at {offset}: {e:?}"))?;
        let (entries, hard_state) = RaftBatch::decode(frame.payload())
            .map_err(|e| format!("batch at {offset}: {e:?}"))?
            .into_parts();
        if entries.is_empty() {
            let changed = hard_state
                .is_some_and(|state| state.term != persisted.term || state.vote != persisted.vote);
            if !changed {
                return Err(format!(
                    "frame {frames} at {offset} persists only {hard_state:?} over {persisted:?}"
                ));
            }
        }
        if let Some(state) = hard_state {
            persisted = state;
        }
        offset += frame.header().frame_len();
        frames += 1;
    }
    Ok(frames)
}

/// What a node has synced: the hard state and log length in its journal up to its last
/// completed sync.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Durable {
    pub term: u64,
    pub vote: u64,
    pub last: u64,
}

/// The durable state the journal bytes `synced` hold.
pub fn durable_state(synced: &[u8]) -> Durable {
    let mut state = Durable::default();
    let mut offset = 0;
    while offset < synced.len() {
        let frame = journal::decode(&synced[offset..]).unwrap();
        let (entries, hard_state) = RaftBatch::decode(frame.payload()).unwrap().into_parts();
        if let Some(first) = entries.first() {
            state.last = first.index.get() - 1 + u64::try_from(entries.len()).unwrap();
        }
        if let Some(hard_state) = hard_state {
            state.term = hard_state.term.get();
            state.vote = hard_state.vote.map_or(0, NodeId::get);
        }
        offset += frame.header().frame_len();
    }
    state
}

/// The terms of the log the journal bytes `synced` hold, by index from 1, after every
/// logical truncation (§7.6).
pub fn synced_terms(synced: &[u8]) -> Vec<u64> {
    let mut terms = Vec::new();
    let mut offset = 0;
    while offset < synced.len() {
        let frame = journal::decode(&synced[offset..]).unwrap();
        let batch = match frame.header().kind() {
            journal::FrameKind::RaftBatch => true,
            journal::FrameKind::Genesis => false,
        };
        if batch {
            let (entries, _) = RaftBatch::decode(frame.payload()).unwrap().into_parts();
            if let Some(first) = entries.first() {
                terms.truncate(usize::try_from(first.index.get() - 1).unwrap());
                terms.extend(entries.iter().map(|entry| entry.term.get()));
            }
        }
        offset += frame.header().frame_len();
    }
    terms
}

/// The client-success obligation of every entry in `applied`, judged against what its
/// node has synced (the journal bytes `synced`): an entry applies only once its frame is
/// durable at the applying node, with the term its synced log holds (§7.4 step 5 after
/// step 4; K-11; DUR-2; br-78hc). raft-rs keeps it by construction (`next_entries` stops at
/// `min(committed, persisted)`), so this catches an adapter change that applies a Ready's
/// committed entries before its frame syncs.
pub fn unsynced_applies(applied: &[(Entry, LogIndex)], synced: &[u8]) -> Vec<String> {
    let terms = synced_terms(synced);
    applied
        .iter()
        .filter_map(|(entry, commit)| {
            let at = usize::try_from(entry.index.get()).unwrap().checked_sub(1);
            let why = match at.and_then(|at| terms.get(at)) {
                None => "its frame is not synced at the applying node",
                Some(&term) if term != entry.term.get() => {
                    "its term differs from the applying node's synced log"
                }
                Some(_) => return None,
            };
            Some(format!(
                "entry {} (term {}) applied under commit {commit}: {why}; the synced log holds {} \
                 entries",
                entry.index.get(),
                entry.term.get(),
                terms.len()
            ))
        })
        .collect()
}

/// The DUR-2 obligations of one message, judged against what its sender has synced
/// (§7.4: never release a vote grant or an append acknowledgement on unsynced state).
pub fn dur2_violation(from: NodeId, to: NodeId, bytes: &[u8], durable: Durable) -> Option<String> {
    let message = decode_message(bytes).unwrap();
    let kind = message.get_msg_type();
    let pre_vote = [
        MessageType::MsgRequestPreVote,
        MessageType::MsgRequestPreVoteResponse,
    ]
    .contains(&kind);
    let broken = if !pre_vote && message.term > durable.term {
        Some("its term is not synced")
    } else if kind == MessageType::MsgRequestVoteResponse
        && !message.reject
        && (durable.term, durable.vote) != (message.term, to.get())
    {
        Some("a vote grant without the synced vote")
    } else if kind == MessageType::MsgRequestVote
        && (durable.term, durable.vote) != (message.term, from.get())
    {
        Some("a vote request without the synced term and self-vote")
    } else if kind == MessageType::MsgAppendResponse
        && !message.reject
        && durable.last < message.index
    {
        Some("an append acknowledgement beyond the synced log")
    } else if kind == MessageType::MsgAppend
        && durable.last < message.index + u64::try_from(message.entries.len()).unwrap()
    {
        Some("an append of entries the leader has not synced (v1 holds them, §7.4 step 4)")
    } else {
        None
    };
    broken.map(|why| {
        format!(
            "node {from} sent {kind:?} to {to} (term {}, index {}, {} entries, reject {}): {why}; synced {durable:?}",
            message.term,
            message.index,
            message.entries.len(),
            message.reject
        )
    })
}

/// A cluster of loops joined by an in-memory network, with storage that syncs only when a
/// test says so.
pub struct Cluster {
    pub members: BTreeMap<NodeId, Member>,
    /// Messages in flight: sender, destination, bytes.
    pub network: VecDeque<(NodeId, NodeId, Vec<u8>)>,
    /// Each node's outstanding sync.
    pub syncs: BTreeMap<NodeId, SyncSeq>,
    /// Each node's journal length when it asked for its outstanding sync.
    pub covered: BTreeMap<NodeId, usize>,
    /// Each node's synced state.
    pub durable: BTreeMap<NodeId, Durable>,
    /// Every message sent on state its sender had not synced (DUR-2), and every entry
    /// applied before its frame synced at the applying node (§7.4 step 5, K-11; br-78hc).
    pub violations: Vec<String>,
    pub events: Vec<Event>,
}

impl Cluster {
    pub fn new(config: &Config, seed: u64) -> Self {
        let members = config
            .voters()
            .map(|id| {
                (
                    id,
                    member(config, id, seed.wrapping_mul(31).wrapping_add(id.get())),
                )
            })
            .collect();
        Self {
            members,
            network: VecDeque::new(),
            syncs: BTreeMap::new(),
            covered: BTreeMap::new(),
            durable: BTreeMap::new(),
            violations: Vec::new(),
            events: Vec::new(),
        }
    }

    pub fn ids(&self) -> Vec<NodeId> {
        self.members.keys().copied().collect()
    }

    pub fn get(&self, id: NodeId) -> &Member {
        &self.members[&id]
    }

    /// Executes `effects` as the composition root would, and records them.
    pub fn record(&mut self, node: NodeId, effects: Vec<Effect>) {
        for effect in effects {
            match &effect {
                Effect::Append { frame } => self.members[&node].journal.push(frame),
                Effect::Sync { seq, incarnation } => {
                    assert_eq!(
                        *incarnation, INCARNATION,
                        "rule: a sync carries its incarnation (§7.2)"
                    );
                    self.syncs.insert(node, *seq);
                    let len = self.members[&node].journal.bytes().len();
                    self.covered.insert(node, len);
                }
                Effect::Send { to, bytes } => {
                    let durable = self.durable.get(&node).copied().unwrap_or_default();
                    if let Some(violation) = dur2_violation(node, *to, bytes, durable) {
                        self.violations
                            .push(format!("event {}: {violation}", self.events.len()));
                    }
                    self.network.push_back((node, *to, bytes.clone()));
                }
                Effect::Truncate { .. }
                | Effect::CreateJournal
                | Effect::SyncDir { .. }
                | Effect::Respond { .. }
                | Effect::Log(_)
                | Effect::Observe(_) => {
                    panic!("rule: the loop emits only Append, Sync and Send; got {effect:?}")
                }
            }
            self.events.push(Event::Effect(node, effect));
        }
    }

    pub fn feed(&mut self, node: NodeId, input: RaftInput) -> Fed {
        let member = self.members.get_mut(&node).unwrap();
        let applied = member.recorder.applied.len();
        let mut effects = Vec::new();
        let fed = member
            .lp
            .feed(input, &mut member.recorder, &mut effects)
            .unwrap();
        self.record(node, effects);
        self.judge_applies(node, applied);
        fed
    }

    /// Judges the entries `node` applied from its `applied`-th on against its synced
    /// journal, recording each one applied before its frame synced (br-78hc).
    pub fn judge_applies(&mut self, node: NodeId, applied: usize) {
        let found = {
            let member = &self.members[&node];
            let bytes = member.journal.bytes();
            unsynced_applies(
                &member.recorder.applied[applied..],
                &bytes[..member.synced_len],
            )
        };
        let at = self.events.len();
        self.violations.extend(
            found
                .into_iter()
                .map(|violation| format!("event {at}: node {node} {violation}")),
        );
    }

    /// Completes `node`'s outstanding sync; false if it has none.
    pub fn complete_sync(&mut self, node: NodeId) -> bool {
        let Some(seq) = self.syncs.remove(&node) else {
            return false;
        };
        self.events.push(Event::SyncDone(node, seq));
        let covered = self.covered.remove(&node).unwrap();
        let synced = durable_state(&self.members[&node].journal.bytes()[..covered]);
        self.durable.insert(node, synced);
        let member = self.members.get_mut(&node).unwrap();
        member.synced_len = covered;
        let applied = member.recorder.applied.len();
        let mut effects = Vec::new();
        let done = member
            .lp
            .sync_done(seq, INCARNATION, &mut member.recorder, &mut effects)
            .unwrap();
        assert_eq!(
            done,
            Synced::Completed,
            "rule: the outstanding sync completes"
        );
        self.record(node, effects);
        self.judge_applies(node, applied);
        true
    }

    /// Delivers the message in flight at `at`. A node waiting for a sync defers it, and the
    /// message goes back in flight, as a controller would queue it (§7.11).
    pub fn deliver(&mut self, at: usize) {
        let (from, to, bytes) = self.network.remove(at).unwrap();
        if let Fed::Deferred(RaftInput::Peer(bytes)) = self.feed(to, RaftInput::Peer(bytes)) {
            self.network.push_back((from, to, bytes));
        }
    }

    /// Delivers messages and completes syncs, oldest first, until nothing is left.
    pub fn settle(&mut self) {
        for _ in 0..100_000 {
            let ready = (0..self.network.len()).find(|&at| {
                let to = self.network[at].1;
                !self.syncs.contains_key(&to)
            });
            if let Some(at) = ready {
                self.deliver(at);
            } else if let Some(&node) = self.syncs.keys().next() {
                self.complete_sync(node);
            } else {
                return;
            }
        }
        panic!("the cluster did not settle");
    }

    /// The effects `node` emitted since event `mark`.
    pub fn effects_since(&self, mark: usize, node: NodeId) -> Vec<&Effect> {
        self.events[mark..]
            .iter()
            .filter_map(|event| match event {
                Event::Effect(from, effect) if *from == node => Some(effect),
                Event::Effect(..) | Event::SyncDone(..) => None,
            })
            .collect()
    }

    /// The types of the messages in flight from `from`.
    pub fn in_flight_from(&self, from: NodeId) -> Vec<(NodeId, MessageType)> {
        self.network
            .iter()
            .filter(|(sender, _, _)| *sender == from)
            .map(|(_, to, bytes)| (*to, decode_message(bytes).unwrap().get_msg_type()))
            .collect()
    }
}

/// Asserts that every message the cluster sent was covered by its sender's synced state,
/// and every entry applied by its node's synced log.
pub fn assert_synced_obligations(cluster: &Cluster) {
    assert_eq!(
        cluster.violations,
        Vec::<String>::new(),
        "rule: every message is covered by its sender's synced term, vote and log (DUR-2), and \
         every applied entry by its node's synced log (§7.4 step 5, K-11)"
    );
}

/// The kinds of `effects`: `Append`, `Sync`, or the decoded message type of a `Send`.
pub fn kinds(effects: &[&Effect]) -> Vec<String> {
    effects
        .iter()
        .map(|effect| match effect {
            Effect::Append { .. } => "Append".to_owned(),
            Effect::Sync { .. } => "Sync".to_owned(),
            Effect::Send { bytes, .. } => {
                format!("{:?}", decode_message(bytes).unwrap().get_msg_type())
            }
            Effect::Truncate { .. }
            | Effect::CreateJournal
            | Effect::SyncDir { .. }
            | Effect::Respond { .. }
            | Effect::Log(_)
            | Effect::Observe(_) => format!("{effect:?}"),
        })
        .collect()
}

/// Ticks the first voter until it asks for pre-votes, then delivers messages until it is a
/// candidate waiting for the sync of its new term and vote. Returns the candidate.
pub fn start_campaign(cluster: &mut Cluster) -> NodeId {
    let candidate = cluster.ids()[0];
    let mark = cluster.events.len();
    let ticks = (1..=1_000)
        .find(|_| {
            cluster.feed(candidate, RaftInput::Tick);
            !cluster.network.is_empty()
        })
        .unwrap();
    let first = cluster.effects_since(mark, candidate);
    assert!(
        kinds(&first).iter().all(|kind| kind == "MsgRequestPreVote"),
        "rule: a pre-vote changes no term or vote, so it writes no frame and its requests leave \
         at once (§7.4 step 3); after {ticks} ticks: {:?}",
        kinds(&first)
    );
    while !cluster.syncs.contains_key(&candidate) {
        assert!(
            !cluster.network.is_empty(),
            "fixture: the pre-vote round makes a candidate"
        );
        cluster.deliver(0);
    }
    candidate
}

#[test]
fn a_candidates_vote_requests_wait_for_its_new_term_and_vote_to_sync() {
    let mut cluster = Cluster::new(&config("three-voters"), 1);
    let candidate = start_campaign(&mut cluster);
    let mark = cluster.events.len();
    assert!(
        cluster
            .in_flight_from(candidate)
            .iter()
            .all(|(_, kind)| *kind != MessageType::MsgRequestVote),
        "rule: no vote request leaves before the candidate's term and vote are synced (DUR-2); \
         in flight: {:?}",
        cluster.in_flight_from(candidate)
    );
    assert!(
        cluster.get(candidate).lp.node().storage().is_sealed(),
        "rule: the storage view stays sealed until the sync (§7.4 step 4)"
    );
    cluster.complete_sync(candidate);
    let released = cluster.effects_since(mark, candidate);
    assert_eq!(
        kinds(&released),
        vec!["MsgRequestVote"; cluster.ids().len() - 1],
        "rule: after SyncDone every held message is released, in recorded order"
    );
    assert!(
        !cluster.get(candidate).lp.node().storage().is_sealed(),
        "rule: SyncDone unseals the storage view before the release (§7.4 step 4)"
    );
    assert_synced_obligations(&cluster);
}

#[test]
fn a_vote_grant_waits_for_the_voters_term_and_vote_to_sync() {
    let mut cluster = Cluster::new(&config("three-voters"), 2);
    let candidate = start_campaign(&mut cluster);
    cluster.complete_sync(candidate);
    let at = cluster
        .network
        .iter()
        .position(|(_, _, bytes)| {
            decode_message(bytes).unwrap().get_msg_type() == MessageType::MsgRequestVote
        })
        .unwrap();
    let voter = cluster.network[at].1;
    let mark = cluster.events.len();
    cluster.deliver(at);
    assert_eq!(
        kinds(&cluster.effects_since(mark, voter)),
        vec!["Append", "Sync"],
        "rule: a vote grant is held until the voter's new term and vote are synced (DUR-2)"
    );
    let state = cluster.get(voter).lp.node().storage().log().hard_state();
    assert_eq!(
        state.vote,
        Some(candidate),
        "rule: the frame being synced holds the vote; hard state {state:?}"
    );
    let mark = cluster.events.len();
    cluster.complete_sync(voter);
    let released = cluster.effects_since(mark, voter);
    let [Effect::Send { to, bytes }] = released.as_slice() else {
        panic!(
            "rule: after SyncDone the grant is sent; got {:?}",
            kinds(&released)
        );
    };
    let grant = decode_message(bytes).unwrap();
    assert_eq!(
        (*to, grant.get_msg_type(), grant.get_reject()),
        (candidate, MessageType::MsgRequestVoteResponse, false),
        "rule: the released message is the grant, to the candidate"
    );
    assert_synced_obligations(&cluster);
}

#[test]
fn an_append_acknowledgement_waits_for_the_entries_to_sync() {
    let mut cluster = Cluster::new(&config("three-voters"), 3);
    let leader = start_campaign(&mut cluster);
    cluster.complete_sync(leader);
    // Grants come back until the candidate leads and waits for the sync of its first entry.
    while !cluster.syncs.contains_key(&leader) {
        let at = (0..cluster.network.len())
            .find(|&at| !cluster.syncs.contains_key(&cluster.network[at].1))
            .unwrap();
        cluster.deliver(at);
        if let Some(&voter) = cluster.syncs.keys().find(|&&id| id != leader) {
            cluster.complete_sync(voter);
        }
    }
    assert_eq!(
        cluster.get(leader).lp.status().role,
        Role::Leader,
        "fixture: elected"
    );
    assert!(
        cluster
            .in_flight_from(leader)
            .iter()
            .all(|(_, kind)| *kind != MessageType::MsgAppend),
        "rule: v1 holds even the leader's appends until its own entry is synced (§7.4 step 4)"
    );
    cluster.complete_sync(leader);
    let at = cluster
        .network
        .iter()
        .position(|(_, _, bytes)| {
            decode_message(bytes).unwrap().get_msg_type() == MessageType::MsgAppend
        })
        .unwrap();
    let follower = cluster.network[at].1;
    let mark = cluster.events.len();
    cluster.deliver(at);
    assert_eq!(
        kinds(&cluster.effects_since(mark, follower)),
        vec!["Append", "Sync"],
        "rule: an append acknowledgement is held until the entries are synced (DUR-2)"
    );
    let mark = cluster.events.len();
    cluster.complete_sync(follower);
    assert_eq!(
        kinds(&cluster.effects_since(mark, follower)),
        vec!["MsgAppendResponse"],
        "rule: after SyncDone the acknowledgement is sent"
    );
    assert_synced_obligations(&cluster);
}

/// A seeded schedule over a three-voter cluster: ticks, deliveries, sync completions and
/// proposals at the leader, in an order the seed picks, then settled. `small-limits` hands
/// over one committed entry per Ready, so every commit drains (§7.4 step 6).
pub fn seeded_run(seed: u64) -> Cluster {
    let config = config("small-limits");
    let mut cluster = Cluster::new(&config, seed);
    let ids = cluster.ids();
    let mut rng = Seeded::new(seed ^ 0x5eed_5eed);
    let mut draw = |below: usize| {
        let raw = types::Entropy::next_u64(&mut rng, types::EntropyStream::ElectionTimeout);
        usize::try_from(raw % u64::try_from(below).unwrap()).unwrap()
    };
    let mut proposals = 0_u32;
    for _ in 0..600 {
        match draw(20) {
            0..=6 => {
                let node = ids[draw(ids.len())];
                // A node waiting for a sync defers the tick; its controller would count it.
                cluster.feed(node, RaftInput::Tick);
            }
            7..=13 if !cluster.network.is_empty() => {
                let at = draw(cluster.network.len());
                cluster.deliver(at);
            }
            14..=18 if !cluster.syncs.is_empty() => {
                let pending: Vec<NodeId> = cluster.syncs.keys().copied().collect();
                cluster.complete_sync(pending[draw(pending.len())]);
            }
            _ => {
                let leader = ids
                    .iter()
                    .copied()
                    .find(|&id| cluster.get(id).lp.status().role == Role::Leader);
                if let Some(leader) = leader {
                    proposals += 1;
                    cluster.feed(
                        leader,
                        RaftInput::Propose(format!("cmd-{seed}-{proposals}").into_bytes()),
                    );
                }
            }
        }
    }
    cluster.settle();
    cluster
}

#[test]
fn no_message_leaves_on_unsynced_state_in_seeded_cluster_runs() {
    let mut applied_commands = 0;
    for seed in 0..12 {
        let cluster = seeded_run(seed);
        assert_eq!(
            cluster.violations,
            Vec::<String>::new(),
            "rule: every message is covered by its sender's synced term, vote and log (DUR-2), \
             and every applied entry by its node's synced log (§7.4 step 5, K-11); seed={seed}"
        );
        assert_eq!(
            sends_wait_for_syncs(&cluster.events),
            Ok(()),
            "rule: no vote grant, append acknowledgement or other message leaves while its \
             node's sync is outstanding (DUR-2, §7.4 step 4); seed={seed}"
        );
        let applied: Vec<Vec<Entry>> = cluster
            .ids()
            .iter()
            .map(|&id| {
                let member = cluster.get(id);
                for (entry, commit) in &member.recorder.applied {
                    assert!(
                        entry.index.index() <= *commit,
                        "rule: an entry applies only under a commit index covering it; seed={seed} \
                         node={id} entry={entry:?} commit={commit}"
                    );
                }
                assert_eq!(
                    member.lp.applied(),
                    member.lp.node().applied_index(),
                    "rule: applied progress is reported to the library (§7.4 step 7); seed={seed} node={id}"
                );
                member.recorder.applied.iter().map(|(entry, _)| entry.clone()).collect()
            })
            .collect();
        for (a, b) in applied.iter().zip(applied.iter().skip(1)) {
            let common = a.len().min(b.len());
            assert_eq!(
                a[..common],
                b[..common],
                "rule: nodes apply the same committed prefix (the library's commit rule); seed={seed}"
            );
        }
        applied_commands += applied
            .iter()
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| entry.payload != EntryPayload::Empty)
                    .count()
            })
            .max()
            .unwrap_or(0);
    }
    assert!(
        applied_commands > 0,
        "fixture: the seeded runs commit and apply commands, so the rule is not vacuous"
    );
}

#[test]
fn a_frame_never_persists_a_commit_index_alone() {
    let mut frames = 0;
    for seed in 0..6 {
        let cluster = seeded_run(seed);
        for id in cluster.ids() {
            frames += frames_never_persist_a_commit_alone(&cluster.get(id).journal).unwrap_or_else(
                |violation| panic!("rule: the commit index alone is never worth a frame (§7.4 step 3); seed={seed} node={id}: {violation}"),
            );
        }
    }
    assert!(frames > 0, "fixture: the runs write frames");
}

#[test]
fn the_same_seed_gives_the_same_effects() {
    assert_eq!(
        seeded_run(5).events,
        seeded_run(5).events,
        "rule: same seeds, same inputs, same effects in the same order (P4, T-3)"
    );
}

/// Whether `effect` is an `Append`.
pub const fn is_append(effect: &Effect) -> bool {
    match effect {
        Effect::Append { .. } => true,
        Effect::Send { .. }
        | Effect::Sync { .. }
        | Effect::Truncate { .. }
        | Effect::CreateJournal
        | Effect::SyncDir { .. }
        | Effect::Respond { .. }
        | Effect::Log(_)
        | Effect::Observe(_) => false,
    }
}

impl Member {
    /// Executes `effects` as a composition root would: every frame is appended to the
    /// in-memory journal (the storage view reads it back), and every effect is logged.
    pub fn execute(&mut self, effects: Vec<Effect>) {
        for effect in &effects {
            if let Effect::Append { frame } = effect {
                self.journal.push(frame);
            } else if let Effect::Sync { .. } = effect {
                self.syncing = Some(self.journal.bytes().len());
            }
        }
        self.effects.extend(effects);
    }

    /// Asserts that every entry applied from the `applied`-th on is in this node's synced
    /// journal with its term (§7.4 step 5, K-11; br-78hc).
    pub fn judge_applies(&self, applied: usize) {
        let bytes = self.journal.bytes();
        assert_eq!(
            unsynced_applies(&self.recorder.applied[applied..], &bytes[..self.synced_len]),
            Vec::<String>::new(),
            "rule: an entry applies only once its frame is synced at the applying node, with its \
             synced term (§7.4 step 5 after step 4, K-11, DUR-2)"
        );
    }

    pub fn feed(&mut self, input: RaftInput) -> Result<Fed, LoopError> {
        let applied = self.recorder.applied.len();
        let mut effects = Vec::new();
        let fed = self.lp.feed(input, &mut self.recorder, &mut effects);
        self.execute(effects);
        self.judge_applies(applied);
        fed
    }

    pub fn sync_done(
        &mut self,
        seq: SyncSeq,
        incarnation: Incarnation,
    ) -> Result<Synced, LoopError> {
        let applied = self.recorder.applied.len();
        let covered = self.syncing;
        let mut effects = Vec::new();
        let done = self
            .lp
            .sync_done(seq, incarnation, &mut self.recorder, &mut effects);
        if done == Ok(Synced::Completed) {
            // The completed sync covers what the journal held when it was asked for; a
            // sync the same call asks for is recorded by `execute` below.
            self.synced_len = covered.unwrap_or(self.synced_len);
            self.syncing = None;
        }
        self.execute(effects);
        self.judge_applies(applied);
        done
    }

    pub fn poll(&mut self) -> Result<(), LoopError> {
        let applied = self.recorder.applied.len();
        let mut effects = Vec::new();
        let polled = self.lp.poll(&mut self.recorder, &mut effects);
        self.execute(effects);
        self.judge_applies(applied);
        polled
    }

    /// Completes the outstanding sync.
    pub fn complete(&mut self) {
        let seq = self.lp.outstanding_sync().unwrap();
        assert_eq!(self.sync_done(seq, INCARNATION), Ok(Synced::Completed));
    }

    /// Ticks until a sync is outstanding.
    pub fn tick_until_syncing(&mut self) {
        for _ in 0..1_000 {
            if self.lp.outstanding_sync().is_some() {
                return;
            }
            self.feed(RaftInput::Tick).unwrap();
        }
        panic!("no Ready needed a sync");
    }

    /// Ticks and completes syncs until this lone voter leads with nothing outstanding.
    pub fn elect(&mut self) {
        for _ in 0..1_000 {
            if self.lp.outstanding_sync().is_some() {
                self.complete();
            } else if self.lp.status().role == Role::Leader {
                return;
            } else {
                self.feed(RaftInput::Tick).unwrap();
            }
        }
        panic!("a lone voter never led");
    }

    /// Proposes `command` as the leader and completes the frame's sync.
    pub fn commit(&mut self, command: &[u8]) {
        assert_eq!(
            self.feed(RaftInput::Propose(command.to_vec())),
            Ok(Fed::Proposed(Ok(()))),
            "rule: the leader takes a proposal"
        );
        self.complete();
    }
}

/// A lone voter's member and id.
pub fn lone(config: &Config) -> (Member, NodeId) {
    let id = config.voters().next().unwrap();
    (member(config, id, 7), id)
}

#[test]
fn a_lone_voter_applies_each_proposal_in_order_and_reports_it() {
    let (mut member, _) = lone(&config("one-voter"));
    member.elect();
    for command in [b"one".as_slice(), b"two", b"three"] {
        member.commit(command);
    }
    let applied: Vec<(u64, EntryPayload)> = member
        .recorder
        .applied
        .iter()
        .map(|(entry, _)| (entry.index.get(), entry.payload.clone()))
        .collect();
    assert_eq!(
        applied,
        vec![
            (1, EntryPayload::Empty),
            (2, EntryPayload::Command(b"one".to_vec())),
            (3, EntryPayload::Command(b"two".to_vec())),
            (4, EntryPayload::Command(b"three".to_vec())),
        ],
        "rule: committed entries apply once each, in index order (§7.4 step 5)"
    );
    assert_eq!(
        (
            member.lp.applied().get(),
            member.lp.node().applied_index().get()
        ),
        (4, 4),
        "rule: applied progress is reported for the entries applied (§7.4 step 7)"
    );
    let syncs: Vec<(u64, Incarnation)> = member
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Sync { seq, incarnation } => Some((seq.get(), *incarnation)),
            Effect::Send { .. }
            | Effect::Append { .. }
            | Effect::Truncate { .. }
            | Effect::CreateJournal
            | Effect::SyncDir { .. }
            | Effect::Respond { .. }
            | Effect::Log(_)
            | Effect::Observe(_) => None,
        })
        .collect();
    let expected: Vec<(u64, Incarnation)> = (0..u64::try_from(syncs.len()).unwrap())
        .map(|n| (FIRST_SYNC.get() + n, INCARNATION))
        .collect();
    assert_eq!(
        syncs, expected,
        "rule: sync numbers start where the controller left off and strictly increase (§7.2)"
    );
    let pairs: Vec<(bool, bool)> = member
        .effects
        .windows(2)
        .filter(|pair| is_append(&pair[0]))
        .map(|pair| (is_append(&pair[0]), !is_append(&pair[1])))
        .collect();
    assert!(
        !pairs.is_empty() && pairs.iter().all(|&(_, sync_follows)| sync_follows),
        "rule: each frame is followed by its sync, one sync per frame; effects={:?}",
        kinds(&member.effects.iter().collect::<Vec<_>>())
    );
}

#[test]
fn the_apply_check_names_an_unsynced_entry_and_a_term_the_synced_log_does_not_hold() {
    let (mut member, _) = lone(&config("one-voter"));
    member.elect();
    member.commit(b"one");
    let synced = member.journal.bytes();
    let applied = member.recorder.applied.clone();
    assert_eq!(
        unsynced_applies(&applied, &synced),
        Vec::<String>::new(),
        "fixture: every entry the lone voter applied is in its synced journal"
    );
    let (last, commit) = applied.last().unwrap().clone();
    let other_term = Entry {
        term: Term::new(last.term.get() + 1).unwrap(),
        ..last
    };
    let found = unsynced_applies(&[(other_term, commit)], &synced);
    assert!(
        found.len() == 1 && found[0].contains("its term differs"),
        "rule: an applied entry whose term the synced log does not hold is caught; {found:?}"
    );
    let found = unsynced_applies(&applied, &[]);
    assert!(
        found.len() == applied.len() && found.iter().all(|v| v.contains("not synced")),
        "rule: with nothing synced, every applied entry is caught (§7.4 step 5); {found:?}"
    );
}

#[test]
#[should_panic(expected = "K-11")]
fn an_entry_applied_before_its_frame_syncs_fails_the_harness_check() {
    // The harness is under test here, not the loop. The lone voter applies its second
    // proposal as that proposal's sync completes. Pretending the sync covered only the
    // frames before it is exactly an apply ahead of its frame's sync (§7.4 step 5 before
    // step 4), and the check must fire (br-78hc).
    let (mut member, _) = lone(&config("one-voter"));
    member.elect();
    member.commit(b"one");
    let before = member.journal.bytes().len();
    assert_eq!(
        member.feed(RaftInput::Propose(b"two".to_vec())),
        Ok(Fed::Proposed(Ok(()))),
        "fixture: the leader takes the proposal"
    );
    member.syncing = Some(before);
    member.complete();
}

#[test]
fn inputs_wait_while_a_sync_is_outstanding() {
    let (mut member, _) = lone(&config("one-voter"));
    member.tick_until_syncing();
    let status = member.lp.status().clone();
    let before = member.effects.len();
    for input in [
        RaftInput::Tick,
        RaftInput::Peer(vec![1, 2, 3]),
        RaftInput::Propose(b"later".to_vec()),
    ] {
        assert_eq!(
            member.feed(input.clone()),
            Ok(Fed::Deferred(input)),
            "rule: the library takes no input until SyncDone; the input comes back for the \
             controller to queue (§7.4 step 3)"
        );
    }
    member.poll().unwrap();
    assert_eq!(
        member.effects.len(),
        before,
        "rule: a deferred input, or a poll, has no effect while the sync is outstanding"
    );
    assert_eq!(
        member.lp.status(),
        &status,
        "rule: status lookups are served without touching the library (§7.4 step 1)"
    );
    member.complete();
    assert!(
        member.lp.outstanding_sync().is_some() || member.lp.status().role == Role::Leader,
        "rule: after SyncDone the loop resumes where it waited"
    );
}

#[test]
fn a_sync_done_from_another_incarnation_is_ignored() {
    let (mut member, _) = lone(&config("one-voter"));
    member.tick_until_syncing();
    let seq = member.lp.outstanding_sync().unwrap();
    let before = member.effects.len();
    assert_eq!(
        member.sync_done(seq, Incarnation::new(3)),
        Ok(Synced::Stale),
        "rule: a completion from a previous incarnation is ignored (§7.2, B-17)"
    );
    assert_eq!(
        (member.effects.len(), member.lp.outstanding_sync()),
        (before, Some(seq)),
        "rule: an ignored completion releases nothing, and the sync is still outstanding"
    );
    assert_eq!(
        member.sync_done(seq, INCARNATION),
        Ok(Synced::Completed),
        "rule: this incarnation's completion completes it"
    );
}

#[test]
#[should_panic(
    expected = "invariant RA-SYNC-UNEXPECTED violated: node 1: SyncDone for sync 10 in \
                incarnation 4, but the outstanding sync is None"
)]
fn a_sync_done_nobody_asked_for_breaks_its_invariant() {
    let (mut member, _) = lone(&config("one-voter"));
    let _ = member.sync_done(FIRST_SYNC, INCARNATION);
}

/// `small-limits` with only its first node: one voter, one committed byte per Ready.
pub fn one_voter_small_limits() -> Config {
    let (_, text) = crate::test_support::CONFIGS
        .iter()
        .find(|(name, _)| *name == "small-limits")
        .unwrap();
    let mut blocks = text.split("[[nodes]]");
    let head = blocks.next().unwrap();
    let first = blocks.next().unwrap();
    Config::parse(
        &format!("{head}[[nodes]]{first}"),
        Path::new("config/fixtures"),
    )
    .unwrap()
}

#[test]
fn a_commit_after_a_restart_drains_past_the_per_ready_bound_within_one_sync() {
    let config = one_voter_small_limits();
    let (mut first, id) = lone(&config);
    assert_eq!(
        config.consensus(id).unwrap().max_committed_size_per_ready(),
        ByteCount::new(1),
        "fixture: one committed byte per Ready, so every entry needs its own Ready"
    );
    first.elect();
    for n in 0..5 {
        first.commit(format!("c{n}").as_bytes());
    }
    assert_eq!(
        first.lp.status().commit.get(),
        6,
        "fixture: the no-op and five commands committed"
    );
    let recovered = first.lp.node().storage().log().clone();
    let persisted = recovered.hard_state().commit;
    let mut restarted = member_over(&config, id, 8, recovered, first.journal.clone());
    restarted.poll().unwrap();
    assert_eq!(
        (restarted.lp.status().commit, restarted.lp.applied()),
        (persisted, persisted),
        "rule: recovery restarts from the persisted commit index; a commit learned from a \
         LightReady is volatile and never persisted on its own (§7.4, DUR-2)"
    );
    let mut applied_per_sync = Vec::new();
    for _ in 0..1_000 {
        if restarted.lp.outstanding_sync().is_some() {
            let before = restarted.recorder.applied.len();
            restarted.complete();
            let after = restarted.recorder.applied.len();
            if after > before {
                applied_per_sync.push(after - before);
            }
        } else if restarted.lp.status().role == Role::Leader {
            break;
        } else {
            restarted.feed(RaftInput::Tick).unwrap();
        }
    }
    let last = restarted.lp.status().commit.get();
    assert_eq!(last, 7, "fixture: the new term's no-op commits everything");
    assert_eq!(
        applied_per_sync,
        vec![usize::try_from(last - persisted.get()).unwrap()],
        "rule: one SyncDone applies every newly committed entry, draining one Ready per entry \
         before the next input (§7.4 step 6)"
    );
    let applied: Vec<u64> = restarted
        .recorder
        .applied
        .iter()
        .map(|(entry, _)| entry.index.get())
        .collect();
    assert_eq!(
        applied,
        (persisted.get() + 1..=last).collect::<Vec<_>>(),
        "rule: entries apply once each, in index order"
    );
}

#[test]
fn a_hard_stop_stops_the_loop() {
    let config = config("one-voter");
    let id = config.voters().next().unwrap();
    let voters: Vec<NodeId> = config.voters().collect();
    let tiny = LogicalLog::after_genesis(
        0,
        &voters,
        LogBounds::new(ByteCount::new(16), ByteCount::new(1)),
    );
    let mut member = member_over(&config, id, 7, tiny, Journal::default());
    let stopped = (0..1_000)
        .find_map(|_| member.feed(RaftInput::Tick).err())
        .unwrap();
    let LoopError::HardStop { limit, .. } = stopped else {
        panic!(
            "rule: the first frame past max_journal_bytes is a hard stop (DM-7); got {stopped:?}"
        );
    };
    assert_eq!(limit, 1, "rule: the error names the limit");
    assert!(
        member.effects.is_empty() && member.journal.is_empty(),
        "rule: a refused frame is never appended or synced"
    );
    assert_eq!(
        (
            member.feed(RaftInput::Tick),
            member.poll(),
            member.sync_done(FIRST_SYNC, INCARNATION),
        ),
        (
            Err(LoopError::Stopped),
            Err(LoopError::Stopped),
            Err(LoopError::Stopped)
        ),
        "rule: a stopped loop takes no more work; the node is failed (DM-7)"
    );
}

#[test]
fn a_failed_journal_read_stops_the_loop_with_its_cause() {
    let (mut member, _) = lone(&config("one-voter"));
    member.elect();
    // Entries larger than the 16-byte entry cache are read back from the journal.
    member.commit(&[b'a'; 100]);
    assert_eq!(
        member.lp.applied().get(),
        2,
        "fixture: the no-op and one command applied"
    );
    member.journal.fail_reads();
    assert_eq!(
        member.feed(RaftInput::Propose(vec![b'b'; 100])),
        Ok(Fed::Proposed(Ok(()))),
        "fixture: the proposal is taken and its frame written"
    );
    let seq = member.lp.outstanding_sync().unwrap();
    let before = member.effects.len();
    let failed = member.sync_done(seq, INCARNATION);
    let Err(LoopError::StorageFailed(failure)) = failed else {
        panic!(
            "rule: a failed journal read under the library stops the loop with its cause; got \
             {failed:?}"
        );
    };
    assert_eq!(
        (failure.index.get(), failure.error),
        (3, JournalError::Storage(types::StorageError::Failed)),
        "rule: the error names the entry and the cause (§7.11)"
    );
    assert_eq!(
        (member.effects.len(), member.lp.applied().get()),
        (before, 2),
        "rule: nothing more is persisted, sent or applied after the failure"
    );
    assert_eq!(
        (member.feed(RaftInput::Tick), member.poll()),
        (Err(LoopError::Stopped), Err(LoopError::Stopped)),
        "rule: the loop takes no more work; the node is failed (§7.11)"
    );
}

#[test]
fn a_refused_peer_message_comes_back_with_its_reason_and_the_port_folds_it() {
    let config = config("three-voters");
    let ids: Vec<NodeId> = config.voters().collect();
    let (me, peer) = (ids[0], ids[1]);
    let mut member = member(&config, me, 7);
    let last = member.lp.status().last.get();
    let mut heartbeat = eraftpb::Message::default();
    heartbeat.set_msg_type(MessageType::MsgHeartbeat);
    heartbeat.from = peer.get();
    heartbeat.to = me.get();
    heartbeat.term = 2;
    heartbeat.commit = last + 5;
    let mut from_a_stranger = heartbeat.clone();
    from_a_stranger.from = 99;
    from_a_stranger.commit = 0;
    let cases = [
        (
            "bytes that are not a message",
            vec![0xff, 0xff, 0xff],
            Inadmissible::Undecodable,
        ),
        (
            "a heartbeat from outside the voters",
            from_a_stranger.write_to_bytes().unwrap(),
            Inadmissible::Sender { from: 99 },
        ),
        (
            "a heartbeat committing past this node's log",
            heartbeat.write_to_bytes().unwrap(),
            Inadmissible::CommitPastLog {
                commit: last + 5,
                last,
            },
        ),
    ];
    for (case, bytes, reason) in cases {
        let status = member.lp.status().clone();
        let before = member.effects.len();
        assert_eq!(
            member.feed(RaftInput::Peer(bytes.clone())),
            Ok(Fed::Stepped(Err(StepRefusal::Inadmissible(reason)))),
            "rule: the loop hands back the refusal with its reason, for the controller's \
             counters (§8.2, br-i0tk); case: {case}"
        );
        assert_eq!(
            (member.lp.status(), member.effects.len()),
            (&status, before),
            "rule: a refused message changes nothing and sends nothing; case: {case}"
        );
        let mut node = RaftNode::new(
            &config.consensus(me).unwrap(),
            config.voters(),
            empty_log(),
            Journal::default(),
            Seeded::new(7),
            LogIndex::ZERO,
        )
        .unwrap();
        assert_eq!(
            Consensus::step(&mut node, bytes),
            Err(StepError::Rejected),
            "rule: the port answers every refusal as Rejected; the reason stays in the \
             adapter (br-i0tk); case: {case}"
        );
    }
}
