//! What K-13 reads of a `keyval.observation/v1` document (PLAN.md §10.16): the scenario's
//! selected faults, the fault, phase and response records, and the run's completeness fields.
//! Records that say nothing about non-vacuity are skipped.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::heal::{Completion, Heal};
use crate::history::{array_field, bool_field, field, str_field, u64_field, u64_set};
use crate::report::CheckError;
use crate::trace::Resolved;

/// One run's non-vacuity observations.
#[derive(Clone, Debug)]
pub struct Vacuity {
    /// How the heal completes.
    pub completion: Completion,
    /// The configured voters.
    pub voters: BTreeSet<u64>,
    /// The fault types the manifest selected.
    pub selected: BTreeSet<String>,
    /// The fault types a `fault` record started.
    pub injected: BTreeSet<String>,
    /// The position of the first `fault` start record, if any.
    pub first_fault: Option<u64>,
    /// The position of the `baseline` phase record, if any.
    pub baseline_at: Option<u64>,
    /// The position of the `chaos` phase record, if any.
    pub chaos_at: Option<u64>,
    /// The position of the `heal` phase record, if any.
    pub heal_at: Option<u64>,
    /// The position of each successful write response after the `baseline` phase began (setup
    /// writes do not count).
    pub write_successes: Vec<u64>,
    /// Per node that served a successful read: how many reads.
    pub read_servers: BTreeMap<u64, u64>,
    /// The position of each `marker` record.
    pub markers: Vec<u64>,
    /// The position of the `drain` record, if any.
    pub drain: Option<u64>,
    /// Each logical write's request ID with how its resolution record settled it; `None` if it
    /// has no resolution record.
    pub writes: BTreeMap<String, Option<Resolved>>,
    /// Number of records.
    pub records: u64,
    /// Whether the document says the run is complete.
    pub complete: bool,
    /// The names of the required observations that overflowed.
    pub overflowed: Vec<String>,
    /// Whether the event budget was exhausted.
    pub budget_exhausted: bool,
    /// The scenario's status (`valid` or `invalid`) and the rule it violated, if it names one.
    pub scenario: (String, Option<String>),
}

impl Vacuity {
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
        let heal = Heal::from_value(doc)?;
        let manifest = field(doc, "", "manifest")?;
        let mut selected = BTreeSet::new();
        for (item, entry) in array_field(manifest, "manifest", "faults")?
            .iter()
            .enumerate()
        {
            selected
                .insert(str_field(entry, &format!("manifest.faults[{item}]"), "fault")?.to_owned());
        }
        let scenario = field(doc, "", "scenario")?;
        let overflowed = array_field(doc, "", "overflowed")?
            .iter()
            .map(|name| {
                name.as_str()
                    .map_or_else(|| name.to_string(), str::to_owned)
            })
            .collect();
        let mut view = Self {
            completion: heal.completion,
            voters: u64_set(field(doc, "", "voters")?, "voters")?,
            selected,
            injected: BTreeSet::new(),
            first_fault: None,
            baseline_at: None,
            chaos_at: None,
            heal_at: None,
            write_successes: Vec::new(),
            read_servers: BTreeMap::new(),
            markers: Vec::new(),
            drain: None,
            writes: heal.writes.keys().map(|id| (id.clone(), None)).collect(),
            records: 0,
            complete: bool_field(doc, "", "complete")?,
            overflowed,
            budget_exhausted: bool_field(
                field(doc, "", "event_budget")?,
                "event_budget",
                "exhausted",
            )?,
            scenario: (
                str_field(scenario, "scenario", "status")?.to_owned(),
                field(scenario, "scenario", "violated")?
                    .as_str()
                    .map(str::to_owned),
            ),
        };
        for resolution in &heal.resolutions {
            if let Some(slot) = view.writes.get_mut(&resolution.request_id) {
                *slot = Some(resolution.resolved);
            }
        }
        for (position, record) in array_field(doc, "", "records")?.iter().enumerate() {
            view.record(record, position as u64, &format!("records[{position}]"))?;
            view.records += 1;
        }
        Ok(view)
    }

    fn record(&mut self, record: &Value, position: u64, at: &str) -> Result<(), CheckError> {
        match str_field(record, at, "kind")? {
            "phase" => match str_field(record, at, "phase")? {
                "baseline" => self.baseline_at = Some(position),
                "chaos" => self.chaos_at = Some(position),
                "heal" => self.heal_at = Some(position),
                _ => {}
            },
            "fault" => {
                if str_field(record, at, "action")? == "start" {
                    self.injected
                        .insert(str_field(record, at, "fault")?.to_owned());
                    self.first_fault.get_or_insert(position);
                }
            }
            "response" => {
                let op = str_field(record, at, "op")?;
                let outcome = field(record, at, "outcome")?;
                let code = str_field(outcome, &format!("{at}.outcome"), "code")?;
                if ["put", "delete", "cas"].contains(&op)
                    && code == "ok"
                    && self.baseline_at.is_some()
                {
                    self.write_successes.push(position);
                }
                if ["get", "scan"].contains(&op)
                    && ["ok", "not_found"].contains(&code)
                    && let Some(served) = outcome.get("served_by").filter(|v| !v.is_null())
                {
                    let node = u64_field(served, &format!("{at}.outcome.served_by"), "node_id")?;
                    *self.read_servers.entry(node).or_insert(0) += 1;
                }
            }
            "marker" => self.markers.push(position),
            "drain" => self.drain = Some(position),
            _ => {}
        }
        Ok(())
    }
}
