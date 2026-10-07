//! The checkers' own view of a `keyval.observation/v1` document (T-12): only the records K-1
//! and K-2 read, parsed into the reference model's types, with every field checked.

use std::collections::{BTreeMap, BTreeSet};

use model::{
    Command, DataError, Decision, Entry, Expected, Index, Key, Limits, LiveLimit, Op, RequestId,
    TerminalResult, Value as Data, Version,
};
use serde_json::Value;

use crate::report::CheckError;

/// A committed entry (`committed` record): its index, exact payload bytes (lowercase hex) and
/// the decoded command, if it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed {
    /// The log index.
    pub index: Index,
    /// The entry's payload bytes as the document writes them: lowercase hex, empty for a no-op.
    pub payload: String,
    /// The decoded entry.
    pub entry: Entry,
}

/// One entry of a node's log, as the document records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    /// The term the entry was appended in.
    pub term: u64,
    /// The payload bytes as lowercase hex.
    pub payload: String,
}

/// A terminal execution response: what a node told a client one request executed as. A write's
/// `ok`, `version_mismatch` or `capacity_exceeded`, or the same inside a known `status` answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Claim {
    /// The attempt the response answers.
    pub attempt: u64,
    /// Whether the response says it is a replay (`replayed: true`).
    pub replayed: bool,
    /// The request ID, index and decision it reports.
    pub result: TerminalResult,
    /// Whether the response was a `status` answer rather than a write's own response.
    pub via_status: bool,
}

/// What an `invoke` record says about an attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invoked {
    /// Whether the attempt is a misuse attempt (a conflicting reuse of a request ID, §10.3).
    pub misuse: bool,
}

/// One run's observations, as K-1 and K-2 need them.
#[derive(Clone, Debug)]
pub struct History {
    /// The configured limits the reference model takes.
    pub limits: Limits,
    /// The configured voters.
    pub voters: BTreeSet<u64>,
    /// The committed entries, in index order.
    pub committed: Vec<Committed>,
    /// Every invoked attempt.
    pub invoked: BTreeMap<u64, Invoked>,
    /// Every terminal execution response, in record order.
    pub claims: Vec<Claim>,
    /// The attempts that had at least one response copy delivered to their client.
    pub delivered: BTreeSet<u64>,
    /// Each node's log at the end of the run, rebuilt from `recovered`, `append` and `truncate`.
    pub final_logs: BTreeMap<u64, BTreeMap<u64, LogEntry>>,
    /// The permanently lost nodes.
    pub lost: BTreeSet<u64>,
}

impl History {
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
        let limits = limits(field(doc, "", "manifest")?)?;
        let mut history = Self {
            limits,
            voters: u64_set(field(doc, "", "voters")?, "voters")?,
            committed: Vec::new(),
            invoked: BTreeMap::new(),
            claims: Vec::new(),
            delivered: BTreeSet::new(),
            final_logs: BTreeMap::new(),
            lost: BTreeSet::new(),
        };
        let records = field(doc, "", "records")?
            .as_array()
            .ok_or_else(|| CheckError::malformed("records", "not an array"))?;
        for (position, record) in records.iter().enumerate() {
            history.record(record, &format!("records[{position}]"))?;
        }
        Ok(history)
    }

    fn record(&mut self, record: &Value, at: &str) -> Result<(), CheckError> {
        match str_field(record, at, "kind")? {
            "invoke" => {
                let attempt = u64_field(record, at, "attempt")?;
                let misuse = bool_field(record, at, "misuse")?;
                self.invoked.insert(attempt, Invoked { misuse });
            }
            "response" => self.response(record, at)?,
            "delivery" => {
                if bool_field(record, at, "delivered")? {
                    self.delivered.insert(u64_field(record, at, "attempt")?);
                }
            }
            "committed" => self.committed_record(record, at)?,
            "recovered" => {
                let node = node_of(record, at)?;
                let mut log = BTreeMap::new();
                for (position, item) in array_field(record, at, "log")?.iter().enumerate() {
                    let at = format!("{at}.log[{position}]");
                    log.insert(u64_field(item, &at, "index")?, log_entry(item, &at)?);
                }
                self.final_logs.insert(node, log);
            }
            "append" => {
                let node = node_of(record, at)?;
                let index = u64_field(record, at, "index")?;
                self.final_logs
                    .entry(node)
                    .or_default()
                    .insert(index, log_entry(record, at)?);
            }
            "truncate" => {
                let node = node_of(record, at)?;
                let from = u64_field(record, at, "from_index")?;
                if let Some(log) = self.final_logs.get_mut(&node) {
                    log.retain(|index, _| *index < from);
                }
            }
            "lost" => self.lost = u64_set(field(record, at, "nodes")?, &format!("{at}.nodes"))?,
            _ => {}
        }
        Ok(())
    }

    fn committed_record(&mut self, record: &Value, at: &str) -> Result<(), CheckError> {
        let index = Index::new(u64_field(record, at, "index")?)
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
        Ok(())
    }

    fn response(&mut self, record: &Value, at: &str) -> Result<(), CheckError> {
        let attempt = u64_field(record, at, "attempt")?;
        let outcome = field(record, at, "outcome")?;
        let at = format!("{at}.outcome");
        let claim = match str_field(record, &at, "op")? {
            "put" | "delete" | "cas" => claim_of(outcome, &at, attempt, false)?,
            "status" => {
                if str_field(outcome, &at, "code")? == "ok"
                    && str_field(outcome, &at, "state")? == "known"
                {
                    claim_of(
                        field(outcome, &at, "result")?,
                        &format!("{at}.result"),
                        attempt,
                        true,
                    )?
                } else {
                    None
                }
            }
            _ => None,
        };
        self.claims.extend(claim);
        Ok(())
    }

    /// Every request's first execution, from the reference model's replay of the committed
    /// entries in index order (§10.14): never from a node's claim.
    ///
    /// # Errors
    ///
    /// [`CheckError::Replay`] if an entry cannot be applied: indices are not 1, 2, 3, … or a
    /// command breaks the configured limits.
    pub fn first_executions(&self) -> Result<BTreeMap<RequestId, TerminalResult>, CheckError> {
        let mut model = model::Model::new(self.limits);
        let mut first = BTreeMap::new();
        for committed in &self.committed {
            let applied = model
                .apply(committed.index, &committed.entry)
                .map_err(|error| CheckError::Replay {
                    index: committed.index.get(),
                    reason: error.to_string(),
                })?;
            if let model::Applied::Executed(result) = applied {
                first.insert(result.request_id, result);
            }
        }
        Ok(first)
    }

    /// The recoverable nodes: the voters less the permanently lost.
    pub fn recoverable(&self) -> impl Iterator<Item = u64> + '_ {
        self.voters.difference(&self.lost).copied()
    }
}

/// A terminal execution outcome as a claim, or `None` for any other answer.
fn claim_of(
    outcome: &Value,
    at: &str,
    attempt: u64,
    via_status: bool,
) -> Result<Option<Claim>, CheckError> {
    let decision = match str_field(outcome, at, "code")? {
        "ok" => {
            if let Some(version) = opt_u64(outcome, at, "version")? {
                Decision::Set {
                    version: Version::new(version).map_err(|error| bad(at, "version", &error))?,
                }
            } else {
                Decision::Deleted {
                    existed: opt_bool(outcome, at, "existed")?.ok_or_else(|| {
                        CheckError::malformed(at, "ok with neither version nor existed")
                    })?,
                }
            }
        }
        "version_mismatch" => Decision::VersionMismatch {
            current: opt_u64(outcome, at, "current_version")?
                .map(|v| Version::new(v).map_err(|error| bad(at, "current_version", &error)))
                .transpose()?,
        },
        "capacity_exceeded" => Decision::CapacityExceeded {
            limit: match str_field(outcome, at, "limit")? {
                "max_live_keys" => LiveLimit::MaxLiveKeys,
                "max_live_bytes" => LiveLimit::MaxLiveBytes,
                other => {
                    return Err(CheckError::malformed(
                        format!("{at}.limit"),
                        format!("unknown live limit {other:?}"),
                    ));
                }
            },
        },
        _ => return Ok(None),
    };
    let index = opt_u64(outcome, at, "index")?
        .ok_or_else(|| CheckError::malformed(format!("{at}.index"), "terminal without an index"))?;
    Ok(Some(Claim {
        attempt,
        replayed: opt_bool(outcome, at, "replayed")?.unwrap_or(false),
        result: TerminalResult {
            request_id: request_id(outcome, at, "request_id")?,
            index: Index::new(index).map_err(|error| bad(at, "index", &error))?,
            decision,
        },
        via_status,
    }))
}

fn command_of(command: &Value, at: &str) -> Result<Command, CheckError> {
    let operation = field(command, at, "operation")?;
    let at_op = format!("{at}.operation");
    let key = || -> Result<Key, CheckError> {
        Key::new(str_field(operation, &at_op, "key")?).map_err(|error| bad(&at_op, "key", &error))
    };
    let value = || -> Result<Data, CheckError> {
        Data::new(str_field(operation, &at_op, "value")?)
            .map_err(|error| bad(&at_op, "value", &error))
    };
    let op = match str_field(operation, &at_op, "op")? {
        "put" => Op::Put {
            key: key()?,
            value: value()?,
        },
        "delete" => Op::Delete { key: key()? },
        "cas" => {
            let expected = field(operation, &at_op, "expected")?;
            let at_expected = format!("{at_op}.expected");
            Op::Cas {
                key: key()?,
                expected: match str_field(expected, &at_expected, "kind")? {
                    "absent" => Expected::Absent,
                    "version" => Expected::Version(
                        Version::new(u64_field(expected, &at_expected, "version")?)
                            .map_err(|error| bad(&at_expected, "version", &error))?,
                    ),
                    other => {
                        return Err(CheckError::malformed(
                            at_expected,
                            format!("unknown expectation {other:?}"),
                        ));
                    }
                },
                value: value()?,
            }
        }
        other => {
            return Err(CheckError::malformed(
                at_op,
                format!("unknown write operation {other:?}"),
            ));
        }
    };
    Ok(Command {
        request_id: request_id(command, at, "request_id")?,
        op,
    })
}

fn limits(manifest: &Value) -> Result<Limits, CheckError> {
    let at = "manifest.limits";
    let limits = field(manifest, "manifest", "limits")?;
    Limits::new(
        u64_field(limits, at, "max_key_bytes")?,
        u64_field(limits, at, "max_value_bytes")?,
        u64_field(limits, at, "max_live_keys")?,
        u64_field(limits, at, "max_live_bytes")?,
    )
    .map_err(|error| CheckError::malformed(at, error.to_string()))
}

fn bad(at: &str, name: &str, error: &DataError) -> CheckError {
    CheckError::malformed(format!("{at}.{name}"), error.to_string())
}

fn log_entry(item: &Value, at: &str) -> Result<LogEntry, CheckError> {
    Ok(LogEntry {
        term: u64_field(item, at, "term")?,
        payload: str_field(item, at, "payload")?.to_owned(),
    })
}

fn node_of(record: &Value, at: &str) -> Result<u64, CheckError> {
    u64_field(field(record, at, "at")?, &format!("{at}.at"), "node")
}

fn request_id(object: &Value, at: &str, name: &str) -> Result<RequestId, CheckError> {
    RequestId::parse(str_field(object, at, name)?).map_err(|error| bad(at, name, &error))
}

fn field<'a>(object: &'a Value, at: &str, name: &str) -> Result<&'a Value, CheckError> {
    object
        .get(name)
        .ok_or_else(|| CheckError::malformed(format!("{at}.{name}"), "field absent".to_owned()))
}

fn str_field<'a>(object: &'a Value, at: &str, name: &str) -> Result<&'a str, CheckError> {
    field(object, at, name)?
        .as_str()
        .ok_or_else(|| CheckError::malformed(format!("{at}.{name}"), "not a string"))
}

fn u64_field(object: &Value, at: &str, name: &str) -> Result<u64, CheckError> {
    field(object, at, name)?
        .as_u64()
        .ok_or_else(|| CheckError::malformed(format!("{at}.{name}"), "not an unsigned integer"))
}

fn bool_field(object: &Value, at: &str, name: &str) -> Result<bool, CheckError> {
    field(object, at, name)?
        .as_bool()
        .ok_or_else(|| CheckError::malformed(format!("{at}.{name}"), "not a boolean"))
}

fn array_field<'a>(object: &'a Value, at: &str, name: &str) -> Result<&'a Vec<Value>, CheckError> {
    field(object, at, name)?
        .as_array()
        .ok_or_else(|| CheckError::malformed(format!("{at}.{name}"), "not an array"))
}

fn opt_u64(object: &Value, at: &str, name: &str) -> Result<Option<u64>, CheckError> {
    let value = field(object, at, name)?;
    if value.is_null() {
        return Ok(None);
    }
    value.as_u64().map(Some).ok_or_else(|| {
        CheckError::malformed(format!("{at}.{name}"), "not an unsigned integer or null")
    })
}

fn opt_bool(object: &Value, at: &str, name: &str) -> Result<Option<bool>, CheckError> {
    let value = field(object, at, name)?;
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| CheckError::malformed(format!("{at}.{name}"), "not a boolean or null"))
}

fn u64_set(value: &Value, at: &str) -> Result<BTreeSet<u64>, CheckError> {
    value
        .as_array()
        .ok_or_else(|| CheckError::malformed(at, "not an array"))?
        .iter()
        .map(|item| {
            item.as_u64()
                .ok_or_else(|| CheckError::malformed(at, "not an unsigned integer"))
        })
        .collect()
}
