//! The god view (PLAN.md §10.2; T-5): every node's Raft, journal and applied state after
//! every step, for the invariant checks that may use it (§10.5):
//! - applied state as the applied index and the [`StateDigest`] of the map and request
//!   history, which convergence compares (K-7, the heal phase);
//! - each log entry's acceptance term, the term in which the replica durably accepted it,
//!   which commit certificates need (K-8; br-dbn0).
//!
//! A view borrows the composition root's state, so taking one after every step copies
//! nothing. [`Changes`] turns successive views into the canonical trace's role and term,
//! log, commit, apply and journal records, so the trace and the checkers see the same
//! state. Neither holds a generator, so observing a run never consumes its randomness.

use std::collections::BTreeMap;

use statemachine::StateDigest;
#[cfg(test)]
use types::EntryIndex;
use types::{Entry, HardState, LogIndex, NodeId, Role, Term};

use crate::scheduler::NodeIncarnation;
use crate::trace::Record;

/// A node's journal: how far it is appended and how far it is durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalView {
    /// The last appended index.
    pub appended: LogIndex,
    /// The last durable (synced) index.
    pub synced: LogIndex,
}

/// One node's state after a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeView<'a> {
    /// The node and its current incarnation.
    pub node: NodeIncarnation,
    /// Its Raft role.
    pub role: Role,
    /// Its Raft hard state: term, vote and commit index.
    pub hard: HardState,
    /// Its Raft log, in index order.
    pub log: &'a [Entry],
    /// The acceptance term of each entry of `log`, position by position: the term in which
    /// this replica durably accepted it, which can be later than the entry's own term
    /// (K-8). [`GodView::new`] refuses a view whose two lengths differ.
    pub accepted: &'a [Term],
    /// Its journal.
    pub journal: JournalView,
    /// Its applied index.
    pub applied: LogIndex,
    /// The digest of its applied state: the map and the request history at `applied`
    /// ([`statemachine::StateMachine::digest`], K-7).
    pub state: StateDigest,
}

impl NodeView<'_> {
    /// The term in which this replica durably accepted the entry at `index`, if its log
    /// holds that entry (K-8's commit certificates).
    #[must_use]
    pub fn acceptance_term(&self, index: LogIndex) -> Option<Term> {
        self.log
            .iter()
            .position(|entry| entry.index.index() == index)
            .and_then(|position| self.accepted.get(position).copied())
    }
}

/// Every node's state after a step, in node-ID order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GodView<'a> {
    nodes: Vec<NodeView<'a>>,
}

impl<'a> GodView<'a> {
    /// The view of `nodes`, sorted by node ID, or `None` if two share an ID or a view's
    /// acceptance terms do not match its log entry for entry.
    #[must_use]
    pub fn new(mut nodes: Vec<NodeView<'a>>) -> Option<Self> {
        nodes.sort_by_key(|view| view.node.node);
        let unique = nodes
            .windows(2)
            .all(|pair| matches!(pair, [first, second] if first.node.node != second.node.node));
        let accepted = nodes
            .iter()
            .all(|view| view.accepted.len() == view.log.len());
        (unique && accepted).then_some(Self { nodes })
    }

    /// Every node's state, in node-ID order.
    #[must_use]
    pub fn nodes(&self) -> &[NodeView<'a>] {
        &self.nodes
    }

    /// The state of `node`, if the view has it.
    #[must_use]
    pub fn node(&self, node: NodeId) -> Option<&NodeView<'a>> {
        self.nodes
            .binary_search_by_key(&node, |view| view.node.node)
            .ok()
            .and_then(|position| self.nodes.get(position))
    }
}

/// What the trace last recorded for a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Summary {
    node: NodeIncarnation,
    role: Role,
    term: Term,
    last: (LogIndex, Term),
    commit: LogIndex,
    applied: LogIndex,
    journal: JournalView,
}

impl Summary {
    fn of(view: &NodeView<'_>) -> Self {
        Self {
            node: view.node,
            role: view.role,
            term: view.hard.term,
            last: view
                .log
                .last()
                .map_or((LogIndex::ZERO, Term::ZERO), |entry| {
                    (entry.index.index(), entry.term)
                }),
            commit: view.hard.commit,
            applied: view.applied,
            journal: view.journal,
        }
    }
}

/// Turns successive god views into canonical trace records.
#[derive(Clone, Debug, Default)]
pub struct Changes {
    last: BTreeMap<NodeId, Summary>,
}

impl Changes {
    /// A tracker that has seen no view yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last: BTreeMap::new(),
        }
    }

    /// The records for what changed since the previous view, in node-ID order. A node
    /// seen for the first time, or in a new incarnation, has every record; otherwise only
    /// the fields that moved are recorded.
    pub fn since_last(&mut self, view: &GodView<'_>) -> Vec<Record> {
        let mut records = Vec::new();
        for node in view.nodes() {
            let now = Summary::of(node);
            let before = self
                .last
                .insert(node.node.node, now)
                .filter(|previous| previous.node == now.node);
            let moved = |field: fn(&Summary) -> (u64, u64)| {
                before.is_none_or(|previous| field(&previous) != field(&now))
            };
            if before.is_none_or(|previous| (previous.role, previous.term) != (now.role, now.term))
            {
                records.push(Record::Role {
                    node: now.node,
                    role: now.role,
                    term: now.term,
                });
            }
            if moved(|summary| (summary.last.0.get(), summary.last.1.get())) {
                records.push(Record::Log {
                    node: now.node,
                    last_index: now.last.0,
                    last_term: now.last.1,
                });
            }
            if moved(|summary| (summary.commit.get(), 0)) {
                records.push(Record::Commit {
                    node: now.node,
                    index: now.commit,
                });
            }
            if moved(|summary| (summary.applied.get(), 0)) {
                records.push(Record::Applied {
                    node: now.node,
                    index: now.applied,
                });
            }
            if moved(|summary| (summary.journal.appended.get(), summary.journal.synced.get())) {
                records.push(Record::Journal {
                    node: now.node,
                    appended: now.journal.appended,
                    synced: now.journal.synced,
                });
            }
        }
        records
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entropy::{MasterSeed, Subsystem};
    use crate::scheduler::{EventBudget, Scheduler, Step, VirtualTime};
    use crate::trace::{Name, ObservationBound, Trace};
    use statemachine::{Command, StateLimits, StateMachine, codec};
    use types::{
        ByteCount, EntryPayload, Incarnation, ItemCount, Key, KeyValueLimits, Mutation, RequestId,
        Value,
    };

    fn on(node: u64, incarnation: u64) -> NodeIncarnation {
        NodeIncarnation {
            node: NodeId::new(node).unwrap(),
            incarnation: Incarnation::new(incarnation),
        }
    }

    fn index(raw: u64) -> LogIndex {
        LogIndex::new(raw).unwrap()
    }

    fn term(raw: u64) -> Term {
        Term::new(raw).unwrap()
    }

    fn entry(at: u64, in_term: u64) -> Entry {
        Entry {
            index: EntryIndex::new(at).unwrap(),
            term: term(in_term),
            payload: EntryPayload::Command(vec![u8::try_from(at).unwrap()]),
        }
    }

    /// Acceptance terms for the tests that do not look at them.
    const ACCEPTED: [Term; 4] = [Term::ZERO; 4];

    fn machine() -> StateMachine {
        let limits = StateLimits {
            key_values: KeyValueLimits::MAXIMA,
            max_live_keys: ItemCount::new(100),
            max_live_bytes: ByteCount::new(1 << 20),
        };
        StateMachine::new(NodeId::new(1).unwrap(), limits)
    }

    fn view(node: NodeIncarnation, log: &[Entry], commit: u64, applied: u64) -> NodeView<'_> {
        NodeView {
            node,
            role: Role::Follower,
            hard: HardState {
                term: term(1),
                vote: None,
                commit: index(commit),
            },
            log,
            accepted: &ACCEPTED[..log.len()],
            journal: JournalView {
                appended: index(u64::try_from(log.len()).unwrap()),
                synced: index(commit),
            },
            applied: index(applied),
            state: machine().digest(),
        }
    }

    /// Applies `entries` to a fresh state machine with every entry committed.
    fn applied(entries: &[Entry]) -> StateMachine {
        let mut state = machine();
        let commit = entries
            .last()
            .map_or(LogIndex::ZERO, |entry| entry.index.index());
        for entry in entries {
            let _ = state.apply(entry, commit);
        }
        state
    }

    fn put_entry(at: u64, request: u8, key: &str, value: &str) -> Entry {
        let command = Command {
            request_id: RequestId::from_bytes([request; 16]),
            mutation: Mutation::Put {
                key: Key::parse(key.to_owned(), KeyValueLimits::MAXIMA).unwrap(),
                value: Value::parse(value.to_owned(), KeyValueLimits::MAXIMA).unwrap(),
            },
        };
        Entry {
            index: EntryIndex::new(at).unwrap(),
            term: term(1),
            payload: EntryPayload::Command(codec::encode(&command)),
        }
    }

    fn lines(records: &[Record]) -> Vec<String> {
        records.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_god_view_lists_every_node_in_id_order() {
        let log = [entry(1, 1)];
        let god = GodView::new(vec![
            view(on(3, 1), &log, 1, 1),
            view(on(1, 2), &log, 0, 0),
            view(on(2, 1), &[], 0, 0),
        ])
        .unwrap();
        let ids: Vec<u64> = god
            .nodes()
            .iter()
            .map(|node| node.node.node.get())
            .collect();
        assert_eq!(ids, [1, 2, 3], "rule: the view is in node-ID order");
        assert_eq!(
            god.node(NodeId::new(1).unwrap())
                .map(|node| node.node.incarnation),
            Some(Incarnation::new(2))
        );
        assert_eq!(
            god.node(NodeId::new(3).unwrap()).map(|node| node.log),
            Some(&log[..]),
            "rule: the view exposes the Raft log itself (T-5)"
        );
        assert_eq!(god.node(NodeId::new(4).unwrap()), None);
        assert_eq!(
            GodView::new(vec![view(on(1, 1), &[], 0, 0), view(on(1, 2), &[], 0, 0)]),
            None,
            "rule: one view per node"
        );
    }

    #[test]
    fn the_view_carries_each_nodes_applied_state_digest() {
        let log = [put_entry(1, 1, "k", "a"), put_entry(2, 2, "j", "b")];
        let mut trailing = log.to_vec();
        trailing.push(Entry {
            index: EntryIndex::new(3).unwrap(),
            term: term(2),
            payload: EntryPayload::Empty,
        });
        let diverged = [put_entry(1, 1, "k", "a"), put_entry(2, 2, "j", "c")];
        let accepted = [term(1), term(1), term(2)];
        let mut one = view(on(1, 1), &log, 2, 2);
        one.state = applied(&log).digest();
        let mut two = view(on(2, 1), &trailing, 3, 3);
        two.accepted = &accepted;
        two.state = applied(&trailing).digest();
        let mut three = view(on(3, 1), &diverged, 2, 2);
        three.state = applied(&diverged).digest();
        let god = GodView::new(vec![one, two, three]).unwrap();
        let state = |node: u64| god.node(NodeId::new(node).unwrap()).map(|view| view.state);
        assert_eq!(
            state(1),
            state(2),
            "rule: a trailing election no-op leaves applied state equal, though the applied \
             indices differ (K-7, T-5)"
        );
        assert_ne!(
            state(1),
            state(3),
            "rule: a different applied value shows in the digest (K-7)"
        );
        assert_ne!(
            god.node(NodeId::new(1).unwrap()).map(|view| view.state),
            Some(machine().digest()),
            "rule: the digest is of the applied state, not an empty machine"
        );
    }

    #[test]
    fn the_view_records_each_entrys_acceptance_term() {
        let log = [entry(1, 1), entry(2, 1)];
        let accepted = [term(1), term(3)];
        let mut node = view(on(1, 1), &log, 2, 2);
        node.accepted = &accepted;
        let god = GodView::new(vec![node]).unwrap();
        let only = god.node(NodeId::new(1).unwrap()).unwrap();
        assert_eq!(
            (
                only.acceptance_term(index(1)),
                only.acceptance_term(index(2))
            ),
            (Some(term(1)), Some(term(3))),
            "rule: an old-term entry accepted later keeps its acceptance term, which a commit \
             certificate needs (K-8)"
        );
        assert_eq!(only.acceptance_term(index(3)), None, "no such entry");
        let mut short = view(on(1, 1), &log, 2, 2);
        short.accepted = &accepted[..1];
        assert_eq!(
            GodView::new(vec![short]),
            None,
            "rule: every entry has exactly one acceptance term"
        );
    }

    #[test]
    fn the_first_view_records_every_nodes_state() {
        let log = [entry(1, 1), entry(2, 1)];
        let god = GodView::new(vec![view(on(1, 1), &log, 2, 1)]).unwrap();
        assert_eq!(
            lines(&Changes::new().since_last(&god)),
            [
                "role n1/1 follower 1",
                "log n1/1 2 1",
                "commit n1/1 2",
                "applied n1/1 1",
                "journal n1/1 2 2",
            ]
        );
    }

    #[test]
    fn later_views_record_only_what_moved() {
        let mut changes = Changes::new();
        let short = [entry(1, 1)];
        let long = [entry(1, 1), entry(2, 1)];
        let _ = changes.since_last(&GodView::new(vec![view(on(1, 1), &short, 1, 1)]).unwrap());
        assert_eq!(
            changes.since_last(&GodView::new(vec![view(on(1, 1), &short, 1, 1)]).unwrap()),
            Vec::<Record>::new(),
            "rule: an unchanged node adds nothing"
        );
        assert_eq!(
            lines(&changes.since_last(&GodView::new(vec![view(on(1, 1), &long, 1, 1)]).unwrap())),
            ["log n1/1 2 1", "journal n1/1 2 1"]
        );
        let mut leader = view(on(1, 1), &long, 2, 2);
        leader.role = Role::Leader;
        leader.hard.term = term(2);
        assert_eq!(
            lines(&changes.since_last(&GodView::new(vec![leader]).unwrap())),
            [
                "role n1/1 leader 2",
                "commit n1/1 2",
                "applied n1/1 2",
                "journal n1/1 2 2",
            ]
        );
    }

    #[test]
    fn a_restart_records_the_new_incarnation_afresh() {
        let mut changes = Changes::new();
        let log = [entry(1, 1)];
        let _ = changes.since_last(&GodView::new(vec![view(on(1, 1), &log, 1, 1)]).unwrap());
        let records = changes.since_last(&GodView::new(vec![view(on(1, 2), &log, 1, 1)]).unwrap());
        assert_eq!(
            lines(&records),
            [
                "role n1/2 follower 1",
                "log n1/2 1 1",
                "commit n1/2 1",
                "applied n1/2 1",
                "journal n1/2 1 1",
            ],
            "rule: a new incarnation's state is recorded in full"
        );
    }

    #[test]
    fn observing_the_god_view_never_shifts_the_schedule() {
        let run = |observe: bool| {
            let mut scheduler = Scheduler::new(
                MasterSeed::new(21).stream(Subsystem::Scheduler),
                EventBudget::new(64),
            );
            for payload in 0..4_u64 {
                scheduler
                    .schedule(VirtualTime::ZERO, Some(on(payload + 1, 1)), payload)
                    .unwrap();
            }
            let log = [entry(1, 1)];
            let mut trace = Trace::new(ObservationBound::DEFAULT);
            let mut changes = Changes::new();
            let mut order = Vec::new();
            while let Step::Event(event) = scheduler.step() {
                order.push((event.seq, event.tie));
                if observe {
                    trace.event(&event, &Name::new("tick").unwrap());
                    let god = GodView::new(vec![view(on(1, 1), &log, 1, event.seq.get())]).unwrap();
                    for record in changes.since_last(&god) {
                        trace.record(&record).unwrap();
                    }
                }
                let at = VirtualTime::from_micros(event.time.as_micros() + 10);
                scheduler.schedule(at, event.node, event.payload).unwrap();
            }
            order
        };
        assert_eq!(
            run(true),
            run(false),
            "rule: god-view and trace observation consume no scheduler or protocol entropy"
        );
    }
}
