//! The god-view records of a `keyval.observation/v1` document, in event order (T-5): what K-8
//! reads of each node's log, state and acceptances, parsed into the checkers' own types. Records
//! that say nothing about a node's Raft state are skipped.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::history::{
    LogEntry, array_field, field, log_entry, node_of, opt_u64, str_field, u64_field, u64_set,
};
use crate::report::CheckError;

/// A node's process, as `node_state` records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Process {
    /// Taking steps.
    Running,
    /// Paused: holds its state, takes no steps.
    Paused,
    /// Crashed: the state is the last it had.
    Crashed,
    /// Permanently lost.
    Lost,
}

/// A node's Raft role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Leads its term.
    Leader,
    /// Campaigning.
    Candidate,
    /// Probing before a campaign.
    PreCandidate,
    /// Follows a leader.
    Follower,
}

/// A node's god-view state after a step (`node_state`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeState {
    /// The process.
    pub process: Process,
    /// The role.
    pub role: Role,
    /// The node's current term.
    pub term: u64,
    /// The node's vote in that term, if it has one.
    pub vote: Option<u64>,
    /// The commit index the node believes.
    pub commit: u64,
    /// The applied index.
    pub applied: u64,
    /// The state digest at that applied index, as the document writes it.
    pub digest: String,
}

/// One god-view observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A node incarnation began with this whole log.
    Recovered {
        /// The node.
        node: u64,
        /// Its recovered log, index 1 first.
        log: Vec<LogEntry>,
        /// The durable term the incarnation recovered with.
        durable_term: u64,
        /// The durable vote the incarnation recovered with, in `durable_term`.
        durable_vote: Option<u64>,
    },
    /// An entry was appended one past the end of the node's log.
    Append {
        /// The node.
        node: u64,
        /// The entry's index.
        index: u64,
        /// The entry.
        entry: LogEntry,
    },
    /// The node removed its log suffix from `from` on.
    Truncate {
        /// The node.
        node: u64,
        /// The first removed index.
        from: u64,
    },
    /// A replica accepted an entry.
    Accept {
        /// The node.
        node: u64,
        /// The entry's index.
        index: u64,
        /// The term the entry was appended in.
        entry_term: u64,
        /// The term the replica was in when it accepted.
        acceptance_term: u64,
        /// SHA-256 of the entry's payload bytes, as the document writes it.
        digest: String,
    },
    /// The harness's own certificate computation committed an entry.
    Committed {
        /// The index.
        index: u64,
        /// The entry's term.
        term: u64,
        /// The payload bytes as lowercase hex.
        payload: String,
    },
    /// A node's state after a step that changed it.
    State {
        /// The node.
        node: u64,
        /// The state.
        state: NodeState,
    },
}

/// An event with the scheduler event that produced it and where it is in the document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamped {
    /// The scheduler event (`seq`): records with one `seq` are one step.
    pub seq: u64,
    /// The record's position, such as `records[7]`.
    pub at: String,
    /// What was observed.
    pub event: Event,
}

/// One run's god view: the voters and the node-side observations, in event order.
#[derive(Clone, Debug)]
pub struct GodView {
    /// The configured voters.
    pub voters: BTreeSet<u64>,
    /// The observations that concern a node's Raft state, in record order.
    pub events: Vec<Stamped>,
}

impl GodView {
    /// Parses a document's text.
    ///
    /// # Errors
    ///
    /// [`CheckError::NotJson`] or [`CheckError::Malformed`].
    pub fn parse(text: &str) -> Result<Self, CheckError> {
        let value: Value =
            serde_json::from_str(text).map_err(|error| CheckError::NotJson(error.to_string()))?;
        Self::from_value(&value)
    }

    /// Reads a parsed document.
    ///
    /// # Errors
    ///
    /// [`CheckError::Malformed`] naming the first field that is absent or wrong.
    pub fn from_value(doc: &Value) -> Result<Self, CheckError> {
        let voters = u64_set(field(doc, "", "voters")?, "voters")?;
        let records = field(doc, "", "records")?
            .as_array()
            .ok_or_else(|| CheckError::malformed("records", "not an array"))?;
        let mut events = Vec::new();
        for (position, record) in records.iter().enumerate() {
            let at = format!("records[{position}]");
            if let Some(event) = event_of(record, &at)? {
                events.push(Stamped {
                    seq: u64_field(record, &at, "seq")?,
                    at,
                    event,
                });
            }
        }
        Ok(Self { voters, events })
    }
}

fn event_of(record: &Value, at: &str) -> Result<Option<Event>, CheckError> {
    Ok(Some(match str_field(record, at, "kind")? {
        "recovered" => {
            let mut log = Vec::new();
            for (position, item) in array_field(record, at, "log")?.iter().enumerate() {
                let item_at = format!("{at}.log[{position}]");
                let index = u64_field(item, &item_at, "index")?;
                if index != position as u64 + 1 {
                    return Err(CheckError::malformed(
                        item_at,
                        format!("recovered log index {index} where {} is next", position + 1),
                    ));
                }
                log.push(log_entry(item, &item_at)?);
            }
            let durable = field(record, at, "durable")?;
            let durable_at = format!("{at}.durable");
            Event::Recovered {
                node: node_of(record, at)?,
                log,
                durable_term: u64_field(durable, &durable_at, "term")?,
                durable_vote: opt_u64(durable, &durable_at, "vote")?,
            }
        }
        "append" => Event::Append {
            node: node_of(record, at)?,
            index: u64_field(record, at, "index")?,
            entry: log_entry(record, at)?,
        },
        "truncate" => Event::Truncate {
            node: node_of(record, at)?,
            from: u64_field(record, at, "from_index")?,
        },
        "accept" => {
            let replica = field(record, at, "replica")?;
            Event::Accept {
                node: u64_field(replica, &format!("{at}.replica"), "node")?,
                index: u64_field(record, at, "index")?,
                entry_term: u64_field(record, at, "entry_term")?,
                acceptance_term: u64_field(record, at, "acceptance_term")?,
                digest: str_field(record, at, "payload_digest")?.to_owned(),
            }
        }
        "committed" => Event::Committed {
            index: u64_field(record, at, "index")?,
            term: u64_field(record, at, "term")?,
            payload: str_field(record, at, "payload")?.to_owned(),
        },
        "node_state" => Event::State {
            node: node_of(record, at)?,
            state: NodeState {
                process: match str_field(record, at, "process")? {
                    "running" => Process::Running,
                    "paused" => Process::Paused,
                    "crashed" => Process::Crashed,
                    "lost" => Process::Lost,
                    other => return Err(unknown(at, "process", other)),
                },
                role: match str_field(record, at, "role")? {
                    "leader" => Role::Leader,
                    "candidate" => Role::Candidate,
                    "pre_candidate" => Role::PreCandidate,
                    "follower" => Role::Follower,
                    other => return Err(unknown(at, "role", other)),
                },
                term: u64_field(record, at, "term")?,
                vote: opt_u64(record, at, "vote")?,
                commit: u64_field(record, at, "commit")?,
                applied: u64_field(record, at, "applied")?,
                digest: str_field(record, at, "state_digest")?.to_owned(),
            },
        },
        _ => return Ok(None),
    }))
}

fn unknown(at: &str, name: &str, value: &str) -> CheckError {
    CheckError::malformed(format!("{at}.{name}"), format!("unknown value {value:?}"))
}
