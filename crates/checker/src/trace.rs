//! The client-side records of a `keyval.observation/v1` document, in record order: what K-12
//! reads of each attempt's life, answers and resolutions, parsed into the checkers' own types.
//! Records that say nothing about an attempt or a logical write are skipped.

use serde_json::Value;

use crate::history::{array_field, field, str_field, u64_field};
use crate::report::CheckError;

/// Where an attempt stands on the node that took it (`attempt` record).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptState {
    /// Passed admission.
    Admitted,
    /// Answered from a stored result without a proposal.
    ShortCircuit,
    /// Waiting on a same-ID, same-digest attempt: its command is in flight, so it counts as
    /// submitted.
    Attached,
    /// Its command was proposed to Raft.
    Submitted,
    /// A synchronous `ProposalDropped`: nothing was appended, so it was never submitted.
    Dropped,
    /// Reached a terminal decision.
    Terminal,
}

/// What a response said, reduced to what K-12 judges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// A write's outcome (`put`, `delete` or `cas`).
    Write {
        /// The outcome code, such as `unknown_outcome`.
        code: String,
        /// The outcome's `attempt_state` (`submitted` or `not_submitted`), if it has one.
        attempt_state: Option<String>,
    },
    /// A `status` answer.
    Status {
        /// Whether the node knew a stored result (`state: known`).
        known: bool,
        /// Whether the answer carried a result.
        has_result: bool,
    },
    /// Any other answer: a read, or a status error.
    Other,
}

/// How a logical write was resolved (`resolution`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// It executed.
    Executed,
    /// It never executed, proven by a drain.
    NotExecuted,
    /// It was refused before any submission.
    Refused,
    /// The run could not tell.
    Unresolved,
}

/// One client-side observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// A client invoked an attempt.
    Invoke {
        /// The attempt.
        attempt: u64,
        /// The logical write it belongs to, or the write a `status` lookup asks about.
        request_id: Option<String>,
        /// Whether the operation is a write (`put`, `delete` or `cas`).
        write: bool,
    },
    /// An attempt moved to a state.
    Attempt {
        /// The attempt.
        attempt: u64,
        /// The state it reached.
        state: AttemptState,
    },
    /// A node answered an attempt.
    Response {
        /// The attempt.
        attempt: u64,
        /// The answer.
        answer: Answer,
    },
    /// The run settled a logical write.
    Resolution {
        /// The write's request ID.
        request_id: String,
        /// How it was settled.
        resolved: Resolved,
        /// The attempts the resolution rests on.
        evidence: Vec<u64>,
    },
}

/// An item with its position in the document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Located {
    /// The record's position, such as `records[7]`.
    pub at: String,
    /// What was observed.
    pub item: Item,
}

/// One run's client-side observations, in record order.
#[derive(Clone, Debug)]
pub struct Trace {
    /// The observations, in record order.
    pub items: Vec<Located>,
}

impl Trace {
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
        let records = field(doc, "", "records")?
            .as_array()
            .ok_or_else(|| CheckError::malformed("records", "not an array"))?;
        let mut items = Vec::new();
        for (position, record) in records.iter().enumerate() {
            let at = format!("records[{position}]");
            if let Some(item) = item_of(record, &at)? {
                items.push(Located { at, item });
            }
        }
        Ok(Self { items })
    }
}

fn item_of(record: &Value, at: &str) -> Result<Option<Item>, CheckError> {
    Ok(Some(match str_field(record, at, "kind")? {
        "invoke" => {
            let operation = field(record, at, "operation")?;
            let operation_at = format!("{at}.operation");
            let write =
                ["put", "delete", "cas"].contains(&str_field(operation, &operation_at, "op")?);
            // A write carries its request ID at the top; a status lookup names the write in
            // its operation.
            let request_id = field(record, at, "request_id")?
                .as_str()
                .or_else(|| operation.get("request_id").and_then(Value::as_str))
                .map(str::to_owned);
            Item::Invoke {
                attempt: u64_field(record, at, "attempt")?,
                request_id,
                write,
            }
        }
        "attempt" => Item::Attempt {
            attempt: u64_field(record, at, "attempt")?,
            state: match str_field(record, at, "state")? {
                "admitted" => AttemptState::Admitted,
                "short_circuit" => AttemptState::ShortCircuit,
                "attached" => AttemptState::Attached,
                "submitted" => AttemptState::Submitted,
                "dropped" => AttemptState::Dropped,
                "terminal" => AttemptState::Terminal,
                other => return Err(unknown(at, "state", other)),
            },
        },
        "response" => Item::Response {
            attempt: u64_field(record, at, "attempt")?,
            answer: answer_of(record, at)?,
        },
        "resolution" => {
            let mut evidence = Vec::new();
            for (position, item) in array_field(record, at, "evidence")?.iter().enumerate() {
                evidence.push(item.as_u64().ok_or_else(|| {
                    CheckError::malformed(
                        format!("{at}.evidence[{position}]"),
                        "not an unsigned integer",
                    )
                })?);
            }
            Item::Resolution {
                request_id: str_field(record, at, "request_id")?.to_owned(),
                resolved: match str_field(record, at, "resolved")? {
                    "executed" => Resolved::Executed,
                    "not_executed" => Resolved::NotExecuted,
                    "refused" => Resolved::Refused,
                    "unresolved" => Resolved::Unresolved,
                    other => return Err(unknown(at, "resolved", other)),
                },
                evidence,
            }
        }
        _ => return Ok(None),
    }))
}

fn answer_of(record: &Value, at: &str) -> Result<Answer, CheckError> {
    let outcome = field(record, at, "outcome")?;
    let outcome_at = format!("{at}.outcome");
    Ok(match str_field(record, at, "op")? {
        "put" | "delete" | "cas" => Answer::Write {
            code: str_field(outcome, &outcome_at, "code")?.to_owned(),
            attempt_state: match field(outcome, &outcome_at, "attempt_state")?.as_str() {
                Some(state) => Some(state.to_owned()),
                None if field(outcome, &outcome_at, "attempt_state")?.is_null() => None,
                None => {
                    return Err(CheckError::malformed(
                        format!("{outcome_at}.attempt_state"),
                        "not a string or null",
                    ));
                }
            },
        },
        "status" if str_field(outcome, &outcome_at, "code")? == "ok" => {
            let known = match str_field(outcome, &outcome_at, "state")? {
                "known" => true,
                "unknown" => false,
                other => return Err(unknown(&outcome_at, "state", other)),
            };
            Answer::Status {
                known,
                has_result: !field(outcome, &outcome_at, "result")?.is_null(),
            }
        }
        _ => Answer::Other,
    })
}

fn unknown(at: &str, name: &str, value: &str) -> CheckError {
    CheckError::malformed(format!("{at}.{name}"), format!("unknown value {value:?}"))
}
