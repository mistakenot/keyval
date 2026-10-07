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

/// Where a sync stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncState {
    /// Asked for: covers the log as it was now.
    Requested,
    /// Done: what it covered is on disk.
    Completed,
    /// Completed for a request of an earlier incarnation: covers nothing (B-17).
    Stale,
}

/// What a sync covers, or a node recovers with: the hard state and the log's end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Durable {
    /// The term.
    pub term: u64,
    /// The vote in that term.
    pub vote: Option<u64>,
    /// The last log index.
    pub last_index: u64,
    /// The term of the entry at `last_index`, 0 for an empty log.
    pub last_term: u64,
}

/// What a release has to find already durable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Obligation {
    /// The term and vote that must be durable (term, vote), if the release depends on them.
    pub hard_state: Option<(u64, Option<u64>)>,
    /// The log must be durable up to this index.
    pub last_index: u64,
    /// The term of the entry at `last_index`.
    pub last_term: u64,
}

/// What a node released.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Released {
    /// A vote grant.
    VoteGrant,
    /// An append acknowledgement.
    AppendAck,
    /// A client success for an attempt.
    ClientSuccess(u64),
    /// A published read for an attempt.
    PublishedRead(u64),
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
    /// A sync obligation changed state on a node incarnation.
    Sync {
        /// The node.
        node: u64,
        /// The node's incarnation.
        incarnation: u64,
        /// The sync's ID, unique in the run.
        id: u64,
        /// The state the sync reached.
        state: SyncState,
        /// The durable state the sync covers.
        covers: Durable,
    },
    /// A node released a vote, an acknowledgement, a success or a read.
    Release {
        /// The node.
        node: u64,
        /// The node's incarnation.
        incarnation: u64,
        /// What was released.
        what: Released,
        /// What had to be durable first.
        requires: Obligation,
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
        "sync" => sync_event(record, at)?,
        "release" => release_event(record, at)?,
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

fn sync_event(record: &Value, at: &str) -> Result<Event, CheckError> {
    let at_node = field(record, at, "at")?;
    let at_node_path = format!("{at}.at");
    Ok(Event::Sync {
        node: u64_field(at_node, &at_node_path, "node")?,
        incarnation: u64_field(at_node, &at_node_path, "incarnation")?,
        id: u64_field(record, at, "sync")?,
        state: match str_field(record, at, "state")? {
            "requested" => SyncState::Requested,
            "completed" => SyncState::Completed,
            "stale" => SyncState::Stale,
            other => return Err(unknown(at, "state", other)),
        },
        covers: durable_of(field(record, at, "covers")?, &format!("{at}.covers"))?,
    })
}

fn release_event(record: &Value, at: &str) -> Result<Event, CheckError> {
    let at_node = field(record, at, "at")?;
    let at_node_path = format!("{at}.at");
    let attempt = opt_u64(record, at, "attempt")?;
    let requires = field(record, at, "requires")?;
    let requires_at = format!("{at}.requires");
    let hard_state = field(requires, &requires_at, "hard_state")?;
    let hard_state = if hard_state.is_null() {
        None
    } else {
        let hard_at = format!("{requires_at}.hard_state");
        Some((
            u64_field(hard_state, &hard_at, "term")?,
            opt_u64(hard_state, &hard_at, "vote")?,
        ))
    };
    Ok(Event::Release {
        node: u64_field(at_node, &at_node_path, "node")?,
        incarnation: u64_field(at_node, &at_node_path, "incarnation")?,
        what: released_of(str_field(record, at, "what")?, attempt, at)?,
        requires: Obligation {
            hard_state,
            last_index: u64_field(requires, &requires_at, "last_index")?,
            last_term: u64_field(requires, &requires_at, "last_term")?,
        },
    })
}

fn durable_of(value: &Value, at: &str) -> Result<Durable, CheckError> {
    Ok(Durable {
        term: u64_field(value, at, "term")?,
        vote: opt_u64(value, at, "vote")?,
        last_index: u64_field(value, at, "last_index")?,
        last_term: u64_field(value, at, "last_term")?,
    })
}

fn released_of(what: &str, attempt: Option<u64>, at: &str) -> Result<Released, CheckError> {
    let attempt_of = || {
        attempt.ok_or_else(|| {
            CheckError::malformed(format!("{at}.attempt"), format!("{what} names no attempt"))
        })
    };
    Ok(match what {
        "vote_grant" => Released::VoteGrant,
        "append_ack" => Released::AppendAck,
        "client_success" => Released::ClientSuccess(attempt_of()?),
        "published_read" => Released::PublishedRead(attempt_of()?),
        other => return Err(unknown(at, "what", other)),
    })
}

fn unknown(at: &str, name: &str, value: &str) -> CheckError {
    CheckError::malformed(format!("{at}.{name}"), format!("unknown value {value:?}"))
}
