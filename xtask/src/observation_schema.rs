//! The observation schema's contract tests (PLAN.md §10.2 "Outputs", §10.5, T-12; W10.2).
//!
//! `schemas/observation/observation.v1.schema.json` is what the oracles read: the
//! reference model and the checkers consume these observations, never production types.
//! The tests prove the contract carries what they need:
//! - the schema follows the repository's conventions and compiles as draft 2020-12;
//! - the fixture runs validate, and together use every record kind, operation, attempt
//!   state, phase, sync state, fault action, role and process state;
//! - the outcome table holds one valid outcome for every (operation, code) pair §8.4
//!   allows, and the schema rejects every other pair by its code;
//! - the fixture runs keep the README's stream rules (what any producer guarantees, which
//!   no per-record schema can state) and are correct runs (DUR-2 cover, the served
//!   prefix, convergence on every recoverable node, faithful commands);
//! - each negative fixture is rejected exactly at the pointer its rule names;
//! - the README documents every record kind and which checkers read it.
#![cfg(test)]
// check: OBSERVATION-SCHEMA

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use jsonschema::{Draft, Validator};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::test_support::workspace_root;

/// The schema's directory, relative to the workspace root.
const DIR: &str = "schemas/observation";

/// The identifier the schema defines.
const IDENTIFIER: &str = "keyval.observation/v1";

/// The fixture runs.
const RUNS: [&str; 2] = ["fixtures/run.v1.json", "fixtures/drain.v1.json"];

/// Every write error code (§8.4).
const WRITE_CODES: [&str; 12] = [
    "ok",
    "not_leader",
    "invalid_request",
    "too_large",
    "version_mismatch",
    "request_id_conflict",
    "busy",
    "unavailable",
    "recovering",
    "unknown_outcome",
    "capacity_exceeded",
    "history_full",
];

/// The codes a read may answer with (§8.4, api/openapi.yaml's read operations).
const READ_CODES: [&str; 5] = ["ok", "invalid_request", "busy", "unavailable", "recovering"];

/// The operations that write.
const WRITES: [&str; 3] = ["put", "delete", "cas"];

/// The codes each operation may answer with: §8.4 and the responses api/openapi.yaml
/// lists per operation (PUT never mismatches; DELETE is never too large or over a
/// live-state limit; only GET answers `not_found`).
fn allowed() -> BTreeMap<&'static str, BTreeSet<&'static str>> {
    let writes = |without: &[&str]| -> BTreeSet<&'static str> {
        WRITE_CODES
            .into_iter()
            .filter(|code| !without.contains(code))
            .collect()
    };
    let reads = |with: &[&'static str]| -> BTreeSet<&'static str> {
        READ_CODES.into_iter().chain(with.iter().copied()).collect()
    };
    BTreeMap::from([
        ("put", writes(&["version_mismatch"])),
        (
            "delete",
            writes(&["version_mismatch", "too_large", "capacity_exceeded"]),
        ),
        ("cas", writes(&[])),
        ("get", reads(&["not_found"])),
        ("scan", reads(&[])),
        ("status", reads(&[])),
    ])
}

/// The schema definition of an operation's outcome.
fn outcome_def(op: &str) -> &'static str {
    match op {
        "get" => "get_outcome",
        "scan" => "scan_outcome",
        "status" => "status_outcome",
        _ => "write_outcome",
    }
}

fn path(relative: &str) -> PathBuf {
    workspace_root().join(DIR).join(relative)
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{} is not JSON: {err}", path.display()))
}

fn schema() -> Value {
    read_json(&path("observation.v1.schema.json"))
}

fn runs() -> Vec<(&'static str, Value)> {
    RUNS.into_iter()
        .map(|name| (name, read_json(&path(name))))
        .collect()
}

fn validator() -> Validator {
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .should_validate_formats(true)
        .build(&schema())
        .unwrap_or_else(|err| panic!("the observation schema does not compile: {err}"))
}

/// The JSON pointers where `instance` breaks the schema, with each message.
fn violations(validator: &Validator, instance: &Value) -> Vec<(String, String)> {
    validator
        .iter_errors(instance)
        .map(|error| (error.instance_path().to_string(), error.to_string()))
        .collect()
}

/// The strings an `enum` at `pointer` in the schema allows.
fn enum_at(schema: &Value, pointer: &str) -> BTreeSet<String> {
    schema
        .pointer(pointer)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("the schema has no enum at {pointer}"))
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect()
}

fn records(run: &Value) -> &Vec<Value> {
    run["records"].as_array().unwrap()
}

fn num(value: &Value, field: &str) -> u64 {
    value[field]
        .as_u64()
        .unwrap_or_else(|| panic!("{field} is not an integer in {value}"))
}

fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value[field]
        .as_str()
        .unwrap_or_else(|| panic!("{field} is not a string in {value}"))
}

/// A `{node, incarnation}` as a pair.
fn on(value: &Value) -> (u64, u64) {
    (num(value, "node"), num(value, "incarnation"))
}

/// An array of node IDs as a set.
fn node_set(value: &Value) -> BTreeSet<u64> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node.as_u64().unwrap())
        .collect()
}

fn index_of(index: u64) -> usize {
    usize::try_from(index).unwrap()
}

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

fn sha256_hex(payload: &str) -> String {
    hex(&Sha256::digest(unhex(payload)))
}

/// A command in the canonical command codec, version 1 (format byte, kind, request ID,
/// key length and key, expectation for a CAS, value length and value for a PUT or CAS).
fn encode(command: &Value) -> String {
    let op = &command["operation"];
    let kind = text(op, "op");
    let mut bytes = vec![
        1,
        match kind {
            "put" => 1,
            "delete" => 2,
            _ => 3,
        },
    ];
    bytes.extend(unhex(text(command, "request_id")));
    let key = text(op, "key").as_bytes();
    bytes.extend(u32::try_from(key.len()).unwrap().to_be_bytes());
    bytes.extend(key);
    if kind == "cas" {
        let expected = &op["expected"];
        if text(expected, "kind") == "absent" {
            bytes.push(0);
        } else {
            bytes.push(1);
            bytes.extend(num(expected, "version").to_be_bytes());
        }
    }
    if kind != "delete" {
        let value = text(op, "value").as_bytes();
        bytes.extend(u32::try_from(value.len()).unwrap().to_be_bytes());
        bytes.extend(value);
    }
    hex(&bytes)
}

/// `run` with `records` in place of its own, so one record can be judged on its own.
fn with_records(run: &Value, records: Vec<Value>) -> Value {
    let mut document = run.clone();
    document["records"] = Value::Array(records);
    document
}

fn response_record(op: &str, outcome: &Value) -> Value {
    json!({"kind": "response", "seq": 0, "time": 0, "attempt": 1,
           "from": {"node": 1, "incarnation": 0}, "generation": 1, "op": op,
           "outcome": outcome})
}

/// The release a response needs (K-11): a client success for a terminal execution
/// decision, a published read for a read answered with `served_by`, with the index it
/// decides or serves.
fn needed_release(record: &Value) -> Option<(&'static str, u64)> {
    let outcome = &record["outcome"];
    if WRITES.contains(&text(record, "op")) {
        outcome["index"]
            .as_u64()
            .map(|index| ("client_success", index))
    } else {
        outcome["served_by"].is_object().then(|| {
            (
                "published_read",
                num(&outcome["served_by"], "applied_index"),
            )
        })
    }
}

/// An invoked attempt: the request ID it carries, the logical write it belongs to, the
/// request a status lookup asks about, its operation and its target.
struct Invoked {
    request_id: Option<String>,
    logical: Option<String>,
    looks_up: Option<String>,
    op: String,
    target: (u64, u64),
}

/// A log entry: its term and payload.
type Entry = (u64, String);

/// A release: its step, incarnation, kind and index.
type Release = (u64, (u64, u64), String, u64);

/// The stream rules of README.md (what any producer guarantees), judged record by record
/// over one fixture run.
#[derive(Default)]
struct Stream {
    run: &'static str,
    voters: BTreeSet<u64>,
    selected: BTreeSet<String>,
    quiet_ticks: u64,
    last: (u64, u64),
    phases: Vec<(String, u64)>,
    open_faults: BTreeSet<(String, Vec<u64>)>,
    invoked: BTreeMap<u64, Invoked>,
    responded: BTreeSet<u64>,
    releases: BTreeMap<u64, Release>,
    generations: BTreeMap<(u64, u64), u64>,
    incarnations: BTreeMap<u64, u64>,
    syncs: BTreeMap<u64, (u64, u64, Value)>,
    logs: BTreeMap<u64, Vec<Entry>>,
    certificates: BTreeMap<(u64, u64, String), BTreeMap<u64, Vec<Entry>>>,
    committed: u64,
    quiescence: Option<u64>,
    drained: Option<u64>,
    last_seen: BTreeMap<String, usize>,
    resolved: BTreeSet<String>,
}

impl Stream {
    fn new(run: &'static str, document: &Value) -> Self {
        let manifest = &document["manifest"];
        Self {
            run,
            voters: node_set(&document["voters"]),
            selected: manifest["faults"]
                .as_array()
                .unwrap()
                .iter()
                .map(|fault| text(fault, "fault").to_owned())
                .collect(),
            quiet_ticks: num(manifest, "quiet_ticks"),
            ..Self::default()
        }
    }

    fn observe(&mut self, at: usize, record: &Value) {
        let now = (num(record, "seq"), num(record, "time"));
        assert!(
            now.0 >= self.last.0 && now.1 >= self.last.1,
            "rule 1: seq and time never decrease; run={} record {at}: {now:?} after {:?}",
            self.run,
            self.last
        );
        self.last = now;
        match text(record, "kind") {
            "phase" => self.phases.push((text(record, "phase").to_owned(), now.1)),
            "fault" => self.fault(at, record),
            "invoke" => self.invoke(at, record),
            "attempt" | "delivery" | "response" => self.named(at, record),
            "release" => self.release(record),
            "recovered" => self.recovered(at, record),
            "sync" => self.sync(at, record),
            "append" | "truncate" => self.log(at, record),
            "accept" => self.accept(at, record),
            "committed" => self.committed(at, record),
            "quiescence" => self.quiescence = Some(now.1),
            "drain" => self.drain(at, record),
            "resolution" => self.resolution(at, record),
            _ => {}
        }
        self.current_incarnation(at, record);
    }

    fn fault(&mut self, at: usize, record: &Value) {
        let nodes = node_set(&record["nodes"]);
        let fault = text(record, "fault").to_owned();
        assert!(
            nodes.is_subset(&self.voters) && self.selected.contains(&fault),
            "rule 9: a fault is one the manifest selected, on configured voters; run={} \
             record {at}",
            self.run
        );
        let key = (fault, nodes.iter().copied().collect::<Vec<u64>>());
        if text(record, "action") == "start" {
            self.open_faults.insert(key);
        } else {
            assert!(
                self.open_faults.remove(&key),
                "rule 9: a fault ends only after it started; run={} record {at}",
                self.run
            );
        }
        if let Some(groups) = record["partition"].as_array() {
            let members: Vec<u64> = groups.iter().flat_map(node_set).collect();
            let distinct: BTreeSet<u64> = members.iter().copied().collect();
            assert_eq!(
                distinct.len(),
                members.len(),
                "rule 9: partition groups are disjoint; run={} record {at}",
                self.run
            );
        }
        if let Some(points) = record["crash_points"].as_array() {
            let crashed: BTreeSet<u64> = points.iter().map(|point| num(point, "node")).collect();
            assert!(
                crashed == nodes && points.len() == nodes.len(),
                "rule 9: a crash names one crash point per crashed node; run={} record {at}",
                self.run
            );
        }
    }

    fn invoke(&mut self, at: usize, record: &Value) {
        let attempt = num(record, "attempt");
        let op = text(&record["operation"], "op").to_owned();
        assert!(
            self.quiescence.is_none() || op == "status",
            "rule 10: after quiescence every invocation is a status lookup; run={} record {at}",
            self.run
        );
        let request_id = record["request_id"].as_str().map(str::to_owned);
        let logical = request_id.clone().filter(|_| record["misuse"] == false);
        if let Some(request_id) = &logical {
            self.last_seen.insert(request_id.clone(), at);
        }
        let invoked = Invoked {
            request_id,
            logical,
            looks_up: record["operation"]["request_id"]
                .as_str()
                .map(str::to_owned),
            op,
            target: on(&record["target"]),
        };
        assert!(
            self.invoked.insert(attempt, invoked).is_none(),
            "rule 3: attempt {attempt} is invoked once; run={} again at record {at}",
            self.run
        );
    }

    fn named(&mut self, at: usize, record: &Value) {
        let attempt = num(record, "attempt");
        let invoked = self.invoked.get(&attempt).unwrap_or_else(|| {
            panic!(
                "rule 3: attempt {attempt} is named before it is invoked; run={} record {at}",
                self.run
            )
        });
        let (op, target) = (invoked.op.clone(), invoked.target);
        if let Some(request_id) = invoked.logical.clone() {
            self.last_seen.insert(request_id, at);
        }
        match text(record, "kind") {
            "attempt" => self.attempt(at, record, attempt),
            "response" => self.response(at, attempt, record, (&op, target)),
            _ => assert!(
                self.responded.contains(&attempt),
                "rule 4: a delivery follows a response; run={} attempt {attempt} at record {at}",
                self.run
            ),
        }
    }

    fn attempt(&self, at: usize, record: &Value, attempt: u64) {
        let invoked = &self.invoked[&attempt];
        let waits_on = record["attached_to"]
            .as_u64()
            .map(|other| &self.invoked[&other]);
        assert!(
            on(&record["at"]) == invoked.target
                && waits_on.is_none_or(|other| {
                    other.request_id == invoked.request_id && other.target == invoked.target
                }),
            "rule 3: an attempt's lifecycle runs on the incarnation it was sent to, and attaches \
             only to a same-ID attempt there; run={} record {at}",
            self.run
        );
    }

    fn release(&mut self, record: &Value) {
        if let Some(attempt) = record["attempt"].as_u64() {
            let release = (
                num(record, "seq"),
                on(&record["at"]),
                text(record, "what").to_owned(),
                num(&record["requires"], "last_index"),
            );
            self.releases.insert(attempt, release);
        }
    }

    fn response(&mut self, at: usize, attempt: u64, record: &Value, sent: (&str, (u64, u64))) {
        let from = on(&record["from"]);
        assert!(
            text(record, "op") == sent.0 && from == sent.1,
            "rule 3: a response answers the invoked operation, from the incarnation it was sent \
             to; run={} attempt {attempt} at record {at}",
            self.run
        );
        if let Some((what, index)) = needed_release(record) {
            let released = self.releases.get(&attempt).is_some_and(|release| {
                release.0 == num(record, "seq")
                    && release.1 == from
                    && release.2 == what
                    && release.3 >= index
            });
            assert!(
                released,
                "rule 4: a {what} response follows, in its step, its incarnation's release of \
                 index {index}; run={} attempt {attempt} at record {at}",
                self.run
            );
        }
        self.responded.insert(attempt);
        let generation = num(record, "generation");
        let before = self.generations.insert(from, generation);
        assert!(
            before.is_none_or(|before| generation > before),
            "rule 5: response generations strictly increase per incarnation; run={} record \
             {at}: {generation} after {before:?} at {from:?}",
            self.run
        );
    }

    fn recovered(&mut self, at: usize, record: &Value) {
        let (node, incarnation) = on(&record["at"]);
        let expected = self
            .incarnations
            .get(&node)
            .map_or(0, |current| current + 1);
        assert_eq!(
            incarnation, expected,
            "rule 6: incarnations of node {node} count up from 0; run={} record {at}",
            self.run
        );
        self.incarnations.insert(node, incarnation);
        let log: Vec<Entry> = record["log"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(position, entry)| {
                assert_eq!(
                    index_of(num(entry, "index")),
                    position + 1,
                    "rule 6: a recovered log runs from index 1; run={} record {at}",
                    self.run
                );
                (num(entry, "term"), text(entry, "payload").to_owned())
            })
            .collect();
        let durable = &record["durable"];
        assert_eq!(
            (
                index_of(num(durable, "last_index")),
                num(durable, "last_term")
            ),
            (log.len(), log.last().map_or(0, |entry| entry.0)),
            "rule 6: the recovered durable state describes the recovered log; run={} record {at}",
            self.run
        );
        self.logs.insert(node, log);
    }

    fn sync(&mut self, at: usize, record: &Value) {
        let (node, incarnation) = on(&record["at"]);
        let sync = num(record, "sync");
        let covers = record["covers"].clone();
        if text(record, "state") == "requested" {
            let fresh = self
                .syncs
                .insert(sync, (node, incarnation, covers))
                .is_none();
            assert!(
                fresh,
                "rule 7: sync IDs are unique; run={} record {at}",
                self.run
            );
            return;
        }
        let stale = text(record, "state") == "stale";
        let matches = self.syncs.get(&sync).is_some_and(|requested| {
            requested.0 == node
                && requested.2 == covers
                && if stale {
                    incarnation > requested.1
                } else {
                    incarnation == requested.1
                }
        });
        assert!(
            matches,
            "rule 7: a completed sync names its incarnation's requested sync, and a stale one a \
             request of an earlier incarnation, with the same covers; run={} record {at}",
            self.run
        );
    }

    fn log(&mut self, at: usize, record: &Value) {
        let node = on(&record["at"]).0;
        let log = self.logs.entry(node).or_default();
        if text(record, "kind") == "append" {
            assert_eq!(
                index_of(num(record, "index")),
                log.len() + 1,
                "rule 7: an append extends node {node}'s log by one; run={} record {at}",
                self.run
            );
            log.push((num(record, "term"), text(record, "payload").to_owned()));
        } else {
            let from = index_of(num(record, "from_index"));
            assert!(
                from >= 1 && from <= log.len(),
                "rule 7: a truncate removes an existing suffix of node {node}'s log (length \
                 {}); run={} record {at} from {from}",
                log.len(),
                self.run
            );
            log.truncate(from - 1);
        }
    }

    fn accept(&mut self, at: usize, record: &Value) {
        let node = on(&record["replica"]).0;
        let (index, term) = (num(record, "index"), num(record, "entry_term"));
        let digest = text(record, "payload_digest").to_owned();
        let log = &self.logs[&node];
        let held = log
            .get(index_of(index) - 1)
            .map(|(held_term, payload)| (*held_term, sha256_hex(payload)));
        assert_eq!(
            held,
            Some((term, digest.clone())),
            "rule 7: an accept names an entry its replica's log holds; run={} record {at}",
            self.run
        );
        if num(record, "acceptance_term") == term {
            let prefix = log[..index_of(index)].to_vec();
            self.certificates
                .entry((index, term, digest))
                .or_default()
                .insert(node, prefix);
        }
    }

    fn committed(&mut self, at: usize, record: &Value) {
        let index = num(record, "index");
        assert_eq!(
            index,
            self.committed + 1,
            "rule 8: committed indices run 1, 2, 3, … once each; run={} record {at}",
            self.run
        );
        self.committed = index;
        let entry: Entry = (num(record, "term"), text(record, "payload").to_owned());
        let certified = self
            .certificates
            .iter()
            .any(|((certified, _, _), holders)| {
                *certified >= index
                    && holders
                        .values()
                        .filter(|prefix| prefix.get(index_of(index) - 1) == Some(&entry))
                        .count()
                        * 2
                        > self.voters.len()
            });
        assert!(
            certified,
            "rule 8: a committed entry follows a certificate for it or for a later index whose \
             certifiers hold it (K-8); run={} record {at}",
            self.run
        );
        assert_eq!(
            entry.1.is_empty(),
            record["command"].is_null(),
            "rule 8: the payload is empty exactly when there is no command; run={} record {at}",
            self.run
        );
    }

    fn drain(&mut self, at: usize, record: &Value) {
        assert_eq!(
            num(record, "quiet_ticks"),
            self.quiet_ticks,
            "rule 10: a drain holds over the manifest's quiet period; run={} record {at}",
            self.run
        );
        self.drained = Some(num(record, "time"));
    }

    fn resolution(&mut self, at: usize, record: &Value) {
        let request_id = text(record, "request_id").to_owned();
        let after = self.last_seen.get(&request_id).copied();
        assert!(
            after.is_some_and(|last| last < at) && self.resolved.insert(request_id.clone()),
            "rule 11: one resolution per logical write, after its attempts; run={} record \
             {at}: {request_id}",
            self.run
        );
        for evidence in record["evidence"].as_array().unwrap() {
            let attempt = evidence.as_u64().unwrap();
            let about = self.invoked.get(&attempt).is_some_and(|invoked| {
                invoked.logical.as_ref() == Some(&request_id)
                    || invoked.looks_up.as_ref() == Some(&request_id)
            });
            assert!(
                about && self.responded.contains(&attempt),
                "rule 11: a resolution's evidence is answered attempts of that write or lookups \
                 of it; run={} record {at}: attempt {attempt}",
                self.run
            );
        }
    }

    fn current_incarnation(&self, at: usize, record: &Value) {
        for field in ["at", "replica", "from", "target"] {
            if record.get(field).is_some_and(Value::is_object) {
                let (node, incarnation) = on(&record[field]);
                assert!(
                    self.voters.contains(&node)
                        && self.incarnations.get(&node) == Some(&incarnation),
                    "rule 6: a record names a configured voter's current incarnation; run={} \
                     record {at} ({field})",
                    self.run
                );
            }
        }
    }

    fn finish(&self, document: &Value) {
        let manifest = &document["manifest"];
        let tick = num(manifest, "tick_us");
        let names: Vec<&str> = self.phases.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            ["setup", "baseline", "chaos", "heal", "check"],
            "rule 2: the phases appear once each, in order; run={}",
            self.run
        );
        let time = |phase: usize| self.phases[phase].1;
        assert!(
            time(3) - time(2) == num(manifest, "chaos_ticks") * tick
                && time(4) - time(3) <= num(manifest, "heal_budget_ticks") * tick,
            "rule 2: the heal starts chaos_ticks after the chaos phase, and the check within \
             the heal budget; run={}",
            self.run
        );
        assert_eq!(
            self.selected.len(),
            manifest["faults"].as_array().unwrap().len(),
            "rule 9: the manifest selects each fault type once; run={}",
            self.run
        );
        let budget = &document["event_budget"];
        assert_eq!(
            budget["exhausted"].as_bool(),
            Some(num(budget, "used") >= num(manifest, "max_events")),
            "rule 10: the event budget is exhausted exactly when max_events were used; run={}",
            self.run
        );
        if let Some(drained) = self.drained {
            let quiet = self.quiet_ticks * tick;
            assert!(
                self.quiescence
                    .is_some_and(|quiet_at| drained >= quiet_at + quiet),
                "rule 10: a drain follows quiescence by quiet_ticks; run={}",
                self.run
            );
        }
        let writes: BTreeSet<&String> = self.last_seen.keys().collect();
        let resolved: BTreeSet<&String> = self.resolved.iter().collect();
        assert_eq!(
            writes, resolved,
            "rule 11: every logical write is resolved; run={}",
            self.run
        );
    }
}

/// A node's durable state: the hard state and the log as of the request of the latest
/// completed sync, or of its recovery (DUR-2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Durable {
    term: u64,
    vote: Option<u64>,
    log: Vec<Entry>,
}

/// The checker properties a correct run has that one record stream shows: K-11's DUR-2
/// cover, K-5's served prefix, K-7's convergence on every recoverable node, K-12's single
/// response, K-13's injected faults, and commands that are what was invoked, faithfully
/// encoded.
#[derive(Default)]
struct Correct {
    run: &'static str,
    voters: BTreeSet<u64>,
    lost: BTreeSet<u64>,
    logs: BTreeMap<u64, Vec<Entry>>,
    durable: BTreeMap<u64, Durable>,
    requested: BTreeMap<u64, Durable>,
    latest: BTreeMap<u64, (u64, u64)>,
    responded: BTreeSet<u64>,
    invoked: BTreeMap<String, Vec<(Value, bool)>>,
    started: BTreeSet<String>,
    markers: BTreeMap<u64, String>,
    drained: Option<BTreeSet<u64>>,
    commands: Vec<Value>,
}

impl Correct {
    fn new(run: &'static str, document: &Value) -> Self {
        Self {
            run,
            voters: node_set(&document["voters"]),
            ..Self::default()
        }
    }

    fn observe(&mut self, at: usize, record: &Value) {
        match text(record, "kind") {
            "recovered" => self.recovered(record),
            "append" => {
                let entry = (num(record, "term"), text(record, "payload").to_owned());
                self.logs
                    .entry(on(&record["at"]).0)
                    .or_default()
                    .push(entry);
            }
            "truncate" => {
                let from = index_of(num(record, "from_index"));
                self.logs
                    .entry(on(&record["at"]).0)
                    .or_default()
                    .truncate(from - 1);
            }
            "sync" => self.sync(at, record),
            "release" => self.release(at, record),
            "accept" => self.accept(at, record),
            "node_state" => {
                let state = (num(record, "applied"), num(record, "commit"));
                self.latest.insert(on(&record["at"]).0, state);
            }
            "invoke" => self.invoke(record),
            "response" => self.response(at, record),
            "fault" if text(record, "action") == "start" => {
                self.started.insert(text(record, "fault").to_owned());
            }
            "lost" => self.lost = node_set(&record["nodes"]),
            "marker" => {
                let digest = text(record, "state_digest").to_owned();
                self.markers.insert(on(&record["at"]).0, digest);
            }
            "drain" => self.drain(at, record),
            "committed" => self.committed(at, record),
            "resolution" => self.resolution(at, record),
            _ => {}
        }
    }

    fn recovered(&mut self, record: &Value) {
        let node = on(&record["at"]).0;
        let log: Vec<Entry> = record["log"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| (num(entry, "term"), text(entry, "payload").to_owned()))
            .collect();
        let durable = &record["durable"];
        self.durable.insert(
            node,
            Durable {
                term: num(durable, "term"),
                vote: durable["vote"].as_u64(),
                log: log.clone(),
            },
        );
        self.logs.insert(node, log);
    }

    fn sync(&mut self, at: usize, record: &Value) {
        let (node, sync) = (on(&record["at"]).0, num(record, "sync"));
        match text(record, "state") {
            "requested" => {
                let covers = &record["covers"];
                let log = self.logs.get(&node).cloned().unwrap_or_default();
                let upto = index_of(num(covers, "last_index"));
                assert!(
                    upto <= log.len()
                        && log[..upto].last().map_or(0, |entry| entry.0)
                            == num(covers, "last_term"),
                    "K-11: a sync covers a prefix of the log it was requested for; run={} record \
                     {at}",
                    self.run
                );
                let snapshot = Durable {
                    term: num(covers, "term"),
                    vote: covers["vote"].as_u64(),
                    log: log[..upto].to_vec(),
                };
                self.requested.insert(sync, snapshot);
            }
            "completed" => {
                let snapshot = self.requested[&sync].clone();
                self.durable.insert(node, snapshot);
            }
            _ => {}
        }
    }

    /// Whether `node`'s durable state meets an obligation: the hard state (if any) is the
    /// durable one, and the durable log holds the current log up to `last_index`.
    fn covered(&self, node: u64, hard: Option<(u64, Option<u64>)>, last_index: u64) -> bool {
        let durable = &self.durable[&node];
        let current = &self.logs[&node];
        let upto = index_of(last_index);
        hard.is_none_or(|hard| hard == (durable.term, durable.vote))
            && durable.log.len() >= upto
            && current.len() >= upto
            && durable.log[..upto] == current[..upto]
    }

    fn release(&self, at: usize, record: &Value) {
        let node = on(&record["at"]).0;
        let need = &record["requires"];
        let hard = need["hard_state"].is_object().then(|| {
            (
                num(&need["hard_state"], "term"),
                need["hard_state"]["vote"].as_u64(),
            )
        });
        let last_index = num(need, "last_index");
        let last_term = self.logs[&node]
            .get(index_of(last_index).wrapping_sub(1))
            .map_or(0, |entry| entry.0);
        assert!(
            self.covered(node, hard, last_index) && last_term == num(need, "last_term"),
            "K-11: a {} is released only once its obligation is durable (DUR-2); run={} record \
             {at}: needs {need}, durable {:?}",
            text(record, "what"),
            self.run,
            self.durable[&node]
        );
    }

    fn accept(&self, at: usize, record: &Value) {
        let node = on(&record["replica"]).0;
        let term = num(record, "acceptance_term");
        assert!(
            self.durable[&node].term == term && self.covered(node, None, num(record, "index")),
            "K-11: an entry is accepted only once durable in the accepting term; run={} record \
             {at}",
            self.run
        );
    }

    fn invoke(&mut self, record: &Value) {
        if let Some(request_id) = record["request_id"].as_str() {
            let misuse = record["misuse"] == true;
            self.invoked
                .entry(request_id.to_owned())
                .or_default()
                .push((record["operation"].clone(), misuse));
        }
    }

    fn response(&mut self, at: usize, record: &Value) {
        let attempt = num(record, "attempt");
        assert!(
            self.responded.insert(attempt),
            "K-12: one response per attempt; run={} attempt {attempt} again at record {at}",
            self.run
        );
        let served = &record["outcome"]["served_by"];
        if served.is_object() {
            let node = on(&record["from"]).0;
            let prefix = num(served, "applied_index");
            let (applied, commit) = self.latest[&node];
            assert!(
                num(served, "node_id") == node && applied >= prefix && commit >= prefix,
                "K-5: the responding node serves the prefix, applied and committed there before \
                 it answered; run={} record {at}: node {node}, prefix {prefix}, applied \
                 {applied}, commit {commit}",
                self.run
            );
        }
    }

    fn drain(&mut self, at: usize, record: &Value) {
        let nodes = record["nodes"].as_array().unwrap();
        let digests: BTreeSet<&str> = nodes
            .iter()
            .map(|node| text(node, "state_digest"))
            .collect();
        assert!(
            digests.len() == 1
                && nodes.iter().all(|node| {
                    num(node, "last_index") == num(node, "commit")
                        && num(node, "commit") == num(node, "applied")
                }),
            "K-7: a certified drain has last = commit = applied and one digest; run={} record \
             {at}",
            self.run
        );
        self.drained = Some(nodes.iter().map(|node| num(node, "node")).collect());
    }

    fn committed(&mut self, at: usize, record: &Value) {
        let command = &record["command"];
        if command.is_object() {
            let request_id = text(command, "request_id");
            // A misuse command may commit as a no-effect conflict (§7.5 step 3, K-2), but
            // only after the logical write's own command: misuse follows its terminal
            // result (§10.3).
            let first = !self
                .commands
                .iter()
                .any(|earlier| earlier["request_id"].as_str() == Some(request_id));
            let invoked = self.invoked.get(request_id).is_some_and(|ops| {
                ops.iter()
                    .any(|(op, misuse)| *op == command["operation"] && !(first && *misuse))
            });
            assert!(
                invoked && encode(command) == text(record, "payload"),
                "K-4: a committed command is one a client invoked with that request ID, the \
                 first for an ID its logical write's own, and its payload is that command's \
                 canonical encoding; run={} record {at}",
                self.run
            );
        }
        self.commands.push(command.clone());
    }

    fn resolution(&self, at: usize, record: &Value) {
        match text(record, "resolved") {
            "executed" => {
                let index = index_of(num(record, "index"));
                assert_eq!(
                    self.commands[index - 1]["request_id"].as_str(),
                    Some(text(record, "request_id")),
                    "K-1: an executed write's index carries its command; run={} record {at}",
                    self.run
                );
            }
            "unresolved" => panic!(
                "K-9: a correct run resolves every logical write; run={} record {at}",
                self.run
            ),
            _ => {}
        }
    }

    fn finish(&self, manifest: &Value) {
        let selected: BTreeSet<String> = manifest["faults"]
            .as_array()
            .unwrap()
            .iter()
            .map(|fault| text(fault, "fault").to_owned())
            .collect();
        assert!(
            selected.is_subset(&self.started),
            "K-13: every selected fault is injected (§10.16); run={} selected={selected:?} \
             started={:?}",
            self.run,
            self.started
        );
        let recoverable: BTreeSet<u64> = self.voters.difference(&self.lost).copied().collect();
        let heal = text(manifest, "heal");
        let marked: BTreeSet<u64> = self.markers.keys().copied().collect();
        let digests: BTreeSet<&String> = self.markers.values().collect();
        let converged = if heal == "drain" {
            self.markers.is_empty() && self.drained.as_ref() == Some(&recoverable)
        } else {
            self.drained.is_none() && marked == recoverable && digests.len() == 1
        };
        assert!(
            converged,
            "K-7: a {heal} heal converges on every recoverable node {recoverable:?} (a certified \
             drain, or one marker digest per node); run={} markers={:?} drained={:?}",
            self.run, self.markers, self.drained
        );
    }
}

#[test]
fn the_schema_follows_the_repository_conventions() {
    let schema = schema();
    assert_eq!(
        (
            schema["$schema"].as_str(),
            schema["$id"].as_str(),
            schema["title"].as_str(),
            schema
                .pointer("/properties/schema/const")
                .and_then(Value::as_str),
            schema["additionalProperties"].as_bool(),
        ),
        (
            Some("https://json-schema.org/draft/2020-12/schema"),
            Some("https://keyval.invalid/schemas/observation/observation.v1.schema.json"),
            Some(IDENTIFIER),
            Some(IDENTIFIER),
            Some(false),
        ),
        "rule: draft 2020-12, an offline .invalid $id, the identifier as title and as the \
         `schema` const, and a closed document (schemas/dev/README.md conventions)"
    );
    let mut closed = 0;
    for (name, def) in schema["$defs"].as_object().unwrap() {
        if let Some(declared) = def["properties"].as_object() {
            // A dispatcher (`record`, `operation`) closes each branch instead of itself.
            let dispatcher = def.get("additionalProperties").is_none()
                && def["allOf"]
                    .as_array()
                    .is_some_and(|rules| rules.iter().all(|rule| rule.get("then").is_some()));
            if dispatcher {
                continue;
            }
            assert_eq!(
                def["additionalProperties"].as_bool(),
                Some(false),
                "rule: every object in the schema is closed; def={name}"
            );
            let required: BTreeSet<&str> = def["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            let declared: BTreeSet<&str> = declared.keys().map(String::as_str).collect();
            assert_eq!(
                required, declared,
                "rule: every field is present, null when absent; def={name}"
            );
            closed += 1;
        }
    }
    assert!(
        closed >= 19,
        "rule: the record kinds are closed objects; found {closed}"
    );
    validator();
}

#[test]
fn the_fixture_runs_validate_and_together_use_every_kind_operation_and_state() {
    let schema = schema();
    let validator = validator();
    let runs = runs();
    for (name, run) in &runs {
        let found = violations(&validator, run);
        assert!(
            found.is_empty(),
            "rule: {name} is a valid {IDENTIFIER} document; violations={found:#?}"
        );
    }
    let used = |kind: Option<&str>, pick: &dyn Fn(&Value) -> &Value| -> BTreeSet<String> {
        runs.iter()
            .flat_map(|(_, run)| records(run))
            .filter(|record| kind.is_none_or(|kind| record["kind"] == kind))
            .filter_map(|record| pick(record).as_str().map(str::to_owned))
            .collect()
    };
    let cases: [(&str, &str, BTreeSet<String>); 9] = [
        (
            "record kind",
            "/$defs/record/properties/kind/enum",
            used(None, &|r| &r["kind"]),
        ),
        (
            "operation",
            "/$defs/operation/properties/op/enum",
            used(Some("invoke"), &|r| &r["operation"]["op"]),
        ),
        (
            "attempt state",
            "/$defs/record_attempt/properties/state/enum",
            used(Some("attempt"), &|r| &r["state"]),
        ),
        (
            "phase",
            "/$defs/record_phase/properties/phase/enum",
            used(Some("phase"), &|r| &r["phase"]),
        ),
        (
            "sync state",
            "/$defs/record_sync/properties/state/enum",
            used(Some("sync"), &|r| &r["state"]),
        ),
        (
            "fault action",
            "/$defs/record_fault/properties/action/enum",
            used(Some("fault"), &|r| &r["action"]),
        ),
        (
            "release kind",
            "/$defs/record_release/properties/what/enum",
            used(Some("release"), &|r| &r["what"]),
        ),
        (
            "role",
            "/$defs/record_node_state/properties/role/enum",
            used(Some("node_state"), &|r| &r["role"]),
        ),
        (
            "process state",
            "/$defs/record_node_state/properties/process/enum",
            used(Some("node_state"), &|r| &r["process"]),
        ),
    ];
    for (what, pointer, seen) in cases {
        let missing: Vec<String> = enum_at(&schema, pointer)
            .difference(&seen)
            .cloned()
            .collect();
        assert!(
            missing.is_empty(),
            "rule: the fixture runs exercise every {what}, so each has a worked example \
             (W10.2); missing={missing:?}"
        );
    }
    let resolved = used(Some("resolution"), &|r| &r["resolved"]);
    for resolution in ["executed", "not_executed", "refused"] {
        assert!(
            resolved.contains(resolution),
            "rule: the fixture runs resolve a write as {resolution} (§10.2); seen={resolved:?}"
        );
    }
}

#[test]
fn the_outcome_table_covers_every_operation_and_code_and_nothing_else() {
    let schema = schema();
    let validator = validator();
    let run = read_json(&path(RUNS[0]));
    let table = read_json(&path("fixtures/outcomes.v1.json"));
    let mut seen: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for entry in table["outcomes"].as_array().unwrap() {
        let (op, outcome) = (text(entry, "op"), &entry["outcome"]);
        let code = text(outcome, "code");
        let document = with_records(&run, vec![response_record(op, outcome)]);
        let found = violations(&validator, &document);
        assert!(
            found.is_empty(),
            "rule: §8.4 allows a {op} to answer {code}; outcome={outcome} violations={found:#?}"
        );
        seen.entry(op).or_default().insert(code);
    }
    let allowed = allowed();
    assert_eq!(
        seen, allowed,
        "rule: the table has one outcome for every (operation, code) pair §8.4 allows"
    );
    let every: BTreeSet<&str> = allowed.values().flatten().copied().collect();
    for (op, codes) in &allowed {
        let fields = schema["$defs"][outcome_def(op)]["properties"]
            .as_object()
            .unwrap();
        for code in every.difference(codes) {
            // The target operation's own fields, so only the code can be what is wrong.
            let mut outcome: serde_json::Map<String, Value> = fields
                .keys()
                .map(|name| (name.clone(), Value::Null))
                .collect();
            outcome.insert("code".to_owned(), json!(code));
            let document = with_records(&run, vec![response_record(op, &Value::Object(outcome))]);
            let found = violations(&validator, &document);
            assert!(
                found.iter().any(|(at, _)| at == "/records/0/outcome/code"),
                "rule: §8.4 never lets a {op} answer {code}, so the schema rejects the code \
                 itself; found={found:#?}"
            );
        }
    }
}

#[test]
fn the_fixture_runs_keep_the_stream_rules() {
    for (name, run) in runs() {
        let mut stream = Stream::new(name, &run);
        for (at, record) in records(&run).iter().enumerate() {
            stream.observe(at, record);
        }
        stream.finish(&run);
    }
}

#[test]
fn the_fixture_runs_are_correct_runs() {
    for (name, run) in runs() {
        let mut correct = Correct::new(name, &run);
        for (at, record) in records(&run).iter().enumerate() {
            correct.observe(at, record);
        }
        correct.finish(&run["manifest"]);
    }
}

#[test]
fn every_negative_fixture_is_rejected_exactly_where_its_rule_says() {
    let validator = validator();
    let dir = path("invalid");
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("cannot list {}: {err}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for file in &entries {
        let fixture = read_json(file);
        let rule = fixture["rule"].as_str().unwrap_or_default();
        let error_at = fixture["error_at"].as_str();
        assert!(
            !rule.is_empty() && error_at.is_some(),
            "rule: a negative fixture states its rule and error_at; path={}",
            file.display()
        );
        let found = violations(&validator, &fixture["instance"]);
        let pointers: BTreeSet<&str> = found.iter().map(|(at, _)| at.as_str()).collect();
        assert_eq!(
            pointers,
            error_at.into_iter().collect(),
            "rule: {rule}; the schema rejects the fixture there and only there; path={} \
             found={found:#?}",
            file.display()
        );
    }
    assert!(
        entries.len() >= 22,
        "rule: the negative fixtures are all present (P10); found {}",
        entries.len()
    );
}

#[test]
fn the_readme_documents_every_record_kind() {
    let readme = fs::read_to_string(path("README.md")).unwrap();
    for kind in enum_at(&schema(), "/$defs/record/properties/kind/enum") {
        let row = format!("| `{kind}` |");
        let line = readme.lines().find(|line| line.starts_with(&row));
        assert!(
            line.is_some_and(|line| line.contains("K-")),
            "rule: README.md documents `{kind}` and the checkers that read it (T-12: a \
             documented observation schema)"
        );
    }
    assert!(
        readme.contains(IDENTIFIER),
        "rule: README.md names the identifier it documents"
    );
}
