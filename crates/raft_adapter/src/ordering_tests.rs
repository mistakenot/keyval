//! W13.4's adversarial ordering tests (PLAN.md §7.3, §7.4, DUR-2, D-28).
//!
//! - [`pins`] pins the exact raft-rs 0.7.0 semantics the Ready loop relies on: which
//!   message list a Ready fills, when committed entries are handed over, what `advance`,
//!   `advance_append` and `advance_apply_to` each do, and the per-Ready committed bound
//!   that leaves work for an immediate drain. A library upgrade that changes any of them
//!   fails here first.
//! - [`adversarial`] drives three Ready loops through seeded hostile schedules:
//!   - syncs complete late and out of order;
//!   - completions from crashed incarnations arrive afterwards;
//!   - the network reorders, duplicates and drops messages;
//!   - inputs arrive while a sync is outstanding;
//!   - nodes crash and lose whatever they had not synced, then restart through
//!     `journal::recover`.
//!
//!   Every run is judged on DUR-2 for every message, one leader per term, one applied
//!   history across nodes and incarnations, and no ready work left behind an idle loop;
//!   after the faults heal, every node applies everything ever committed.
#![cfg(test)]

/// One `SplitMix64` stream: these tests' only randomness, seeded (§7.14).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A draw from `0..bound`.
    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(bound).unwrap()).unwrap()
    }
}

mod pins {
    // check: RAFT-PINS

    use raft::eraftpb::{Message, MessageType};
    use raft::storage::MemStorage;
    use raft::{Config, RawNode, StateRole};

    use crate::entropy::EntropySource;
    use crate::test_support::Seeded;

    /// Node `id` among `voters`, over the library's own in-memory storage, handing over at
    /// most `max_committed` bytes of committed entries per Ready.
    fn raw(id: u64, voters: &[u64], max_committed: u64) -> RawNode<MemStorage> {
        let config = Config {
            id,
            election_tick: 10,
            heartbeat_tick: 3,
            max_committed_size_per_ready: max_committed,
            ..Config::default()
        };
        config.validate().unwrap();
        let storage = MemStorage::new_with_conf_state((voters.to_vec(), Vec::<u64>::new()));
        let logger = slog::Logger::root(slog::Discard, slog::o!());
        RawNode::new(
            &config,
            storage,
            EntropySource::new(Seeded::new(id)),
            &logger,
        )
        .unwrap()
    }

    /// Writes a Ready's entries and hard state to the node's storage: what the loop's frame
    /// and sync make durable before it advances.
    fn persist(node: &RawNode<MemStorage>, ready: &raft::Ready) {
        let mut store = node.store().wl();
        store.append(ready.entries()).unwrap();
        if let Some(state) = ready.hs() {
            store.set_hardstate(state.clone());
        }
    }

    fn kinds(messages: &[Message]) -> Vec<MessageType> {
        messages.iter().map(Message::get_msg_type).collect()
    }

    /// A lone voter that has elected itself and committed its no-op, with nothing ready.
    fn lone_leader(max_committed: u64) -> RawNode<MemStorage> {
        let mut node = raw(1, &[1], max_committed);
        node.campaign().unwrap();
        while node.has_ready() {
            let ready = node.ready();
            persist(&node, &ready);
            let _light = node.advance_append(ready);
        }
        assert_eq!(
            (node.raft.state, node.raft.raft_log.committed),
            (StateRole::Leader, 1),
            "fixture: a lone voter leads and commits its no-op"
        );
        node
    }

    /// Nodes 1, 2 and 3, with node 1 elected and its first Ready (the no-op and its appends)
    /// still to take.
    fn elected() -> Vec<RawNode<MemStorage>> {
        let mut nodes: Vec<RawNode<MemStorage>> =
            (1..=3).map(|id| raw(id, &[1, 2, 3], u64::MAX)).collect();
        nodes[0].campaign().unwrap();
        let mut ready = nodes[0].ready();
        persist(&nodes[0], &ready);
        let requests = ready.take_persisted_messages();
        let _light = nodes[0].advance_append(ready);
        for request in requests {
            let voter = usize::try_from(request.to - 1).unwrap();
            nodes[voter].step(request).unwrap();
            let mut ready = nodes[voter].ready();
            persist(&nodes[voter], &ready);
            let grants = ready.take_persisted_messages();
            let _light = nodes[voter].advance_append(ready);
            for grant in grants {
                nodes[0].step(grant).unwrap();
            }
        }
        assert_eq!(
            nodes[0].raft.state,
            StateRole::Leader,
            "fixture: node 1 wins the election"
        );
        nodes
    }

    #[test]
    fn a_candidates_vote_requests_are_persisted_messages() {
        let mut node = raw(1, &[1, 2, 3], u64::MAX);
        node.campaign().unwrap();
        let ready = node.ready();
        assert_eq!(
            (
                kinds(ready.messages()),
                kinds(ready.persisted_messages()),
                ready.must_sync()
            ),
            (
                Vec::new(),
                vec![MessageType::MsgRequestVote, MessageType::MsgRequestVote],
                true
            ),
            "pin (raft-rs 0.7.0): a non-leader's messages all wait for persistence \
             (`is_persisted_msg`), and a new term and vote must sync"
        );
    }

    #[test]
    fn a_leaders_appends_are_ordinary_messages_and_a_followers_answer_is_persisted() {
        let mut nodes = elected();
        let mut ready = nodes[0].ready();
        assert_eq!(
            (
                kinds(ready.messages()),
                kinds(ready.persisted_messages()),
                ready.entries().len()
            ),
            (
                vec![MessageType::MsgAppend, MessageType::MsgAppend],
                Vec::new(),
                1
            ),
            "pin (raft-rs 0.7.0): a leader's messages may leave before its own entries persist \
             (raft thesis 10.2.1); the adapter holds them until the sync anyway (§7.4 step 4)"
        );
        persist(&nodes[0], &ready);
        let appends = ready.take_messages();
        let _light = nodes[0].advance_append(ready);
        nodes[1].step(appends[0].clone()).unwrap();
        let mut ready = nodes[1].ready();
        assert_eq!(
            (
                kinds(ready.messages()),
                kinds(ready.persisted_messages()),
                ready.must_sync()
            ),
            (Vec::new(), vec![MessageType::MsgAppendResponse], true),
            "pin (raft-rs 0.7.0): a follower's acknowledgement waits for its entries to persist"
        );
        persist(&nodes[1], &ready);
        let _answer = ready.take_persisted_messages();
        let light = nodes[1].advance_append(ready);
        assert!(
            light.messages().is_empty(),
            "pin (raft-rs 0.7.0): a non-leader never has messages after advance_append (the \
             library would fatal!); light={light:?}"
        );
    }

    #[test]
    fn committed_entries_are_handed_over_once_and_only_once_persisted() {
        let mut node = lone_leader(u64::MAX);
        node.propose(Vec::new(), b"a".to_vec()).unwrap();
        let mut ready = node.ready();
        assert_eq!(
            (ready.entries().len(), ready.committed_entries().len()),
            (1, 0),
            "pin (raft-rs 0.7.0): an entry is not handed over as committed in the Ready that \
             first persists it: committed entries stop at min(committed, persisted)"
        );
        persist(&node, &ready);
        let _ = ready.take_committed_entries();
        let mut light = node.advance_append(ready);
        let handed: Vec<u64> = light
            .take_committed_entries()
            .iter()
            .map(raft::eraftpb::Entry::get_index)
            .collect();
        assert_eq!(
            (light.commit_index(), handed),
            (Some(2), vec![2]),
            "pin (raft-rs 0.7.0): once persisted, the lone leader commits it, and the \
             LightReady hands it over with the new commit index"
        );
        assert!(
            !node.has_ready(),
            "pin (raft-rs 0.7.0): a handed-over entry is never handed over again, even though \
             nothing was reported applied"
        );
        assert_eq!(
            node.raft.raft_log.applied, 0,
            "pin (raft-rs 0.7.0): advance_append applies nothing; the adapter reports applied \
             progress itself (§7.4 step 7)"
        );
        node.advance_apply_to(2);
        assert_eq!(
            (node.raft.raft_log.applied, node.has_ready()),
            (2, false),
            "pin (raft-rs 0.7.0): advance_apply_to moves only the applied index"
        );
    }

    #[test]
    fn advance_also_marks_applied_what_earlier_readies_handed_over() {
        // Two identical lone leaders: one advanced with `advance`, one with `advance_append`.
        let mut plain = lone_leader(u64::MAX);
        let mut append_only = lone_leader(u64::MAX);
        for node in [&mut plain, &mut append_only] {
            node.propose(Vec::new(), b"a".to_vec()).unwrap();
        }
        let ready = plain.ready();
        persist(&plain, &ready);
        let light = plain.advance(ready);
        assert_eq!(
            (light.committed_entries().len(), plain.raft.raft_log.applied),
            (1, 1),
            "pin (raft-rs 0.7.0): advance marks applied everything handed over before it \
             (the no-op), but not the LightReady's own entry (2)"
        );
        let ready = append_only.ready();
        persist(&append_only, &ready);
        let light = append_only.advance_append(ready);
        assert_eq!(
            (
                light.committed_entries().len(),
                append_only.raft.raft_log.applied
            ),
            (1, 0),
            "pin (raft-rs 0.7.0): advance_append marks nothing applied, so the adapter's \
             applied index never runs ahead of what it applied (§7.4 step 7)"
        );
    }

    #[test]
    fn the_per_ready_committed_bound_leaves_work_for_an_immediate_drain() {
        let mut node = lone_leader(1);
        for command in [b"a", b"b", b"c"] {
            node.propose(Vec::new(), command.to_vec()).unwrap();
        }
        let ready = node.ready();
        assert_eq!(
            ready.entries().len(),
            3,
            "fixture: one Ready persists all three"
        );
        persist(&node, &ready);
        let light = node.advance_append(ready);
        let mut handed = vec![light.committed_entries().len()];
        assert!(
            node.has_ready(),
            "pin (raft-rs 0.7.0): with committed entries left over the bound, the library has \
             a Ready at once; the loop must drain it before its next input (§7.4 step 6)"
        );
        while node.has_ready() {
            let ready = node.ready();
            assert!(
                !ready.must_sync() && ready.entries().is_empty(),
                "pin (raft-rs 0.7.0): a drain Ready only hands over committed entries"
            );
            handed.push(ready.committed_entries().len());
            let light = node.advance_append(ready);
            handed.push(light.committed_entries().len());
        }
        handed.retain(|&count| count > 0);
        assert_eq!(
            handed,
            vec![1, 1, 1],
            "pin (raft-rs 0.7.0): the bound keeps at least one entry per handover, so a \
             one-byte bound hands them over one at a time, in Readies and in the LightReadies \
             advance_append returns; the loop applies both (§7.4 steps 5 and 6)"
        );
    }
}

mod adversarial {
    // check: RAFT-ORDERING

    use std::collections::BTreeMap;

    use protobuf::Message as _;
    use raft::eraftpb::{self, MessageType};
    use types::{
        ByteCount, Config, Effect, Entry, EntryPayload, Incarnation, LogIndex, NodeId, Role,
        SyncSeq,
    };

    use super::Rng;
    use crate::admit::Inadmissible;
    use crate::node::{RaftNode, StepRefusal};
    use crate::ready_loop::{Apply, Fed, RaftInput, ReadyLoop, Synced};
    use crate::ready_loop_tests::{dur2_violation, durable_state};
    use crate::test_support::{BOUNDS, Journal, Seeded, config, journal_with_genesis};

    /// The first sync number of every incarnation, so a crashed incarnation's completion
    /// can carry the very number its successor waits for.
    const FIRST_SYNC: SyncSeq = SyncSeq::new(10);

    /// The entries one incarnation applied, in order.
    #[derive(Debug, Default)]
    struct Applied(Vec<Entry>);

    impl Apply for Applied {
        fn apply(&mut self, entry: &Entry, _commit: LogIndex, _effects: &mut Vec<Effect>) {
            self.0.push(entry.clone());
        }
    }

    /// One incarnation of one node, with storage that syncs only when the schedule says so.
    struct Node {
        lp: ReadyLoop<Journal>,
        journal: Journal,
        incarnation: Incarnation,
        /// The journal's genesis frame length: batch frames start here.
        genesis_len: usize,
        /// The journal bytes the last completed sync covers: what any crash keeps.
        synced: usize,
        /// The outstanding sync and the journal length it covers.
        pending: Option<(SyncSeq, usize)>,
        applied: Applied,
        /// How many of `applied` were checked against the one history.
        checked: usize,
    }

    /// Starts incarnation `incarnation` of `id` over the journal `bytes`, as startup does
    /// (§7.7): recover, discard a torn tail, rebuild the library's view from the recovered
    /// log, and replay committed entries from index 1 (v1 keeps no snapshot).
    fn start(config: &Config, id: NodeId, bytes: &[u8], incarnation: u64, seed: u64) -> Node {
        let scratch = Journal::default();
        scratch.push(bytes);
        let len = ByteCount::new(u64::try_from(bytes.len()).unwrap());
        let (_, log) = journal::recover(&scratch, len, config, id, BOUNDS)
            .unwrap_or_else(|error| {
                panic!(
                    "rule: a crashed node's journal always recovers (DUR-6); node={id} \
                     incarnation={incarnation}: {error:?}"
                )
            })
            .into_parts();
        let (fresh, _) = journal_with_genesis(config, id);
        let genesis_len = fresh.bytes().len();
        let end = usize::try_from(log.end()).unwrap();
        let journal = Journal::default();
        journal.push(&bytes[..end]);
        let node = RaftNode::new(
            &config.consensus(id).unwrap(),
            config.voters(),
            log,
            journal.clone(),
            Seeded::new(seed ^ incarnation.wrapping_mul(0x9e37) ^ id.get()),
            LogIndex::ZERO,
        )
        .unwrap();
        Node {
            lp: ReadyLoop::new(node, Incarnation::new(incarnation), FIRST_SYNC),
            journal,
            incarnation: Incarnation::new(incarnation),
            genesis_len,
            synced: end,
            pending: None,
            applied: Applied::default(),
            checked: 0,
        }
    }

    /// How often each fault happened, so a run is never vacuous.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    struct Faults {
        crashes: u32,
        torn: u32,
        stale: u32,
        duplicated: u32,
        dropped: u32,
        deferred: u32,
        flooded: u32,
    }

    impl Faults {
        fn absorb(&mut self, other: Self) {
            self.crashes += other.crashes;
            self.torn += other.torn;
            self.stale += other.stale;
            self.duplicated += other.duplicated;
            self.dropped += other.dropped;
            self.deferred += other.deferred;
            self.flooded += other.flooded;
        }
    }

    /// A cluster under a hostile schedule.
    struct Run {
        config: Config,
        seed: u64,
        nodes: BTreeMap<NodeId, Node>,
        /// Messages in flight: sender, destination, bytes.
        network: Vec<(NodeId, NodeId, Vec<u8>)>,
        /// Completions owed to crashed incarnations, still to arrive.
        stale: Vec<(NodeId, SyncSeq, Incarnation)>,
        /// The one applied history every node and incarnation must follow.
        history: Vec<Entry>,
        /// The leader seen in each term.
        leaders: BTreeMap<u64, NodeId>,
        violations: Vec<String>,
        faults: Faults,
        next_incarnation: u64,
        proposals: u32,
    }

    impl Run {
        fn new(config: Config, seed: u64) -> Self {
            let nodes = config
                .voters()
                .map(|id| {
                    let (journal, _) = journal_with_genesis(&config, id);
                    (id, start(&config, id, &journal.bytes(), 1, seed))
                })
                .collect();
            Self {
                config,
                seed,
                nodes,
                network: Vec::new(),
                stale: Vec::new(),
                history: Vec::new(),
                leaders: BTreeMap::new(),
                violations: Vec::new(),
                faults: Faults::default(),
                next_incarnation: 1,
                proposals: 0,
            }
        }

        fn ids(&self) -> Vec<NodeId> {
            self.nodes.keys().copied().collect()
        }

        fn violation(&mut self, what: &str) {
            self.violations.push(format!("seed {}: {what}", self.seed));
        }

        /// Executes `id`'s effects as the composition root would, judging each send against
        /// what its sender has synced (DUR-2).
        fn execute(&mut self, id: NodeId, effects: Vec<Effect>) {
            for effect in effects {
                let node = self.nodes.get_mut(&id).unwrap();
                let problem = match effect {
                    Effect::Append { frame } => {
                        node.journal.push(&frame);
                        None
                    }
                    Effect::Sync { seq, incarnation } => {
                        let earlier = node.pending.replace((seq, node.journal.bytes().len()));
                        (incarnation != node.incarnation || earlier.is_some()).then(|| {
                            format!("node {id} asked for sync {seq:?} of {incarnation:?} with {earlier:?} outstanding")
                        })
                    }
                    Effect::Send { to, bytes } => {
                        let journal = node.journal.bytes();
                        let durable = durable_state(&journal[node.genesis_len..node.synced]);
                        let problem = node
                            .pending
                            .map(|(seq, _)| {
                                format!("node {id} sent to {to} while sync {seq:?} was outstanding")
                            })
                            .or_else(|| dur2_violation(id, to, &bytes, durable));
                        self.network.push((id, to, bytes));
                        problem
                    }
                    Effect::Truncate { .. }
                    | Effect::CreateJournal
                    | Effect::SyncDir { .. }
                    | Effect::Respond { .. }
                    | Effect::Log(_)
                    | Effect::Observe(_) => Some(format!("node {id} emitted {effect:?}")),
                };
                if let Some(problem) = problem {
                    self.violation(&problem);
                }
            }
        }

        /// The checks after every action on `id`.
        fn after(&mut self, id: NodeId) {
            self.check_history(id);
            self.check_leader(id);
            self.check_idle(id);
        }

        /// Every entry `id` applied follows the one history (state-machine safety, C-2).
        fn check_history(&mut self, id: NodeId) {
            let node = self.nodes.get_mut(&id).unwrap();
            let fresh: Vec<Entry> = node.applied.0[node.checked..].to_vec();
            node.checked = node.applied.0.len();
            for entry in fresh {
                let index = usize::try_from(entry.index.get()).unwrap();
                let problem = match self.history.get(index - 1) {
                    Some(known) if *known == entry => None,
                    Some(known) => Some(format!(
                        "node {id} applied {entry:?} where the history holds {known:?}"
                    )),
                    None if index == self.history.len() + 1 => {
                        self.history.push(entry);
                        None
                    }
                    None => Some(format!(
                        "node {id} applied {entry:?} past the history's end {}",
                        self.history.len()
                    )),
                };
                if let Some(problem) = problem {
                    self.violation(&problem);
                }
            }
        }

        /// At most one leader per term (election safety).
        fn check_leader(&mut self, id: NodeId) {
            let status = self.nodes[&id].lp.status();
            if status.role == Role::Leader {
                let term = status.term.get();
                let first = *self.leaders.entry(term).or_insert(id);
                if first != id {
                    self.violation(&format!("nodes {first} and {id} both led term {term}"));
                }
            }
        }

        /// An idle loop has drained all ready work: a poll finds nothing (§7.4 step 6).
        fn check_idle(&mut self, id: NodeId) {
            let node = self.nodes.get_mut(&id).unwrap();
            if node.lp.outstanding_sync().is_some() {
                return;
            }
            let applied = node.applied.0.len();
            let mut effects = Vec::new();
            node.lp.poll(&mut node.applied, &mut effects).unwrap();
            if !effects.is_empty() || node.applied.0.len() != applied {
                let left = effects.len();
                self.violation(&format!(
                    "node {id} went idle with ready work left: a poll emitted {left} effects"
                ));
                self.execute(id, effects);
                self.check_history(id);
            }
        }

        /// Feeds `input` to `id` and executes what it did.
        fn feed(&mut self, id: NodeId, input: RaftInput) -> Fed {
            let node = self.nodes.get_mut(&id).unwrap();
            let mut effects = Vec::new();
            let fed = node
                .lp
                .feed(input, &mut node.applied, &mut effects)
                .unwrap();
            self.execute(id, effects);
            self.after(id);
            fed
        }

        /// Completes `id`'s outstanding sync, if it has one.
        fn complete(&mut self, id: NodeId) {
            let node = self.nodes.get_mut(&id).unwrap();
            let Some((seq, covered)) = node.pending.take() else {
                return;
            };
            node.synced = covered;
            let incarnation = node.incarnation;
            let mut effects = Vec::new();
            let done = node
                .lp
                .sync_done(seq, incarnation, &mut node.applied, &mut effects)
                .unwrap();
            if done != Synced::Completed {
                self.violation(&format!("node {id}'s own sync {seq:?} came back {done:?}"));
            }
            self.execute(id, effects);
            self.after(id);
        }

        /// Delivers the message in flight at `at`. A node waiting for its sync defers it, and
        /// it goes back in flight, as a controller would queue it (§7.11).
        fn deliver(&mut self, at: usize) {
            let (from, to, bytes) = self.network.remove(at);
            match self.feed(to, RaftInput::Peer(bytes)) {
                Fed::Deferred(RaftInput::Peer(bytes)) => {
                    self.faults.deferred += 1;
                    self.network.push((from, to, bytes));
                }
                Fed::Stepped(Err(error)) => self.violation(&format!(
                    "node {to} refused a message node {from} sent: {error:?}"
                )),
                Fed::Stepped(Ok(())) | Fed::Ticked | Fed::Proposed(_) | Fed::Deferred(_) => {}
            }
        }

        /// Delivers a completion owed to a crashed incarnation: it must change nothing.
        fn deliver_stale(&mut self, at: usize) {
            let (id, seq, incarnation) = self.stale.remove(at);
            let node = self.nodes.get_mut(&id).unwrap();
            let before = node.lp.outstanding_sync();
            let mut effects = Vec::new();
            let done = node
                .lp
                .sync_done(seq, incarnation, &mut node.applied, &mut effects)
                .unwrap();
            let after = node.lp.outstanding_sync();
            self.faults.stale += 1;
            if (done, effects.len(), after) != (Synced::Stale, 0, before) {
                self.violation(&format!(
                    "node {id}: sync {seq:?} of crashed {incarnation:?} gave {done:?}, {} effects, \
                     outstanding {before:?} -> {after:?}",
                    effects.len()
                ));
            }
        }

        /// Offers inputs to a node waiting for its sync: each comes back unchanged, and
        /// nothing happens (§7.4 step 3).
        fn flood(&mut self, id: NodeId, inputs: Vec<RaftInput>) {
            let node = self.nodes.get_mut(&id).unwrap();
            if node.lp.outstanding_sync().is_none() {
                return;
            }
            self.faults.flooded += 1;
            for input in inputs {
                let mut effects = Vec::new();
                let fed = node
                    .lp
                    .feed(input.clone(), &mut node.applied, &mut effects)
                    .unwrap();
                if fed != Fed::Deferred(input) || !effects.is_empty() {
                    self.violations.push(format!(
                        "seed {}: node {id} took an input during its sync: {fed:?}, {} effects",
                        self.seed,
                        effects.len()
                    ));
                    return;
                }
            }
        }

        /// Proposes a fresh command at whichever node believes it leads.
        fn propose(&mut self) {
            let leader = self
                .ids()
                .into_iter()
                .find(|id| self.nodes[id].lp.status().role == Role::Leader);
            if let Some(leader) = leader {
                self.proposals += 1;
                let command = format!("cmd-{}-{}", self.seed, self.proposals).into_bytes();
                let _fed = self.feed(leader, RaftInput::Propose(command));
            }
        }

        /// Crashes `id`: its journal keeps the synced bytes and `extra` of the rest (a torn
        /// tail when that cuts a frame), its outstanding completion is still owed, and it
        /// restarts as a new incarnation that replays the committed log.
        fn crash(&mut self, id: NodeId, extra: usize) {
            let node = self.nodes.remove(&id).unwrap();
            let bytes = node.journal.bytes();
            let keep = (node.synced + extra).min(bytes.len());
            self.faults.crashes += 1;
            if keep < bytes.len() {
                self.faults.torn += 1;
            }
            if let Some((seq, _)) = node.pending {
                self.stale.push((id, seq, node.incarnation));
            }
            self.next_incarnation += 1;
            let restarted = start(
                &self.config,
                id,
                &bytes[..keep],
                self.next_incarnation,
                self.seed,
            );
            self.nodes.insert(id, restarted);
            let node = self.nodes.get_mut(&id).unwrap();
            let mut effects = Vec::new();
            node.lp.poll(&mut node.applied, &mut effects).unwrap();
            self.execute(id, effects);
            self.after(id);
        }

        /// One hostile action, chosen by `rng`.
        fn act(&mut self, rng: &mut Rng) {
            let ids = self.ids();
            let id = ids[rng.below(ids.len())];
            let in_flight = self.network.len();
            match rng.below(100) {
                0..=24 => {
                    let _fed = self.feed(id, RaftInput::Tick);
                }
                25..=49 if in_flight > 0 => self.deliver(rng.below(in_flight)),
                50..=54 if in_flight > 0 => {
                    let copy = self.network[rng.below(in_flight)].clone();
                    self.network.push(copy);
                    self.faults.duplicated += 1;
                }
                55..=59 if in_flight > 0 => {
                    self.network.remove(rng.below(in_flight));
                    self.faults.dropped += 1;
                }
                60..=79 => self.complete(id),
                80..=83 if !self.stale.is_empty() => {
                    self.deliver_stale(rng.below(self.stale.len()));
                }
                84..=87 => {
                    let inputs = vec![
                        RaftInput::Tick,
                        RaftInput::Propose(b"while syncing".to_vec()),
                        RaftInput::Peer(vec![0xff, 0x01]),
                    ];
                    self.flood(id, inputs);
                }
                88..=97 => self.propose(),
                98..=99 => self.crash(id, rng.below(4_096)),
                _ => {}
            }
        }

        /// Heals every fault: completes every sync, delivers every message in order and ticks
        /// every node, until one leader's log is committed and applied everywhere. False if it
        /// never converges.
        fn heal(&mut self) -> bool {
            for round in 0..4_000 {
                for id in self.ids() {
                    self.complete(id);
                }
                let in_flight = self.network.len();
                for _ in 0..in_flight {
                    self.deliver(0);
                }
                if round % 3 == 0 {
                    for id in self.ids() {
                        let _fed = self.feed(id, RaftInput::Tick);
                    }
                }
                if self.converged() {
                    return true;
                }
            }
            false
        }

        /// One leader, whose whole log is committed and applied by every node, and nothing
        /// in flight.
        fn converged(&self) -> bool {
            let quiet = self.network.is_empty() && self.nodes.values().all(|n| n.pending.is_none());
            let leader = self
                .nodes
                .values()
                .map(|node| node.lp.status())
                .find(|status| status.role == Role::Leader);
            quiet
                && leader.is_some_and(|leader| {
                    leader.commit == leader.last
                        && self
                            .nodes
                            .values()
                            .all(|node| node.lp.applied() == leader.commit)
                })
        }
    }

    /// A hostile run of `steps` actions over the three-voter `small-limits` fixture (one
    /// committed entry per Ready, so commits drain), then healed.
    fn hostile(seed: u64, steps: usize) -> Run {
        let mut run = Run::new(config("small-limits"), seed);
        let mut rng = Rng(seed ^ 0x00ad_5eed);
        for _ in 0..steps {
            run.act(&mut rng);
        }
        if !run.heal() {
            run.violation("the healed cluster never converged on one applied log");
        }
        run
    }

    #[test]
    fn hostile_schedules_never_release_early_and_keep_one_history() {
        let mut faults = Faults::default();
        let mut commands = 0;
        for seed in 0..10 {
            let run = hostile(seed, 900);
            assert_eq!(
                run.violations,
                Vec::<String>::new(),
                "rule: under late, reordered and stale syncs, a lossy network and crashes, no \
                 message leaves on unsynced state (DUR-2), one node leads each term, every node \
                 and incarnation applies one history, and an idle loop has no ready work left \
                 (§7.4); seed={seed}"
            );
            let applied = run.history.len();
            for (id, node) in &run.nodes {
                assert_eq!(
                    node.lp.applied().get(),
                    u64::try_from(applied).unwrap(),
                    "rule: after the faults heal, every node applies everything ever committed \
                     (C-2); seed={seed} node={id}"
                );
            }
            commands += run
                .history
                .iter()
                .filter(|entry| entry.payload != EntryPayload::Empty)
                .count();
            faults.absorb(run.faults);
        }
        let Faults {
            crashes,
            torn,
            stale,
            duplicated,
            dropped,
            deferred,
            flooded,
        } = faults;
        assert!(
            [crashes, torn, stale, duplicated, dropped, deferred, flooded]
                .iter()
                .all(|&count| count > 0)
                && commands > 0,
            "fixture: every fault happens and commands commit, so no rule is vacuous; \
             faults={faults:?} commands={commands}"
        );
    }

    #[test]
    fn the_same_seed_gives_the_same_hostile_run() {
        let journals = |run: &Run| -> Vec<Vec<u8>> {
            run.nodes
                .values()
                .map(|node| node.journal.bytes())
                .collect()
        };
        let (first, second) = (hostile(3, 400), hostile(3, 400));
        assert_eq!(
            (&first.history, journals(&first), first.faults),
            (&second.history, journals(&second), second.faults),
            "rule: same seed, same schedule, same journals and history (P4, T-3)"
        );
    }

    /// The lone voter of `one-voter`, ticked until it waits for its first sync.
    fn lone_syncing() -> (Run, NodeId) {
        let mut run = Run::new(config("one-voter"), 1);
        let id = run.ids()[0];
        for _ in 0..1_000 {
            if run.nodes[&id].pending.is_some() {
                return (run, id);
            }
            let _fed = run.feed(id, RaftInput::Tick);
        }
        panic!("fixture: a lone voter campaigns and asks for a sync");
    }

    #[test]
    fn a_crashed_incarnations_completion_never_completes_its_successor() {
        let (mut run, id) = lone_syncing();
        let (old, _) = run.nodes[&id].pending.unwrap();
        run.crash(id, 0);
        assert_eq!(
            run.stale,
            vec![(id, old, Incarnation::new(1))],
            "fixture: the crash leaves the completion owed"
        );
        while run.nodes[&id].pending.is_none() {
            let _fed = run.feed(id, RaftInput::Tick);
        }
        let (new, _) = run.nodes[&id].pending.unwrap();
        assert_eq!(
            new, old,
            "fixture: the new incarnation waits for the same sync number"
        );
        run.deliver_stale(0);
        assert_eq!(
            (run.violations.clone(), run.nodes[&id].lp.outstanding_sync()),
            (Vec::<String>::new(), Some(new)),
            "rule: a completion from a crashed incarnation is ignored, even with the very \
             sequence number its successor waits for (§7.2)"
        );
        run.complete(id);
        assert_eq!(
            run.violations,
            Vec::<String>::new(),
            "rule: the successor's own completion completes it"
        );
    }

    #[test]
    #[should_panic(
        expected = "invariant RA-SYNC-UNEXPECTED violated: node 1: SyncDone for sync 11"
    )]
    fn an_early_sync_done_breaks_its_invariant() {
        let (mut run, id) = lone_syncing();
        let node = run.nodes.get_mut(&id).unwrap();
        let (seq, _) = node.pending.unwrap();
        let early = seq.checked_next().unwrap();
        let _ = node
            .lp
            .sync_done(early, node.incarnation, &mut node.applied, &mut Vec::new());
    }

    #[test]
    #[should_panic(
        expected = "invariant RA-SYNC-UNEXPECTED violated: node 1: SyncDone for sync 10"
    )]
    fn a_repeated_sync_done_breaks_its_invariant() {
        let (mut run, id) = lone_syncing();
        let (seq, _) = run.nodes[&id].pending.unwrap();
        run.complete(id);
        let node = run.nodes.get_mut(&id).unwrap();
        let _ = node
            .lp
            .sync_done(seq, node.incarnation, &mut node.applied, &mut Vec::new());
    }

    #[test]
    fn a_snapshot_from_a_peer_never_reaches_the_loops_ready() {
        // RA-NO-SNAPSHOT fires if the library ever hands over a snapshot Ready (br-kky5's
        // node::invariant_fixtures feeds one below admission). Through the loop, the only way
        // in is a peer's MsgSnapshot, and admission refuses it first (br-xx08).
        let mut run = Run::new(config("small-limits"), 2);
        let ids = run.ids();
        let (me, peer) = (ids[0], ids[1]);
        let mut snapshot = eraftpb::Message::default();
        snapshot.set_msg_type(MessageType::MsgSnapshot);
        snapshot.from = peer.get();
        snapshot.to = me.get();
        snapshot.term = 5;
        snapshot.mut_snapshot().mut_metadata().index = 9;
        snapshot.mut_snapshot().mut_metadata().term = 5;
        let before = run.nodes[&me].lp.status().clone();
        let journal = run.nodes[&me].journal.bytes();
        let fed = run.feed(me, RaftInput::Peer(snapshot.write_to_bytes().unwrap()));
        assert_eq!(
            (
                fed,
                run.nodes[&me].lp.status(),
                run.nodes[&me].journal.bytes()
            ),
            (
                Fed::Stepped(Err(StepRefusal::Inadmissible(Inadmissible::MessageType {
                    code: protobuf::ProtobufEnum::value(&MessageType::MsgSnapshot),
                }))),
                &before,
                journal
            ),
            "rule: a snapshot never reaches the library through the loop, so no snapshot \
             Ready is ever taken (v1 never compacts, §7.4 step 2); v1 peers never exchange \
             MsgSnapshot, and the refusal names that rule"
        );
        assert_eq!(
            (run.violations.clone(), run.network.len()),
            (Vec::<String>::new(), 0),
            "rule: and nothing is sent or left ready"
        );
    }
}
