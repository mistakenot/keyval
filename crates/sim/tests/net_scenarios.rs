//! W16.2's network scenario tests (PLAN.md §9.1, §10.2): five real Ready loops (the real
//! `raft-rs` adapter and journal engine) exchange real sealed peer envelopes over the
//! simulated network, driven by the discrete-event scheduler, under each network fault the
//! fault model names. Every run is judged on one leader per term and one applied history,
//! and on the scenario's own outcome.
//!
//! Claims are simulation evidence: a fixed seed per scenario, never proof (§10.15).
#![cfg(test)]

mod scenarios {
    // check: SIM-NET-SCENARIOS

    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};
    use std::path::Path;
    use std::rc::Rc;

    use journal::{FrameKind, Genesis, LogBounds, LogicalLog};
    use raft_adapter::{Apply, Fed, PeerCodec, RaftInput, RaftNode, ReadyLoop, Synced};
    use sim::{
        Arrival, ClientId, Endpoint, EventBudget, LinkProfile, MasterSeed, NetConfig, NetCounts,
        Network, PacketId, Ppm, QueueBound, Scheduler, Step, Subsystem, VirtualTime,
    };
    use types::{
        ByteCount, Config, Effect, Entry, EntryPayload, Incarnation, InstanceId, ItemCount,
        JournalRead, LogIndex, NodeId, OutboundMessage, ReadLen, Role, StorageError, SyncSeq,
    };

    const LOCAL: &str = include_str!("../../../config/local.toml");
    /// Every simulated disk sync takes this long.
    const SYNC_US: u64 = 500;
    /// A deferred peer message is offered again after this long.
    const RETRY_US: u64 = 200;
    /// A client proposes its command again if it has not committed within this long.
    const CLIENT_DEADLINE_US: u64 = 5_000_000;
    /// The first sync number of every incarnation.
    const FIRST_SYNC: SyncSeq = SyncSeq::new(1);

    fn local() -> Config {
        Config::parse(LOCAL, Path::new("config")).unwrap()
    }

    /// A journal file in memory, shared between the harness and the node's storage view.
    #[derive(Clone, Debug, Default)]
    struct MemJournal(Rc<RefCell<Vec<u8>>>);

    impl MemJournal {
        fn push(&self, bytes: &[u8]) {
            self.0.borrow_mut().extend_from_slice(bytes);
        }

        fn bytes(&self) -> Vec<u8> {
            self.0.borrow().clone()
        }
    }

    impl JournalRead for MemJournal {
        fn read_at(&self, offset: ByteCount, len: ReadLen) -> Result<Vec<u8>, StorageError> {
            let bytes = self.0.borrow();
            let start = usize::try_from(offset.get()).map_err(|_| StorageError::Failed)?;
            let wanted = usize::try_from(len.get()).map_err(|_| StorageError::Failed)?;
            let available = bytes.get(start..).unwrap_or_default();
            if available.len() < wanted {
                return Err(StorageError::ShortRead {
                    wanted: len,
                    got: u32::try_from(available.len()).unwrap_or(u32::MAX),
                });
            }
            Ok(available[..wanted].to_vec())
        }
    }

    /// The entries one incarnation applied, in order.
    #[derive(Debug, Default)]
    struct Applied(Vec<Entry>);

    impl Apply for Applied {
        fn apply(&mut self, entry: &Entry, _commit: LogIndex, _effects: &mut Vec<Effect>) {
            self.0.push(entry.clone());
        }
    }

    /// A scheduled event.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Ev {
        /// A tick for one incarnation of a node.
        Tick(NodeId, u64),
        /// A packet copy arrives.
        Arrive(PacketId),
        /// A node's sync completes.
        SyncDone(NodeId, SyncSeq, Incarnation),
        /// Offer a node its deferred peer messages again.
        Retry(NodeId, u64),
        /// A client command, proposed at whichever node leads.
        Propose(Vec<u8>),
        /// Cut the links between these groups, both ways.
        Partition(Vec<Vec<NodeId>>),
        /// Cut one directed link.
        Cut(NodeId, NodeId),
        /// Restore every link.
        HealAll,
        /// Crash a node and restart it at once from what it synced.
        Restart(NodeId),
        /// A client's deadline: if its command has not committed, it proposes it again.
        Deadline(Vec<u8>),
    }

    /// One incarnation of one node.
    struct Node {
        lp: ReadyLoop<MemJournal>,
        journal: MemJournal,
        incarnation: u64,
        codec: PeerCodec,
        applied: Applied,
        /// How many of `applied` were checked against the one history.
        checked: usize,
        /// The journal bytes the last completed sync covers.
        synced: usize,
        /// The outstanding sync and the journal length it covers.
        pending: Option<(SyncSeq, usize)>,
        /// Peer payloads that arrived while a sync was outstanding, in arrival order.
        deferred: VecDeque<Vec<u8>>,
        /// When this incarnation started.
        started: VirtualTime,
    }

    /// One packet copy in flight, as the harness tracks it.
    #[derive(Clone, Copy, Debug)]
    struct Tracked {
        sent_at: VirtualTime,
        /// Its packet's number among those sent on its link.
        link_seq: u64,
    }

    /// What the scenario's faults did, beyond the network's own counts.
    #[derive(Clone, Copy, Debug, Default)]
    struct Seen {
        /// Packets delivered after a later-sent packet on the same link.
        out_of_order: u64,
        /// Packets delivered to a node's incarnation that started after they were sent.
        stale_to_restarted: u64,
        /// Completions from a crashed incarnation, ignored by its successor.
        stale_completions: u64,
        /// Client responses delivered.
        responses_delivered: u64,
    }

    /// Five Ready loops on the simulated network.
    struct World {
        config: Config,
        instance: InstanceId,
        master: MasterSeed,
        sched: Scheduler<Ev>,
        net: Network,
        nodes: BTreeMap<NodeId, Node>,
        tracked: BTreeMap<PacketId, Tracked>,
        link_sent: BTreeMap<(NodeId, NodeId), u64>,
        link_delivered: BTreeMap<(NodeId, NodeId), u64>,
        /// The one applied history every node and incarnation must follow.
        history: Vec<Entry>,
        /// The leader seen in each term.
        leaders: BTreeMap<u64, NodeId>,
        /// Commands proposed, by the client that will be answered.
        clients: BTreeMap<Vec<u8>, ClientId>,
        violations: Vec<String>,
        seen: Seen,
        tick_us: u64,
    }

    /// The genesis frame of `node`'s journal.
    fn genesis(config: &Config, node: NodeId, instance: InstanceId) -> Vec<u8> {
        let genesis = Genesis::new(config, node, instance).unwrap();
        journal::encode(FrameKind::Genesis, 1, &genesis.encode()).unwrap()
    }

    impl World {
        fn new(peer: LinkProfile, client: LinkProfile, queue: QueueBound, seed: u64) -> Self {
            let config = local();
            let master = MasterSeed::new(seed);
            let net = Network::new(
                NetConfig {
                    peer,
                    client,
                    queue,
                    max_in_flight: EventBudget::new(2_000_000),
                },
                master.stream(Subsystem::Network),
            );
            let sched = Scheduler::new(
                master.stream(Subsystem::Scheduler),
                EventBudget::new(2_000_000),
            );
            let tick_us = config.tick_ms() * 1_000;
            let mut world = Self {
                instance: InstanceId::from_bytes([0x5c; 16]),
                config,
                master,
                sched,
                net,
                nodes: BTreeMap::new(),
                tracked: BTreeMap::new(),
                link_sent: BTreeMap::new(),
                link_delivered: BTreeMap::new(),
                history: Vec::new(),
                leaders: BTreeMap::new(),
                clients: BTreeMap::new(),
                violations: Vec::new(),
                seen: Seen::default(),
                tick_us,
            };
            let voters: Vec<NodeId> = world.config.voters().collect();
            for id in voters {
                let bytes = genesis(&world.config, id, world.instance);
                world.start(id, &bytes, 1);
            }
            world
        }

        fn ids(&self) -> Vec<NodeId> {
            self.nodes.keys().copied().collect()
        }

        fn now(&self) -> VirtualTime {
            self.sched.now()
        }

        fn at(&mut self, micros: u64, event: Ev) {
            self.sched
                .schedule(VirtualTime::from_micros(micros), None, event)
                .unwrap();
        }

        fn after(&mut self, micros: u64, event: Ev) {
            let at = self.now().as_micros().saturating_add(micros);
            self.at(at, event);
        }

        /// Starts `id`'s incarnation `incarnation` over the journal `bytes`, as startup does
        /// (§7.7): recover, keep the durable prefix, replay committed entries from index 1.
        fn start(&mut self, id: NodeId, bytes: &[u8], incarnation: u64) {
            let scratch = MemJournal::default();
            scratch.push(bytes);
            let len = ByteCount::new(u64::try_from(bytes.len()).unwrap());
            let (_, log): (_, LogicalLog) =
                journal::recover(&scratch, len, &self.config, id, LogBounds::of(&self.config))
                    .unwrap()
                    .into_parts();
            let end = usize::try_from(log.end()).unwrap();
            let journal = MemJournal::default();
            journal.push(&bytes[..end]);
            let node = RaftNode::new(
                &self.config.consensus(id).unwrap(),
                self.config.voters(),
                log,
                journal.clone(),
                self.master.node(id, Incarnation::new(incarnation)),
                LogIndex::ZERO,
            )
            .unwrap();
            let codec = PeerCodec::new(
                self.instance,
                self.config.semantic_fingerprint(),
                id,
                self.config.voters(),
            )
            .unwrap();
            self.nodes.insert(
                id,
                Node {
                    lp: ReadyLoop::new(node, Incarnation::new(incarnation), FIRST_SYNC),
                    journal,
                    incarnation,
                    codec,
                    applied: Applied::default(),
                    checked: 0,
                    synced: end,
                    pending: None,
                    deferred: VecDeque::new(),
                    started: self.now(),
                },
            );
            let node = self.nodes.get_mut(&id).unwrap();
            let mut effects = Vec::new();
            node.lp.poll(&mut node.applied, &mut effects).unwrap();
            self.execute(id, effects);
            self.after(1, Ev::Tick(id, incarnation));
        }

        /// Executes `id`'s effects as a composition root would: frames to its journal,
        /// syncs to the scheduler, messages sealed and sent over the network.
        fn execute(&mut self, id: NodeId, effects: Vec<Effect>) {
            for effect in effects {
                let node = self.nodes.get_mut(&id).unwrap();
                match effect {
                    Effect::Append { frame } => node.journal.push(&frame),
                    Effect::Sync { seq, incarnation } => {
                        node.pending = Some((seq, node.journal.bytes().len()));
                        self.after(SYNC_US, Ev::SyncDone(id, seq, incarnation));
                    }
                    Effect::Send { to, bytes } => {
                        if node.pending.is_some() {
                            self.violations
                                .push(format!("node {id} sent while its sync was outstanding"));
                        }
                        let envelope = node.codec.seal(&OutboundMessage { to, bytes }).unwrap();
                        self.send(Endpoint::Node(id), Endpoint::Node(to), &envelope);
                    }
                    other => self.violations.push(format!("node {id} emitted {other:?}")),
                }
            }
            self.check(id);
        }

        fn send(&mut self, from: Endpoint, to: Endpoint, bytes: &[u8]) {
            let now = self.now();
            let sent = match self.net.send(now, from, to, bytes) {
                Ok(sent) => sent,
                Err(error) => {
                    self.violations.push(format!(
                        "the network refused a send ({error:?}): the run is incomplete (P10)"
                    ));
                    return;
                }
            };
            let link_seq = match (from, to) {
                (Endpoint::Node(a), Endpoint::Node(b)) => {
                    let next = self.link_sent.entry((a, b)).or_insert(0);
                    *next += 1;
                    *next
                }
                (Endpoint::Node(_) | Endpoint::Client(_), Endpoint::Client(_))
                | (Endpoint::Client(_), Endpoint::Node(_)) => 0,
            };
            for (at, packet) in sent.arrivals {
                self.tracked.insert(
                    packet,
                    Tracked {
                        sent_at: now,
                        link_seq,
                    },
                );
                self.at(at.as_micros(), Ev::Arrive(packet));
            }
        }

        /// One leader per term, and one applied history; answers clients whose commands the
        /// leader applied.
        fn check(&mut self, id: NodeId) {
            let node = self.nodes.get_mut(&id).unwrap();
            let fresh: Vec<Entry> = node.applied.0[node.checked..].to_vec();
            node.checked = node.applied.0.len();
            let status = node.lp.status().clone();
            if status.role == Role::Leader {
                let first = *self.leaders.entry(status.term.get()).or_insert(id);
                if first != id {
                    self.violations.push(format!(
                        "nodes {first} and {id} both led term {}",
                        status.term.get()
                    ));
                }
            }
            for entry in fresh {
                let index = usize::try_from(entry.index.get()).unwrap();
                match self.history.get(index - 1) {
                    Some(known) if *known == entry => {}
                    Some(known) => self.violations.push(format!(
                        "node {id} applied {entry:?} where the history holds {known:?}"
                    )),
                    None if index == self.history.len() + 1 => self.history.push(entry.clone()),
                    None => self
                        .violations
                        .push(format!("node {id} applied {entry:?} past the history")),
                }
                // The leader answers the client once it has applied the command.
                if status.role == Role::Leader
                    && let EntryPayload::Command(command) = &entry.payload
                    && let Some(client) = self.clients.remove(command)
                {
                    self.send(Endpoint::Node(id), Endpoint::Client(client), b"ok");
                }
            }
        }

        fn feed(&mut self, id: NodeId, input: RaftInput) -> Option<Fed> {
            let node = self.nodes.get_mut(&id)?;
            let mut effects = Vec::new();
            let fed = node
                .lp
                .feed(input, &mut node.applied, &mut effects)
                .unwrap();
            self.execute(id, effects);
            Some(fed)
        }

        fn arrive(&mut self, packet: PacketId) {
            let tracked = self.tracked.remove(&packet);
            let Arrival::Delivered(delivery, _) = self.net.arrive(packet) else {
                return;
            };
            let (Endpoint::Node(from), Endpoint::Node(to)) = (delivery.from, delivery.to) else {
                self.seen.responses_delivered += 1;
                return;
            };
            let Some(node) = self.nodes.get_mut(&to) else {
                return;
            };
            if let Some(tracked) = tracked {
                let last = self.link_delivered.entry((from, to)).or_insert(0);
                if tracked.link_seq < *last {
                    self.seen.out_of_order += 1;
                }
                *last = (*last).max(tracked.link_seq);
                if tracked.sent_at < node.started {
                    self.seen.stale_to_restarted += 1;
                }
            }
            match node.codec.open(from, &delivery.bytes) {
                Ok(payload) => {
                    node.deferred.push_back(payload);
                    self.offer(to);
                }
                Err(error) => self.violations.push(format!(
                    "node {to} dropped a sealed envelope from {from}: {error:?}"
                )),
            }
        }

        /// Feeds `id` its deferred peer messages until one is deferred again.
        fn offer(&mut self, id: NodeId) {
            while let Some(payload) = self.nodes.get_mut(&id).and_then(|n| n.deferred.pop_front()) {
                if let Some(Fed::Deferred(RaftInput::Peer(payload))) =
                    self.feed(id, RaftInput::Peer(payload))
                {
                    let node = self.nodes.get_mut(&id).unwrap();
                    node.deferred.push_front(payload);
                    let incarnation = node.incarnation;
                    self.after(RETRY_US, Ev::Retry(id, incarnation));
                    return;
                }
            }
        }

        fn sync_done(&mut self, id: NodeId, seq: SyncSeq, incarnation: Incarnation) {
            let Some(node) = self.nodes.get_mut(&id) else {
                return;
            };
            if Incarnation::new(node.incarnation) != incarnation {
                let mut effects = Vec::new();
                let done = node
                    .lp
                    .sync_done(seq, incarnation, &mut node.applied, &mut effects)
                    .unwrap();
                if done == Synced::Stale && effects.is_empty() {
                    self.seen.stale_completions += 1;
                } else {
                    self.violations
                        .push(format!("node {id} took a crashed incarnation's completion"));
                }
                return;
            }
            if let Some((_, covered)) = node.pending.take() {
                node.synced = covered;
            }
            let mut effects = Vec::new();
            let done = node
                .lp
                .sync_done(seq, incarnation, &mut node.applied, &mut effects)
                .unwrap();
            if done != Synced::Completed {
                self.violations
                    .push(format!("node {id}'s own sync came back {done:?}"));
            }
            self.execute(id, effects);
            self.offer(id);
        }

        fn propose(&mut self, command: Vec<u8>) {
            let leader = self
                .ids()
                .into_iter()
                .filter(|id| self.nodes[id].lp.status().role == Role::Leader)
                .max_by_key(|id| self.nodes[id].lp.status().term);
            let accepted = leader.is_some_and(|leader| {
                matches!(
                    self.feed(leader, RaftInput::Propose(command.clone())),
                    Some(Fed::Proposed(Ok(())))
                )
            });
            if accepted {
                // A leader cut off in a minority accepts proposals it can never commit: the
                // client retries if its command has not committed by its deadline.
                self.after(CLIENT_DEADLINE_US, Ev::Deadline(command));
            } else {
                // No leader yet, or it is busy: the client retries.
                self.after(self.tick_us, Ev::Propose(command));
            }
        }

        fn restart(&mut self, id: NodeId) {
            let node = self.nodes.remove(&id).unwrap();
            let bytes = node.journal.bytes();
            self.start(id, &bytes[..node.synced], node.incarnation + 1);
        }

        fn dispatch(&mut self, event: Ev) {
            match event {
                Ev::Tick(id, incarnation) => {
                    if self
                        .nodes
                        .get(&id)
                        .is_some_and(|n| n.incarnation == incarnation)
                    {
                        let _ = self.feed(id, RaftInput::Tick);
                        self.after(self.tick_us, Ev::Tick(id, incarnation));
                    }
                }
                Ev::Arrive(packet) => self.arrive(packet),
                Ev::SyncDone(id, seq, incarnation) => self.sync_done(id, seq, incarnation),
                Ev::Retry(id, incarnation) => {
                    if self
                        .nodes
                        .get(&id)
                        .is_some_and(|n| n.incarnation == incarnation)
                    {
                        self.offer(id);
                    }
                }
                Ev::Propose(command) => self.propose(command),
                Ev::Partition(groups) => {
                    let groups: Vec<&[NodeId]> = groups.iter().map(Vec::as_slice).collect();
                    self.net.links_mut().partition(&groups);
                }
                Ev::Cut(from, to) => self.net.links_mut().cut(from, to),
                Ev::HealAll => self.net.links_mut().heal_all(),
                Ev::Restart(id) => self.restart(id),
                Ev::Deadline(command) => {
                    if !self.committed().contains(&command) {
                        self.propose(command);
                    }
                }
            }
        }

        /// Runs every event up to `until` virtual microseconds.
        fn run_until(&mut self, until: u64) {
            loop {
                match self.sched.step() {
                    Step::Event(event) => {
                        let late = event.time.as_micros() > until;
                        self.dispatch(event.payload);
                        if late {
                            return;
                        }
                    }
                    Step::Complete { .. } => return,
                    Step::Incomplete { reason, .. } => {
                        panic!("fixture: the run is incomplete: {reason:?}")
                    }
                }
            }
        }

        /// Proposes `count` commands, `gap` microseconds apart from `from`, each answered
        /// to its own client.
        fn workload(&mut self, from: u64, gap: u64, count: u64) {
            for n in 0..count {
                let command = format!("cmd-{n}").into_bytes();
                self.clients.insert(command.clone(), ClientId::new(n));
                self.at(from + n * gap, Ev::Propose(command));
            }
        }

        /// The commands in the one applied history.
        fn committed(&self) -> Vec<Vec<u8>> {
            self.history
                .iter()
                .filter_map(|entry| match &entry.payload {
                    EntryPayload::Command(command) => Some(command.clone()),
                    EntryPayload::Empty => None,
                })
                .collect()
        }

        /// Asserts safety held, and that every node applied every committed command.
        fn assert_safe_and_converged(&self, scenario: &str, commands: u64) {
            assert_eq!(
                self.violations,
                Vec::<String>::new(),
                "rule: one leader per term, one applied history, nothing sent on unsynced state \
                 and every envelope opened (§7.4, §8.2); scenario={scenario}"
            );
            let mut committed = self.committed();
            committed.sort();
            committed.dedup();
            assert_eq!(
                committed.len(),
                usize::try_from(commands).unwrap(),
                "rule: every command commits once the faults heal (C-2, A-1); scenario={scenario} \
                 committed={committed:?}"
            );
            for (id, node) in &self.nodes {
                assert_eq!(
                    node.applied.0.len(),
                    self.history.len(),
                    "rule: every node applies the whole history; scenario={scenario} node={id}"
                );
            }
        }

        fn counts(&self) -> NetCounts {
            self.net.counts()
        }
    }

    fn id(raw: u64) -> NodeId {
        NodeId::new(raw).unwrap()
    }

    /// Peer links with 1 to 10 ms delay and the given loss and duplication.
    fn peer(loss: u32, duplicate: u32) -> LinkProfile {
        LinkProfile::new(
            1_000,
            10_000,
            Ppm::new(loss).unwrap(),
            Ppm::new(duplicate).unwrap(),
        )
        .unwrap()
    }

    fn world(peer: LinkProfile, seed: u64) -> World {
        World::new(
            peer,
            LinkProfile::reliable(1_000),
            QueueBound::of(&local()),
            seed,
        )
    }

    /// Ten commands from second 3, half a second apart; then 30 seconds to settle.
    fn run_workload(world: &mut World) {
        world.workload(3_000_000, 500_000, 10);
        world.run_until(40_000_000);
    }

    #[test]
    fn a_lossy_network_still_commits_every_command() {
        let mut world = world(peer(150_000, 0), 1);
        run_workload(&mut world);
        world.assert_safe_and_converged("loss", 10);
        assert!(
            world.counts().lost > 100,
            "fixture: messages were lost: {:?}",
            world.counts()
        );
    }

    #[test]
    fn delay_reorders_messages_and_the_cluster_copes() {
        let mut world = world(
            LinkProfile::new(1_000, 80_000, Ppm::NEVER, Ppm::NEVER).unwrap(),
            2,
        );
        run_workload(&mut world);
        world.assert_safe_and_converged("delay and reorder", 10);
        assert!(
            world.seen.out_of_order > 50,
            "fixture: messages on one link were delivered out of send order: {:?}",
            world.seen
        );
    }

    #[test]
    fn duplicated_messages_never_apply_twice() {
        let mut world = world(peer(0, 300_000), 3);
        run_workload(&mut world);
        world.assert_safe_and_converged("duplication", 10);
        assert!(
            world.counts().duplicated > 100,
            "fixture: {:?}",
            world.counts()
        );
        let committed = world.committed();
        let mut unique = committed.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            committed.len(),
            "rule: no command applies twice"
        );
    }

    #[test]
    fn a_symmetric_partition_with_the_old_leader_in_the_minority_heals() {
        let mut world = world(peer(0, 0), 4);
        world.run_until(3_000_000);
        let old = world.leaders.values().next_back().copied().unwrap();
        let mut minority = vec![old];
        minority.extend(world.ids().into_iter().find(|&id| id != old));
        let majority: Vec<NodeId> = world
            .ids()
            .into_iter()
            .filter(|id| !minority.contains(id))
            .collect();
        world.after(1, Ev::Partition(vec![minority.clone(), majority.clone()]));
        world.workload(3_500_000, 500_000, 10);
        world.at(15_000_000, Ev::HealAll);
        world.run_until(45_000_000);
        world.assert_safe_and_converged("3/2 partition", 10);
        let (&term, &new) = world.leaders.iter().next_back().unwrap();
        assert!(
            majority.contains(&new) && world.counts().cut_at_send > 0,
            "rule: the majority elects a new leader in a later term, and traffic across the \
             split was cut; old={old} new={new} term={term} counts={:?}",
            world.counts()
        );
    }

    #[test]
    fn a_leader_that_can_send_but_not_receive_is_replaced() {
        let mut world = world(peer(0, 0), 5);
        world.run_until(3_000_000);
        let old = world.leaders.values().next_back().copied().unwrap();
        for other in world.ids().into_iter().filter(|&id| id != old) {
            world.after(1, Ev::Cut(other, old));
        }
        world.workload(3_500_000, 500_000, 10);
        world.at(15_000_000, Ev::HealAll);
        world.run_until(45_000_000);
        world.assert_safe_and_converged("asymmetric cut", 10);
        let new = world.leaders.values().next_back().copied().unwrap();
        assert!(
            new != old && world.counts().cut_at_send > 0,
            "rule: a leader cut off from every answer loses its lead (check-quorum), and another \
             voter leads a later term; old={old} new={new}"
        );
    }

    #[test]
    fn a_full_link_queue_drops_and_counts_and_raft_recovers() {
        let mut world = World::new(
            LinkProfile::new(20_000, 60_000, Ppm::NEVER, Ppm::NEVER).unwrap(),
            LinkProfile::reliable(1_000),
            QueueBound {
                messages: ItemCount::new(2),
                bytes: ByteCount::new(8 << 20),
            },
            6,
        );
        run_workload(&mut world);
        world.assert_safe_and_converged("queue overflow", 10);
        assert!(
            world.counts().queue_full > 0,
            "fixture: a link queue overflowed and the drops were counted: {:?}",
            world.counts()
        );
    }

    #[test]
    fn stale_packets_reach_a_restarted_node_as_ordinary_stale_messages() {
        let mut world = world(
            LinkProfile::new(1_000, 400_000, Ppm::NEVER, Ppm::NEVER).unwrap(),
            7,
        );
        world.workload(3_000_000, 300_000, 10);
        for (at, node) in [(4_000_000, 2), (5_000_000, 3), (6_500_000, 2)] {
            world.at(at, Ev::Restart(id(node)));
        }
        world.run_until(40_000_000);
        world.assert_safe_and_converged("restart with packets in flight", 10);
        assert!(
            world.seen.stale_to_restarted > 0,
            "rule: packets are not tied to incarnations: some sent to a node before its restart \
             reach the new incarnation and are stepped as ordinary stale messages (§9.1); {:?}",
            world.seen
        );
    }

    #[test]
    fn a_client_response_lost_after_a_committed_application() {
        let mut world = World::new(
            peer(0, 0),
            LinkProfile::new(1_000, 1_000, Ppm::ALWAYS, Ppm::NEVER).unwrap(),
            QueueBound::of(&local()),
            8,
        );
        world.workload(3_000_000, 500_000, 3);
        world.run_until(20_000_000);
        world.assert_safe_and_converged("client response loss", 3);
        assert_eq!(
            (
                world.seen.responses_delivered,
                world.counts().lost,
                world.clients.len()
            ),
            (0, 3, 0),
            "rule: each command committed and applied, and every answer to its client was lost: \
             the client cannot tell, which W14's retries and status lookups resolve (§9.1)"
        );
    }
}
