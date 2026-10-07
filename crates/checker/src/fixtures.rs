#![cfg(test)]
//! Hand-made histories for the checkers' tests: a small document builder that writes only
//! the records K-1 and K-2 read, in the observation schema's field names.

use serde_json::{Value, json};

use crate::history::History;

/// Request IDs the fixtures use.
pub const A: &str = "000000000000000000000000000000a1";
pub const B: &str = "000000000000000000000000000000a2";

/// A document under construction: three voters, each recovered with an empty log.
pub struct Doc {
    records: Vec<Value>,
    lost: Vec<u64>,
}

impl Doc {
    pub fn new() -> Self {
        let records = (1..=3)
            .map(|node| {
                json!({"kind": "recovered", "seq": 0, "time": 0,
                       "at": {"node": node, "incarnation": 0},
                       "durable": {"term": 0, "vote": null, "last_index": 0, "last_term": 0},
                       "log": []})
            })
            .collect();
        Self {
            records,
            lost: Vec::new(),
        }
    }

    /// A document whose first entry is the leader's no-op, held by every voter.
    pub fn with_noop() -> Self {
        let mut doc = Self::new();
        doc.commit_everywhere(1, "", Value::Null);
        doc
    }

    pub fn lose(&mut self, node: u64) -> &mut Self {
        self.lost.push(node);
        self
    }

    /// Commits `command` at `index` and appends it to every voter's log.
    pub fn commit_everywhere(
        &mut self,
        index: u64,
        payload: &str,
        command: impl Into<Value>,
    ) -> &mut Self {
        for node in 1..=3 {
            self.append(node, index, payload);
        }
        self.commit(index, payload, command)
    }

    pub fn append(&mut self, node: u64, index: u64, payload: &str) -> &mut Self {
        self.records
            .push(json!({"kind": "append", "seq": 1, "time": 1,
            "at": {"node": node, "incarnation": 0}, "index": index, "term": 1,
            "payload": payload}));
        self
    }

    pub fn truncate(&mut self, node: u64, from_index: u64) -> &mut Self {
        self.records
            .push(json!({"kind": "truncate", "seq": 2, "time": 2,
            "at": {"node": node, "incarnation": 0}, "from_index": from_index}));
        self
    }

    /// A node restarts and recovers exactly `log` (index, payload).
    pub fn recover(&mut self, node: u64, log: &[(u64, &str)]) -> &mut Self {
        let log: Vec<Value> = log
            .iter()
            .map(|(index, payload)| json!({"index": index, "term": 1, "payload": payload}))
            .collect();
        self.records
            .push(json!({"kind": "recovered", "seq": 3, "time": 3,
            "at": {"node": node, "incarnation": 1},
            "durable": {"term": 1, "vote": null, "last_index": log.len(), "last_term": 1},
            "log": log}));
        self
    }

    pub fn commit(&mut self, index: u64, payload: &str, command: impl Into<Value>) -> &mut Self {
        let command: Value = command.into();
        self.records.push(
            json!({"kind": "committed", "seq": 4, "time": 4, "index": index,
            "term": 1, "payload": payload, "command": command}),
        );
        self
    }

    pub fn invoke(&mut self, attempt: u64, request_id: &str, misuse: bool) -> &mut Self {
        self.records
            .push(json!({"kind": "invoke", "seq": 5, "time": 5, "client": 1,
            "attempt": attempt, "target": {"node": 1, "incarnation": 0},
            "request_id": request_id, "misuse": misuse,
            "operation": {"op": "put", "key": "k", "value": "v"}}));
        self
    }

    pub fn respond(&mut self, attempt: u64, op: &str, outcome: impl Into<Value>) -> &mut Self {
        let outcome: Value = outcome.into();
        self.records.push(
            json!({"kind": "response", "seq": 6, "time": 6, "attempt": attempt,
            "from": {"node": 1, "incarnation": 0}, "generation": attempt, "op": op,
            "outcome": outcome}),
        );
        self
    }

    pub fn deliver(&mut self, attempt: u64) -> &mut Self {
        self.records.push(
            json!({"kind": "delivery", "seq": 7, "time": 7, "attempt": attempt,
            "delivered": true}),
        );
        self
    }

    /// A delivered `ok` answer to a PUT.
    pub fn put_ok(&mut self, attempt: u64, id: &str, index: u64, replayed: bool) -> &mut Self {
        self.respond(attempt, "put", ok_put(id, index, index, replayed))
            .deliver(attempt)
    }

    pub fn history(&self) -> History {
        History::from_value(&self.value()).expect("fixture parses")
    }

    pub fn value(&self) -> Value {
        json!({"schema": "keyval.observation/v1",
            "manifest": {"limits": {"max_key_bytes": 256, "max_value_bytes": 65536,
                "max_live_keys": 10000, "max_live_bytes": 67_108_864,
                "max_log_entries": 1_000_000, "max_history_bytes": 3_221_225_472_u64}},
            "voters": [1, 2, 3],
            "records": self.records.iter().cloned().chain(
                (!self.lost.is_empty()).then(|| json!({"kind": "lost", "seq": 8, "time": 8,
                    "nodes": self.lost}))).collect::<Vec<_>>()})
    }
}

/// A PUT command, as a `committed` record's `command`.
pub fn put(id: &str, key: &str, value: &str) -> Value {
    json!({"request_id": id, "operation": {"op": "put", "key": key, "value": value}})
}

pub fn delete(id: &str, key: &str) -> Value {
    json!({"request_id": id, "operation": {"op": "delete", "key": key}})
}

fn outcome(code: &str, id: &str) -> Value {
    json!({"code": code, "request_id": id, "index": null, "version": null, "existed": null,
        "replayed": null, "served_by": null, "attempt_state": null, "current_version": null,
        "original_index": null, "limit": null, "leader_hint": null, "lifecycle": null})
}

pub fn ok_put(id: &str, index: u64, version: u64, replayed: bool) -> Value {
    let mut out = outcome("ok", id);
    out["index"] = json!(index);
    out["version"] = json!(version);
    out["replayed"] = json!(replayed);
    out
}

pub fn ok_delete(id: &str, index: u64, existed: bool, replayed: bool) -> Value {
    let mut out = outcome("ok", id);
    out["index"] = json!(index);
    out["existed"] = json!(existed);
    out["replayed"] = json!(replayed);
    out
}

pub fn version_mismatch(id: &str, index: u64, current: u64, replayed: bool) -> Value {
    let mut out = outcome("version_mismatch", id);
    out["index"] = json!(index);
    out["current_version"] = json!(current);
    out["replayed"] = json!(replayed);
    out
}

pub fn conflict(id: &str, original_index: u64) -> Value {
    let mut out = outcome("request_id_conflict", id);
    out["original_index"] = json!(original_index);
    out
}
