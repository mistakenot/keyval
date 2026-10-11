//! The coverage matrix (PLAN.md §10.4; W41.2): what a campaign's runs exercised, counted per
//! build from `keyval.observation/v1` documents, so that empty cells are visible. A seed count
//! alone is never a coverage argument.
//!
//! Six dimensions, each a fixed universe of cells (every cell is present in the output, zero
//! included):
//!
//! - **fault**: each fault type, counted per `fault` start record;
//! - **fault pair**: each unordered pair of distinct fault types, counted once per run in which
//!   both started (`a+b`, in the order of [`FAULTS`]);
//! - **topology**: each partition shape, counted per partition start: `3/2`, `4/1` and `2/2/1`
//!   for a symmetric split of five voters, `asymmetric_cycle` for directed links that form one
//!   cycle through at least three nodes, and `other_partition` for any other shape (kept so a
//!   shape outside §10.4 is counted, never dropped; it is never required);
//! - **crash point**: each crash point named by a crashed node, per node;
//! - **outcome**: each response outcome §10.4 lists, per `response` record. `ok` is `success`,
//!   `version_mismatch` is `mismatch`, `request_id_conflict` is `conflict` and
//!   `capacity_exceeded` is `live_capacity`. Codes outside the list (`not_found`,
//!   `invalid_request`, `too_large`) are not cells;
//! - **operation**: each operation, per `invoke` record (attempts, misuse included).
//!
//! The universe is also the enum of `schemas/coverage/coverage.v1.schema.json`, so the report and
//! the required set (`registry/coverage.toml`) can name nothing else.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::history::{array_field, field, str_field, u64_field};
use crate::report::CheckError;

/// Schema identifier of the coverage record.
pub const SCHEMA: &str = "keyval.coverage/v1";

/// The fault types of §9.1, in the schema's order.
pub const FAULTS: [&str; 9] = [
    "crash_restart",
    "all_node_crash",
    "permanent_loss",
    "symmetric_partition",
    "asymmetric_partition",
    "message_loss",
    "message_delay",
    "message_duplication",
    "stall",
];

/// The partition shapes (§10.4), then the catch-all.
pub const TOPOLOGIES: [&str; 5] = ["3/2", "4/1", "2/2/1", "asymmetric_cycle", "other_partition"];

/// The crash points of the schema's `fault` record (§9.1, §10.17).
pub const CRASH_POINTS: [&str; 8] = [
    "idle",
    "before_write",
    "mid_append",
    "after_write_before_sync",
    "after_sync_before_completion",
    "after_commit_before_reply",
    "after_truncate_before_sync",
    "after_create_before_dir_sync",
];

/// The outcomes of §10.4.
pub const OUTCOMES: [&str; 10] = [
    "success",
    "mismatch",
    "conflict",
    "live_capacity",
    "history_full",
    "unknown",
    "not_leader",
    "busy",
    "unavailable",
    "recovering",
];

/// The operations of §8.2.
pub const OPERATIONS: [&str; 6] = ["put", "delete", "cas", "get", "scan", "status"];

/// One axis of the matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Dimension {
    /// Each fault type.
    Fault,
    /// Each pair of fault types.
    FaultPair,
    /// Each partition topology.
    Topology,
    /// Each crash point.
    CrashPoint,
    /// Each outcome.
    Outcome,
    /// Each operation.
    Operation,
}

impl Dimension {
    /// Every dimension, in report order.
    pub const ALL: [Self; 6] = [
        Self::Fault,
        Self::FaultPair,
        Self::Topology,
        Self::CrashPoint,
        Self::Outcome,
        Self::Operation,
    ];

    /// The dimension's name in the JSON record and in `registry/coverage.toml`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fault => "fault",
            Self::FaultPair => "fault_pair",
            Self::Topology => "topology",
            Self::CrashPoint => "crash_point",
            Self::Outcome => "outcome",
            Self::Operation => "operation",
        }
    }

    /// Every cell of the dimension.
    #[must_use]
    pub fn universe(self) -> Vec<String> {
        match self {
            Self::Fault => owned(&FAULTS),
            Self::FaultPair => fault_pairs(),
            Self::Topology => owned(&TOPOLOGIES),
            Self::CrashPoint => owned(&CRASH_POINTS),
            Self::Outcome => owned(&OUTCOMES),
            Self::Operation => owned(&OPERATIONS),
        }
    }
}

fn owned(cells: &[&str]) -> Vec<String> {
    cells.iter().map(|cell| (*cell).to_owned()).collect()
}

/// Every unordered pair of distinct fault types, `a+b` with `a` before `b` in [`FAULTS`].
#[must_use]
pub fn fault_pairs() -> Vec<String> {
    let mut pairs = Vec::new();
    for (first, a) in FAULTS.iter().enumerate() {
        for b in &FAULTS[first + 1..] {
            pairs.push(format!("{a}+{b}"));
        }
    }
    pairs
}

/// What one or more runs of one build exercised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coverage {
    /// The build the runs ran on (`manifest.build_id`).
    pub build_id: String,
    /// How many runs are counted.
    pub runs: u64,
    cells: BTreeMap<Dimension, BTreeMap<String, u64>>,
}

impl Coverage {
    /// A matrix for `build_id` with every cell zero and no runs.
    #[must_use]
    pub fn empty(build_id: &str) -> Self {
        let cells = Dimension::ALL
            .iter()
            .map(|dimension| {
                (
                    *dimension,
                    dimension
                        .universe()
                        .into_iter()
                        .map(|cell| (cell, 0))
                        .collect(),
                )
            })
            .collect();
        Self {
            build_id: build_id.to_owned(),
            runs: 0,
            cells,
        }
    }

    /// How often `cell` of `dimension` was exercised; zero for a name outside the universe.
    #[must_use]
    pub fn count(&self, dimension: Dimension, cell: &str) -> u64 {
        self.cells
            .get(&dimension)
            .and_then(|row| row.get(cell))
            .copied()
            .unwrap_or(0)
    }

    /// Every cell with a zero count, in report order.
    #[must_use]
    pub fn empty_cells(&self) -> Vec<(Dimension, String)> {
        Dimension::ALL
            .iter()
            .flat_map(|dimension| {
                self.cells[dimension]
                    .iter()
                    .filter(|(_, count)| **count == 0)
                    .map(|(cell, _)| (*dimension, cell.clone()))
            })
            .collect()
    }

    fn bump(&mut self, dimension: Dimension, cell: &str) {
        if let Some(count) = self
            .cells
            .get_mut(&dimension)
            .and_then(|row| row.get_mut(cell))
        {
            *count += 1;
        }
    }

    /// Counts one `keyval.observation/v1` document.
    ///
    /// # Errors
    ///
    /// [`CheckError::Malformed`] naming the first field that is absent or wrong.
    pub fn of_document(doc: &Value) -> Result<Self, CheckError> {
        let manifest = field(doc, "", "manifest")?;
        let mut coverage = Self::empty(str_field(manifest, "manifest", "build_id")?);
        coverage.runs = 1;
        let mut started = BTreeSet::new();
        for (position, record) in array_field(doc, "", "records")?.iter().enumerate() {
            let at = format!("records[{position}]");
            match str_field(record, &at, "kind")? {
                "fault" if str_field(record, &at, "action")? == "start" => {
                    coverage.fault(record, &at, &mut started)?;
                }
                "invoke" => {
                    let operation = field(record, &at, "operation")?;
                    let op = str_field(operation, &format!("{at}.operation"), "op")?;
                    coverage.bump(Dimension::Operation, op);
                }
                "response" => {
                    let outcome = field(record, &at, "outcome")?;
                    let code = str_field(outcome, &format!("{at}.outcome"), "code")?;
                    if let Some(cell) = outcome_cell(code) {
                        coverage.bump(Dimension::Outcome, cell);
                    }
                }
                _ => {}
            }
        }
        for (first, a) in FAULTS.iter().enumerate() {
            for b in &FAULTS[first + 1..] {
                if started.contains(*a) && started.contains(*b) {
                    coverage.bump(Dimension::FaultPair, &format!("{a}+{b}"));
                }
            }
        }
        Ok(coverage)
    }

    fn fault<'a>(
        &mut self,
        record: &'a Value,
        at: &str,
        started: &mut BTreeSet<&'a str>,
    ) -> Result<(), CheckError> {
        let fault = str_field(record, at, "fault")?;
        self.bump(Dimension::Fault, fault);
        started.insert(fault);
        if let Some(points) = field(record, at, "crash_points")?.as_array() {
            for (item, point) in points.iter().enumerate() {
                let name = str_field(point, &format!("{at}.crash_points[{item}]"), "point")?;
                self.bump(Dimension::CrashPoint, name);
            }
        }
        if let Some(groups) = field(record, at, "partition")?.as_array() {
            let mut sizes = Vec::new();
            for (item, group) in groups.iter().enumerate() {
                let nodes = group.as_array().ok_or_else(|| {
                    CheckError::malformed(format!("{at}.partition[{item}]"), "not an array")
                })?;
                sizes.push(nodes.len());
            }
            self.bump(Dimension::Topology, split_shape(&mut sizes));
        }
        if fault == "asymmetric_partition"
            && let Some(links) = field(record, at, "links")?.as_array()
        {
            let cell = if is_directed_cycle(links, at)? {
                "asymmetric_cycle"
            } else {
                "other_partition"
            };
            self.bump(Dimension::Topology, cell);
        }
        Ok(())
    }

    /// Adds `other`'s counts to this matrix.
    ///
    /// # Errors
    ///
    /// [`CheckError::Malformed`] if the two are of different builds: coverage is per build, and
    /// a sum across builds would claim cells no single build exercised.
    pub fn merge(&mut self, other: &Self) -> Result<(), CheckError> {
        if self.build_id != other.build_id {
            return Err(CheckError::malformed(
                "manifest.build_id",
                format!(
                    "coverage is per build: {:?} cannot merge with {:?}",
                    self.build_id, other.build_id
                ),
            ));
        }
        self.runs += other.runs;
        for (dimension, row) in &other.cells {
            for (cell, count) in row {
                if let Some(slot) = self
                    .cells
                    .get_mut(dimension)
                    .and_then(|mine| mine.get_mut(cell))
                {
                    *slot += count;
                }
            }
        }
        Ok(())
    }

    /// The coverage record, `keyval.coverage/v1`: every cell of every dimension.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut cells = Map::new();
        for dimension in Dimension::ALL {
            let row: Map<String, Value> = self.cells[&dimension]
                .iter()
                .map(|(cell, count)| (cell.clone(), json!(count)))
                .collect();
            cells.insert(dimension.name().to_owned(), Value::Object(row));
        }
        json!({
            "schema": SCHEMA,
            "build_id": self.build_id,
            "runs": self.runs,
            "cells": Value::Object(cells),
        })
    }
}

/// The matrix cell of a response code, if §10.4 lists it.
fn outcome_cell(code: &str) -> Option<&'static str> {
    match code {
        "ok" => Some("success"),
        "version_mismatch" => Some("mismatch"),
        "request_id_conflict" => Some("conflict"),
        "capacity_exceeded" => Some("live_capacity"),
        "history_full" => Some("history_full"),
        "unknown_outcome" => Some("unknown"),
        "not_leader" => Some("not_leader"),
        "busy" => Some("busy"),
        "unavailable" => Some("unavailable"),
        "recovering" => Some("recovering"),
        _ => None,
    }
}

/// The topology cell of a symmetric split with these group sizes. A shape is only named when
/// the groups cover five voters, as the §10.4 shapes do.
fn split_shape(sizes: &mut [usize]) -> &'static str {
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    match sizes {
        [3, 2] => "3/2",
        [4, 1] => "4/1",
        [2, 2, 1] => "2/2/1",
        _ => "other_partition",
    }
}

/// Whether `links` are the edges of one directed cycle through at least three distinct nodes:
/// each node has one outgoing and one incoming link, and following them from any node visits
/// every node before returning.
fn is_directed_cycle(links: &[Value], at: &str) -> Result<bool, CheckError> {
    let mut next = BTreeMap::new();
    let mut incoming = BTreeSet::new();
    for (item, link) in links.iter().enumerate() {
        let here = format!("{at}.links[{item}]");
        let from = endpoint_node(link, &here, "from")?;
        let to = endpoint_node(link, &here, "to")?;
        match (from, to) {
            (Some(from), Some(to)) => {
                if next.insert(from, to).is_some() || !incoming.insert(to) {
                    return Ok(false);
                }
            }
            (None, _) | (_, None) => return Ok(false),
        }
    }
    let Some(start) = next.keys().next().copied() else {
        return Ok(false);
    };
    if next.len() < 3 {
        return Ok(false);
    }
    let mut seen = 1;
    let mut node = next[&start];
    while node != start {
        seen += 1;
        match next.get(&node) {
            Some(after) if seen <= next.len() => node = *after,
            Some(_) | None => return Ok(false),
        }
    }
    Ok(seen == next.len())
}

/// The node of a link endpoint; `None` for a client endpoint.
fn endpoint_node(link: &Value, at: &str, side: &str) -> Result<Option<u64>, CheckError> {
    let endpoint = field(link, at, side)?;
    let here = format!("{at}.{side}");
    if endpoint.get("node").is_some() {
        u64_field(endpoint, &here, "node").map(Some)
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN: &str = include_str!("../../../schemas/observation/fixtures/run.v1.json");
    const DRAIN: &str = include_str!("../../../schemas/observation/fixtures/drain.v1.json");
    const OUTCOMES_DOC: &str =
        include_str!("../../../schemas/observation/fixtures/outcomes.v1.json");
    const SCHEMA_FILE: &str = include_str!("../../../schemas/coverage/coverage.v1.schema.json");

    fn doc(text: &str) -> Value {
        serde_json::from_str(text).expect("fixture is JSON")
    }

    fn fault_start(fault: &str, nodes: &[u64], partition: &Value, links: &Value) -> Value {
        let crash = if fault == "crash_restart" || fault == "all_node_crash" {
            json!(
                nodes
                    .iter()
                    .map(|node| json!({"node": node, "point": "idle"}))
                    .collect::<Vec<_>>()
            )
        } else {
            Value::Null
        };
        json!({"kind": "fault", "seq": 1, "time": 1, "action": "start", "fault": fault,
               "nodes": nodes, "crash_points": crash, "partition": partition,
               "links": links})
    }

    fn with_records(records: Vec<Value>) -> Value {
        let mut document = doc(RUN);
        document["records"] = Value::Array(records);
        document
    }

    fn count_of(document: &Value, dimension: Dimension, cell: &str) -> u64 {
        Coverage::of_document(document)
            .expect("coverage")
            .count(dimension, cell)
    }

    // check: COVERAGE-MATRIX
    #[test]
    fn every_cell_of_every_dimension_is_present_and_zero_when_nothing_ran() {
        let empty = Coverage::empty("b1");
        let value = empty.to_value();
        assert_eq!(value["schema"], SCHEMA, "rule: the record names its schema");
        for dimension in Dimension::ALL {
            let row = value["cells"][dimension.name()]
                .as_object()
                .expect("dimension row");
            let expected = dimension.universe();
            assert_eq!(
                row.keys().cloned().collect::<BTreeSet<_>>(),
                expected.iter().cloned().collect::<BTreeSet<_>>(),
                "rule: every cell of {} is present; row={row:?}",
                dimension.name()
            );
            assert!(
                row.values().all(|count| count == 0),
                "rule: an empty matrix counts nothing; {}",
                dimension.name()
            );
        }
        assert_eq!(
            empty.empty_cells().len(),
            9 + 36 + 5 + 8 + 10 + 6,
            "rule: with no runs every cell is empty"
        );
    }

    #[test]
    fn the_universe_matches_the_schema_enumeration() {
        let schema: Value = serde_json::from_str(SCHEMA_FILE).expect("schema is JSON");
        for dimension in Dimension::ALL {
            let listed: Vec<String> = schema["properties"]["cells"]["properties"][dimension.name()]
                ["required"]
                .as_array()
                .expect("the schema lists the cells of every dimension")
                .iter()
                .map(|cell| cell.as_str().expect("cell name").to_owned())
                .collect();
            let mut universe = dimension.universe();
            universe.sort();
            let mut listed_sorted = listed;
            listed_sorted.sort();
            assert_eq!(
                listed_sorted,
                universe,
                "rule: the coverage schema names exactly the checker's cells; dimension={}",
                dimension.name()
            );
        }
    }

    #[test]
    fn the_fixture_run_counts_its_faults_pairs_and_crash_points() {
        let coverage = Coverage::of_document(&doc(RUN)).expect("coverage");
        assert_eq!(coverage.runs, 1, "rule: one document is one run");
        for fault in [
            "crash_restart",
            "symmetric_partition",
            "stall",
            "permanent_loss",
        ] {
            assert!(
                coverage.count(Dimension::Fault, fault) >= 1,
                "rule: the run injects {fault}; {coverage:?}"
            );
        }
        assert_eq!(
            coverage.count(Dimension::Fault, "all_node_crash"),
            0,
            "rule: a fault the run never injected stays zero"
        );
        assert_eq!(
            coverage.count(Dimension::FaultPair, "crash_restart+symmetric_partition"),
            1,
            "rule: a pair counts once per run in which both started"
        );
        assert_eq!(
            coverage.count(Dimension::FaultPair, "all_node_crash+stall"),
            0,
            "rule: a pair with an uninjected member stays zero"
        );
        let crash_total: u64 = CRASH_POINTS
            .iter()
            .map(|point| coverage.count(Dimension::CrashPoint, point))
            .sum();
        assert!(
            crash_total >= 1,
            "rule: the fixture's crash names a crash point; {coverage:?}"
        );
    }

    #[test]
    fn operations_and_outcomes_are_counted_from_invokes_and_responses() {
        let table = doc(OUTCOMES_DOC);
        let mut records = Vec::new();
        for operation in OPERATIONS {
            records.push(json!({"kind": "invoke", "operation": {"op": operation}}));
        }
        for entry in table["outcomes"].as_array().expect("outcomes") {
            records.push(json!({"kind": "response", "op": entry["op"],
                                "outcome": entry["outcome"]}));
        }
        let coverage = Coverage::of_document(&with_records(records)).expect("coverage");
        for operation in OPERATIONS {
            assert_eq!(
                coverage.count(Dimension::Operation, operation),
                1,
                "rule: one invoke of {operation} is one count; {coverage:?}"
            );
        }
        for outcome in OUTCOMES {
            assert!(
                coverage.count(Dimension::Outcome, outcome) >= 1,
                "rule: the outcomes fixture holds a {outcome} response; {coverage:?}"
            );
        }
    }

    #[test]
    fn response_codes_outside_the_listed_outcomes_are_not_cells() {
        let document = with_records(vec![
            json!({"kind": "response", "seq": 1, "time": 1, "attempt": "a", "from": {"node": 1, "incarnation": 0},
                   "generation": 1, "op": "get",
                   "outcome": {"code": "not_found", "value": null, "version": null,
                               "served_by": null, "lifecycle": null}}),
        ]);
        let coverage = Coverage::of_document(&document).expect("coverage");
        let total: u64 = OUTCOMES
            .iter()
            .map(|outcome| coverage.count(Dimension::Outcome, outcome))
            .sum();
        assert_eq!(
            total, 0,
            "rule: not_found is not one of §10.4's outcomes; {coverage:?}"
        );
    }

    #[test]
    fn symmetric_splits_map_to_the_three_named_shapes_and_anything_else_to_other() {
        for (groups, cell) in [
            (json!([[1, 2, 3], [4, 5]]), "3/2"),
            (json!([[5], [1, 2, 3, 4]]), "4/1"),
            (json!([[1, 2], [3, 4], [5]]), "2/2/1"),
            (json!([[1, 2, 3], [4], [5]]), "other_partition"),
            (json!([[1, 2], [3, 4, 5, 6, 7, 8]]), "other_partition"),
        ] {
            let document = with_records(vec![fault_start(
                "symmetric_partition",
                &[1, 2, 3, 4, 5],
                &groups,
                &Value::Null,
            )]);
            assert_eq!(
                count_of(&document, Dimension::Topology, cell),
                1,
                "rule: groups {groups} are the {cell} topology"
            );
        }
    }

    #[test]
    fn only_a_single_directed_cycle_of_three_or_more_nodes_is_an_asymmetric_cycle() {
        let link = |from: u64, to: u64| json!({"from": {"node": from}, "to": {"node": to}});
        let cycle3 = json!([link(1, 2), link(2, 3), link(3, 1)]);
        let cycle5 = json!([link(1, 2), link(2, 3), link(3, 4), link(4, 5), link(5, 1)]);
        let two_cycles = json!([link(1, 2), link(2, 1), link(3, 4), link(4, 3)]);
        let one_way = json!([link(1, 2)]);
        let mutual = json!([link(1, 2), link(2, 1)]);
        let fan_out = json!([link(1, 2), link(1, 3), link(2, 3)]);
        let with_client =
            json!([link(1, 2), link(2, 3), {"from": {"node": 3}, "to": {"client": 1}}]);
        for (links, cell) in [
            (&cycle3, "asymmetric_cycle"),
            (&cycle5, "asymmetric_cycle"),
            (&two_cycles, "other_partition"),
            (&one_way, "other_partition"),
            (&mutual, "other_partition"),
            (&fan_out, "other_partition"),
            (&with_client, "other_partition"),
        ] {
            let document = with_records(vec![fault_start(
                "asymmetric_partition",
                &[],
                &Value::Null,
                links,
            )]);
            assert_eq!(
                count_of(&document, Dimension::Topology, cell),
                1,
                "rule: links {links} count as {cell}"
            );
        }
    }

    #[test]
    fn a_fault_end_record_is_not_counted() {
        let mut end = fault_start("stall", &[2], &Value::Null, &Value::Null);
        end["action"] = json!("end");
        let document = with_records(vec![end]);
        assert_eq!(
            count_of(&document, Dimension::Fault, "stall"),
            0,
            "rule: only a fault's start counts as exercising it"
        );
    }

    #[test]
    fn merging_sums_counts_and_runs_and_refuses_another_build() {
        let mut total = Coverage::of_document(&doc(RUN)).expect("run");
        let drain = Coverage::of_document(&doc(DRAIN)).expect("drain");
        let before = total.count(Dimension::Operation, "put");
        total.merge(&drain).expect("same build merges");
        assert_eq!(total.runs, 2, "rule: merged runs add");
        assert_eq!(
            total.count(Dimension::Operation, "put"),
            before + drain.count(Dimension::Operation, "put"),
            "rule: merged counts add"
        );
        let other = Coverage::empty("another-build");
        assert!(
            total.merge(&other).is_err(),
            "rule: coverage is per build; a different build_id is refused"
        );
    }

    #[test]
    fn a_document_missing_its_records_is_an_error_not_an_empty_matrix() {
        let mut document = doc(RUN);
        document.as_object_mut().expect("object").remove("records");
        assert!(
            Coverage::of_document(&document).is_err(),
            "rule: an unreadable document is a CheckError, never a silent zero (P10)"
        );
    }
}
