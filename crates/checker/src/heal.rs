//! The heal-side records of a `keyval.observation/v1` document, in record order: what K-7 and
//! K-9 read of how a run finished (PLAN.md §10.2 Heal). Records that say nothing about the heal
//! are skipped.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::history::{array_field, field, node_of, str_field, u64_field, u64_set};
use crate::report::CheckError;
use crate::trace::Resolved;

/// How the manifest says the heal completes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    /// A uniquely identified catch-up marker is written and applied everywhere.
    Marker,
    /// The marker key was pre-created (DM-6 profiles); the marker is an equal-size overwrite.
    PrecreatedMarker,
    /// No marker (DM-7 profiles): a certified drain over the quiet period.
    Drain,
}

/// A node incarnation applied the catch-up marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkerApplied {
    /// The record's position in `records`.
    pub position: u64,
    /// The record's `time`.
    pub time: u64,
    /// The node.
    pub node: u64,
    /// The node's incarnation.
    pub incarnation: u64,
    /// The marker's log index.
    pub index: u64,
    /// The state digest at that application.
    pub digest: String,
}

/// One recoverable node's row of a certified drain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrainNode {
    /// The node.
    pub node: u64,
    /// Its last log index.
    pub last_index: u64,
    /// Its commit index.
    pub commit: u64,
    /// Its applied index.
    pub applied: u64,
    /// Its state digest.
    pub digest: String,
}

/// A certified drain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drain {
    /// The record's position in `records`.
    pub position: u64,
    /// The record's `time`.
    pub time: u64,
    /// The quiet period the drain held over, in ticks.
    pub quiet_ticks: u64,
    /// The rows, one per node the drain covers.
    pub nodes: Vec<DrainNode>,
}

/// How the run resolved one logical write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// The record's position in `records`.
    pub position: u64,
    /// The record's `time`.
    pub time: u64,
    /// The write's request ID.
    pub request_id: String,
    /// How it was settled.
    pub resolved: Resolved,
    /// The attempts the resolution rests on.
    pub evidence: Vec<u64>,
}

/// A response to an attempt: who generated it and where it is recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Responded {
    /// The record's position in `records`.
    pub position: u64,
    /// The node that generated it.
    pub node: u64,
}

/// One run's heal-side observations.
#[derive(Clone, Debug)]
pub struct Heal {
    /// How the heal completes.
    pub completion: Completion,
    /// The tick length in microseconds.
    pub tick_us: u64,
    /// The heal budget in ticks.
    pub heal_budget_ticks: u64,
    /// The drain's quiet period in ticks.
    pub quiet_ticks: u64,
    /// The configured voters.
    pub voters: BTreeSet<u64>,
    /// The permanently lost nodes, as of the last `lost` record.
    pub lost: BTreeSet<u64>,
    /// When the heal phase began, if it did.
    pub heal_at: Option<u64>,
    /// Where and when client activity stopped, if it did: (position, time).
    pub quiescence: Option<(u64, u64)>,
    /// Every marker application, in record order.
    pub markers: Vec<MarkerApplied>,
    /// The certified drain, if one was recorded.
    pub drain: Option<Drain>,
    /// When each index was committed.
    pub committed_at: BTreeMap<u64, u64>,
    /// Each logical write's request ID, with the position of its first invocation.
    pub writes: BTreeMap<String, u64>,
    /// Invocations after quiescence that are not status lookups: (position, attempt).
    pub late_invocations: Vec<(u64, u64)>,
    /// Every resolution, in record order.
    pub resolutions: Vec<Resolution>,
    /// The response to each attempt (the first, if there are more).
    pub responses: BTreeMap<u64, Responded>,
}

impl Heal {
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
        let manifest = field(doc, "", "manifest")?;
        let completion = match str_field(manifest, "manifest", "heal")? {
            "marker" => Completion::Marker,
            "precreated_marker" => Completion::PrecreatedMarker,
            "drain" => Completion::Drain,
            other => {
                return Err(CheckError::malformed(
                    "manifest.heal",
                    format!("unknown heal `{other}`"),
                ));
            }
        };
        let mut heal = Self {
            completion,
            tick_us: u64_field(manifest, "manifest", "tick_us")?,
            heal_budget_ticks: u64_field(manifest, "manifest", "heal_budget_ticks")?,
            quiet_ticks: u64_field(manifest, "manifest", "quiet_ticks")?,
            voters: u64_set(field(doc, "", "voters")?, "voters")?,
            lost: BTreeSet::new(),
            heal_at: None,
            quiescence: None,
            markers: Vec::new(),
            drain: None,
            committed_at: BTreeMap::new(),
            writes: BTreeMap::new(),
            late_invocations: Vec::new(),
            resolutions: Vec::new(),
            responses: BTreeMap::new(),
        };
        for (position, record) in array_field(doc, "", "records")?.iter().enumerate() {
            let position = position as u64;
            heal.record(record, position, &format!("records[{position}]"))?;
        }
        Ok(heal)
    }

    fn record(&mut self, record: &Value, position: u64, at: &str) -> Result<(), CheckError> {
        let time = u64_field(record, at, "time")?;
        match str_field(record, at, "kind")? {
            "phase" => {
                if str_field(record, at, "phase")? == "heal" {
                    self.heal_at = Some(time);
                }
            }
            "lost" => self.lost = u64_set(field(record, at, "nodes")?, &format!("{at}.nodes"))?,
            "quiescence" => self.quiescence = Some((position, time)),
            "committed" => {
                self.committed_at
                    .entry(u64_field(record, at, "index")?)
                    .or_insert(time);
            }
            "marker" => self.markers.push(MarkerApplied {
                position,
                time,
                node: node_of(record, at)?,
                incarnation: u64_field(
                    field(record, at, "at")?,
                    &format!("{at}.at"),
                    "incarnation",
                )?,
                index: u64_field(record, at, "index")?,
                digest: str_field(record, at, "state_digest")?.to_owned(),
            }),
            "drain" => self.drain = Some(drain_of(record, position, time, at)?),
            "invoke" => self.invoke(record, position, at)?,
            "response" => {
                let from = field(record, at, "from")?;
                self.responses
                    .entry(u64_field(record, at, "attempt")?)
                    .or_insert(Responded {
                        position,
                        node: u64_field(from, &format!("{at}.from"), "node")?,
                    });
            }
            "resolution" => self
                .resolutions
                .push(resolution_of(record, position, time, at)?),
            _ => {}
        }
        Ok(())
    }

    fn invoke(&mut self, record: &Value, position: u64, at: &str) -> Result<(), CheckError> {
        let operation = field(record, at, "operation")?;
        let op = str_field(operation, &format!("{at}.operation"), "op")?;
        if ["put", "delete", "cas"].contains(&op)
            && let Some(id) = field(record, at, "request_id")?.as_str()
        {
            self.writes.entry(id.to_owned()).or_insert(position);
        }
        if self.quiescence.is_some() && op != "status" {
            self.late_invocations
                .push((position, u64_field(record, at, "attempt")?));
        }
        Ok(())
    }

    /// The recoverable nodes: the voters less the permanently lost.
    pub fn recoverable(&self) -> impl Iterator<Item = u64> + '_ {
        self.voters.difference(&self.lost).copied()
    }

    /// The permanently lost voters: the nodes K-7 and K-9 exclude and list.
    #[must_use]
    pub fn excluded(&self) -> Vec<u64> {
        self.voters.intersection(&self.lost).copied().collect()
    }

    /// The `time` by which the heal must have completed, if the run has a heal phase.
    #[must_use]
    pub fn deadline(&self) -> Option<u64> {
        self.heal_at
            .map(|start| start.saturating_add(self.heal_budget_ticks.saturating_mul(self.tick_us)))
    }
}

fn drain_of(record: &Value, position: u64, time: u64, at: &str) -> Result<Drain, CheckError> {
    let mut nodes = Vec::new();
    for (row, item) in array_field(record, at, "nodes")?.iter().enumerate() {
        let row_at = format!("{at}.nodes[{row}]");
        nodes.push(DrainNode {
            node: u64_field(item, &row_at, "node")?,
            last_index: u64_field(item, &row_at, "last_index")?,
            commit: u64_field(item, &row_at, "commit")?,
            applied: u64_field(item, &row_at, "applied")?,
            digest: str_field(item, &row_at, "state_digest")?.to_owned(),
        });
    }
    Ok(Drain {
        position,
        time,
        quiet_ticks: u64_field(record, at, "quiet_ticks")?,
        nodes,
    })
}

fn resolution_of(
    record: &Value,
    position: u64,
    time: u64,
    at: &str,
) -> Result<Resolution, CheckError> {
    let mut evidence = Vec::new();
    for (item, value) in array_field(record, at, "evidence")?.iter().enumerate() {
        evidence.push(value.as_u64().ok_or_else(|| {
            CheckError::malformed(format!("{at}.evidence[{item}]"), "not an unsigned integer")
        })?);
    }
    Ok(Resolution {
        position,
        time,
        request_id: str_field(record, at, "request_id")?.to_owned(),
        resolved: match str_field(record, at, "resolved")? {
            "executed" => Resolved::Executed,
            "not_executed" => Resolved::NotExecuted,
            "refused" => Resolved::Refused,
            "unresolved" => Resolved::Unresolved,
            other => {
                return Err(CheckError::malformed(
                    format!("{at}.resolved"),
                    format!("unknown resolution `{other}`"),
                ));
            }
        },
        evidence,
    })
}
