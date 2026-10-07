#![cfg(test)]
//! Hand-made god views for K-8's tests: five voters, each recovered with an empty log, and a
//! builder that writes the node-side events the judge reads.

use std::collections::BTreeSet;

use crate::godview::{
    Durable, Event, GodView, NodeState, Obligation, Process, Released, Role, Stamped, SyncState,
};
use crate::history::LogEntry;

/// The payload digest the fixtures give an entry.
pub fn digest_of(term: u64, payload: &str) -> String {
    format!("digest-{term}-{payload}")
}

/// A god view under construction. Events written between two [`Run::step`] calls share a step.
pub struct Run {
    events: Vec<Stamped>,
    seq: u64,
}

impl Run {
    pub fn new() -> Self {
        let mut run = Self {
            events: Vec::new(),
            seq: 0,
        };
        for node in 1..=5 {
            run.push(Event::Recovered {
                node,
                log: Vec::new(),
                durable_term: 0,
                durable_vote: None,
            });
        }
        run
    }

    fn push(&mut self, event: Event) -> &mut Self {
        let at = format!("records[{}]", self.events.len());
        self.events.push(Stamped {
            seq: self.seq,
            at,
            event,
        });
        self
    }

    /// Starts the next step.
    pub fn step(&mut self) -> &mut Self {
        self.seq += 1;
        self
    }

    pub fn append(&mut self, node: u64, index: u64, term: u64, payload: &str) -> &mut Self {
        self.push(Event::Append {
            node,
            index,
            entry: LogEntry {
                term,
                payload: payload.to_owned(),
            },
        })
    }

    pub fn truncate(&mut self, node: u64, from: u64) -> &mut Self {
        self.push(Event::Truncate { node, from })
    }

    /// `node` accepts the entry (`index`, `entry_term`, `payload`) while in `acceptance_term`.
    pub fn accept(
        &mut self,
        node: u64,
        index: u64,
        entry_term: u64,
        acceptance_term: u64,
        payload: &str,
    ) -> &mut Self {
        self.push(Event::Accept {
            node,
            index,
            entry_term,
            acceptance_term,
            digest: digest_of(entry_term, payload),
        })
    }

    /// Each of `nodes` appends the entry and accepts it in its own term.
    pub fn replicate(&mut self, nodes: &[u64], index: u64, term: u64, payload: &str) -> &mut Self {
        for node in nodes {
            self.append(*node, index, term, payload)
                .accept(*node, index, term, term, payload);
        }
        self
    }

    pub fn committed(&mut self, index: u64, term: u64, payload: &str) -> &mut Self {
        self.push(Event::Committed {
            index,
            term,
            payload: payload.to_owned(),
        })
    }

    fn node_state(process: Process, role: Role, term: u64, commit: u64, applied: u64) -> NodeState {
        NodeState {
            process,
            role,
            term,
            vote: None,
            commit,
            applied,
            digest: format!("state-{applied}"),
        }
    }

    pub fn state(
        &mut self,
        node: u64,
        role: Role,
        term: u64,
        commit: u64,
        applied: u64,
    ) -> &mut Self {
        let state = Self::node_state(Process::Running, role, term, commit, applied);
        self.state_digest(node, state)
    }

    pub fn voted(&mut self, node: u64, term: u64, vote: u64) -> &mut Self {
        let mut state = Self::node_state(Process::Running, Role::Follower, term, 0, 0);
        state.vote = Some(vote);
        self.state_digest(node, state)
    }

    pub fn lost(&mut self, node: u64) -> &mut Self {
        let state = Self::node_state(Process::Lost, Role::Follower, 0, 0, 0);
        self.state_digest(node, state)
    }

    pub fn state_digest(&mut self, node: u64, state: NodeState) -> &mut Self {
        self.push(Event::State { node, state })
    }

    /// `node` restarts and recovers with this durable term and vote, and an empty log.
    pub fn recover(
        &mut self,
        node: u64,
        durable_term: u64,
        durable_vote: Option<u64>,
    ) -> &mut Self {
        self.push(Event::Recovered {
            node,
            log: Vec::new(),
            durable_term,
            durable_vote,
        })
    }

    /// A sync of node `node`'s incarnation 0, as of its log and hard state now, requested and
    /// then completed in the same step.
    pub fn synced(
        &mut self,
        node: u64,
        id: u64,
        term: u64,
        vote: Option<u64>,
        last: (u64, u64),
    ) -> &mut Self {
        self.sync_record(node, id, SyncState::Requested, term, vote, last);
        self.sync_record(node, id, SyncState::Completed, term, vote, last)
    }

    /// One `sync` record of node `node`'s incarnation 0.
    pub fn sync_record(
        &mut self,
        node: u64,
        id: u64,
        state: SyncState,
        term: u64,
        vote: Option<u64>,
        (last_index, last_term): (u64, u64),
    ) -> &mut Self {
        self.push(Event::Sync {
            node,
            incarnation: 0,
            id,
            state,
            covers: Durable {
                term,
                vote,
                last_index,
                last_term,
            },
        })
    }

    /// A release by node `node`'s incarnation 0.
    pub fn release(
        &mut self,
        node: u64,
        what: Released,
        hard_state: Option<(u64, Option<u64>)>,
        (last_index, last_term): (u64, u64),
    ) -> &mut Self {
        self.push(Event::Release {
            node,
            incarnation: 0,
            what,
            requires: Obligation {
                hard_state,
                last_index,
                last_term,
            },
        })
    }

    pub fn view(&self) -> GodView {
        GodView {
            voters: (1..=5).collect::<BTreeSet<u64>>(),
            events: self.events.clone(),
        }
    }
}
