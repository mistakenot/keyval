//! The checkers' view of the reads in a `keyval.observation/v1` document (T-12): every GET,
//! SCAN page and status lookup a node answered with `served_by`, the god-view samples of the
//! serving nodes (`node_state`) and the committed entries, each with its position in
//! `records`, which is event order (`seq` and `time` do not refine it).
//!
//! K-5 and K-6 judge this view. A read refused (`busy`, `recovering`, ...) carries no
//! `served_by` and no prefix, so it is no observation of either.

use std::collections::BTreeMap;

use model::{Entry, Limits, RequestId, TerminalResult};
use serde_json::Value;

use crate::history::{
    Committed, array_field, bool_field, claim_of, command_of, field, limits, node_of, opt_u64,
    str_field, u64_field,
};
use crate::report::CheckError;

/// What a read asked for, as its `invoke` record says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Query {
    /// GET of one key.
    Get {
        /// The key.
        key: String,
    },
    /// One SCAN page request.
    Scan {
        /// The inclusive lower bound.
        start: Option<String>,
        /// The exclusive upper bound.
        end: Option<String>,
        /// The exclusive continuation key.
        after: Option<String>,
        /// The page size asked for; `None` is the default page.
        limit: Option<u64>,
    },
    /// Status lookup of one write.
    Status {
        /// The write's request ID.
        request_id: RequestId,
    },
}

/// One item of a SCAN page, as the response reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanItem {
    /// The key.
    pub key: String,
    /// The value.
    pub value: String,
    /// The version.
    pub version: u64,
}

/// What a node answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// A GET: the value and version, or `None` for `not_found`.
    Get(Option<(String, u64)>),
    /// A SCAN page.
    Scan {
        /// The items, in the order reported.
        items: Vec<ScanItem>,
        /// Whether the page says another key lies beyond it.
        has_more: bool,
        /// The continuation key the page reports.
        next_after: Option<String>,
    },
    /// A status lookup: the write's terminal result if the node knew it, else `None`.
    Status(Option<TerminalResult>),
}

/// A read a node answered with `served_by`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The `response` record's position in `records`.
    pub position: u64,
    /// The attempt answered.
    pub attempt: u64,
    /// The node incarnation that generated the response.
    pub node: u64,
    /// That node's incarnation.
    pub incarnation: u64,
    /// The response's generation order at that incarnation.
    pub generation: u64,
    /// The node `served_by` names.
    pub served_node: u64,
    /// The applied prefix `served_by` reports.
    pub applied_index: u64,
    /// What was asked.
    pub query: Query,
    /// What was answered.
    pub reply: Reply,
}

/// A node's god-view state after a step (`node_state`), as K-5 needs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// The record's position in `records`.
    pub position: u64,
    /// The commit index the node believed.
    pub commit: u64,
    /// The applied index.
    pub applied: u64,
}

/// One run's reads and what K-5 judges them against.
#[derive(Clone, Debug)]
pub struct Reads {
    /// The configured limits the reference model takes.
    pub limits: Limits,
    /// The committed entries, in record order.
    pub committed: Vec<Committed>,
    /// The position of each `committed` record, parallel to `committed`.
    pub committed_at: Vec<u64>,
    /// Each node's `node_state` samples, in record order.
    pub samples: BTreeMap<u64, Vec<Sample>>,
    /// Every read answered with `served_by`, in record order.
    pub answers: Vec<Answer>,
}

impl Reads {
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
        let mut reads = Self {
            limits: limits(field(doc, "", "manifest")?)?,
            committed: Vec::new(),
            committed_at: Vec::new(),
            samples: BTreeMap::new(),
            answers: Vec::new(),
        };
        let records = array_field(doc, "", "records")?;
        let mut invoked: BTreeMap<u64, &Value> = BTreeMap::new();
        for (position, record) in records.iter().enumerate() {
            let position = position as u64;
            let at = format!("records[{position}]");
            match str_field(record, &at, "kind")? {
                "invoke" => {
                    invoked.insert(
                        u64_field(record, &at, "attempt")?,
                        field(record, &at, "operation")?,
                    );
                }
                "response" => reads.response(record, position, &at, &invoked)?,
                "committed" => reads.committed_record(record, position, &at)?,
                "node_state" => {
                    let sample = Sample {
                        position,
                        commit: u64_field(record, &at, "commit")?,
                        applied: u64_field(record, &at, "applied")?,
                    };
                    reads
                        .samples
                        .entry(node_of(record, &at)?)
                        .or_default()
                        .push(sample);
                }
                _ => {}
            }
        }
        Ok(reads)
    }

    fn committed_record(
        &mut self,
        record: &Value,
        position: u64,
        at: &str,
    ) -> Result<(), CheckError> {
        let index = model::Index::new(u64_field(record, at, "index")?)
            .map_err(|error| CheckError::malformed(format!("{at}.index"), error.to_string()))?;
        let command = field(record, at, "command")?;
        let entry = if command.is_null() {
            Entry::Empty
        } else {
            Entry::Command(command_of(command, &format!("{at}.command"))?)
        };
        self.committed.push(Committed {
            index,
            payload: str_field(record, at, "payload")?.to_owned(),
            entry,
        });
        self.committed_at.push(position);
        Ok(())
    }

    fn response(
        &mut self,
        record: &Value,
        position: u64,
        at: &str,
        invoked: &BTreeMap<u64, &Value>,
    ) -> Result<(), CheckError> {
        let op = str_field(record, at, "op")?;
        if !["get", "scan", "status"].contains(&op) {
            return Ok(());
        }
        let outcome = field(record, at, "outcome")?;
        let outcome_at = format!("{at}.outcome");
        let served_by = field(outcome, &outcome_at, "served_by")?;
        if served_by.is_null() {
            return Ok(());
        }
        let served_at = format!("{outcome_at}.served_by");
        let attempt = u64_field(record, at, "attempt")?;
        let operation = invoked.get(&attempt).copied().ok_or_else(|| {
            CheckError::malformed(
                format!("{at}.attempt"),
                format!("response to attempt {attempt}, which was never invoked"),
            )
        })?;
        let operation_at = format!("records[invoke {attempt}].operation");
        let from = field(record, at, "from")?;
        let from_at = format!("{at}.from");
        let (query, reply) = match op {
            "get" => (
                Query::Get {
                    key: str_field(operation, &operation_at, "key")?.to_owned(),
                },
                get_reply(outcome, &outcome_at)?,
            ),
            "scan" => (
                scan_query(operation, &operation_at)?,
                scan_reply(outcome, &outcome_at)?,
            ),
            _ => {
                let request_id = RequestId::parse(str_field(
                    operation,
                    &operation_at,
                    "request_id",
                )?)
                .map_err(|error| {
                    CheckError::malformed(format!("{operation_at}.request_id"), error.to_string())
                })?;
                (
                    Query::Status { request_id },
                    status_reply(outcome, &outcome_at, attempt)?,
                )
            }
        };
        self.answers.push(Answer {
            position,
            attempt,
            node: u64_field(from, &from_at, "node")?,
            incarnation: u64_field(from, &from_at, "incarnation")?,
            generation: u64_field(record, at, "generation")?,
            served_node: u64_field(served_by, &served_at, "node_id")?,
            applied_index: u64_field(served_by, &served_at, "applied_index")?,
            query,
            reply,
        });
        Ok(())
    }
}

fn get_reply(outcome: &Value, at: &str) -> Result<Reply, CheckError> {
    match str_field(outcome, at, "code")? {
        "ok" => Ok(Reply::Get(Some((
            str_field(outcome, at, "value")?.to_owned(),
            u64_field(outcome, at, "version")?,
        )))),
        "not_found" => Ok(Reply::Get(None)),
        other => Err(CheckError::malformed(
            format!("{at}.code"),
            format!("a GET answered with served_by has code {other:?}"),
        )),
    }
}

fn optional_key(object: &Value, at: &str, name: &str) -> Result<Option<String>, CheckError> {
    let value = field(object, at, name)?;
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .map(|text| Some(text.to_owned()))
        .ok_or_else(|| CheckError::malformed(format!("{at}.{name}"), "not a string or null"))
}

fn scan_query(operation: &Value, at: &str) -> Result<Query, CheckError> {
    Ok(Query::Scan {
        start: optional_key(operation, at, "start")?,
        end: optional_key(operation, at, "end")?,
        after: optional_key(operation, at, "after")?,
        limit: opt_u64(operation, at, "limit")?,
    })
}

fn scan_reply(outcome: &Value, at: &str) -> Result<Reply, CheckError> {
    if str_field(outcome, at, "code")? != "ok" {
        return Err(CheckError::malformed(
            format!("{at}.code"),
            "a SCAN answered with served_by is not ok",
        ));
    }
    let mut items = Vec::new();
    for (position, item) in array_field(outcome, at, "items")?.iter().enumerate() {
        let item_at = format!("{at}.items[{position}]");
        items.push(ScanItem {
            key: str_field(item, &item_at, "key")?.to_owned(),
            value: str_field(item, &item_at, "value")?.to_owned(),
            version: u64_field(item, &item_at, "version")?,
        });
    }
    Ok(Reply::Scan {
        items,
        has_more: bool_field(outcome, at, "has_more")?,
        next_after: optional_key(outcome, at, "next_after")?,
    })
}

fn status_reply(outcome: &Value, at: &str, attempt: u64) -> Result<Reply, CheckError> {
    if str_field(outcome, at, "code")? != "ok" {
        return Err(CheckError::malformed(
            format!("{at}.code"),
            "a status lookup answered with served_by is not ok",
        ));
    }
    match str_field(outcome, at, "state")? {
        "unknown" => Ok(Reply::Status(None)),
        "known" => {
            let result_at = format!("{at}.result");
            let claim = claim_of(field(outcome, at, "result")?, &result_at, attempt, true)?
                .ok_or_else(|| {
                    CheckError::malformed(result_at, "a known status holds no terminal result")
                })?;
            Ok(Reply::Status(Some(claim.result)))
        }
        other => Err(CheckError::malformed(
            format!("{at}.state"),
            format!("unknown status state {other:?}"),
        )),
    }
}
