#![cfg(test)]
//! Hand-made heal-side documents for K-7's and K-9's tests: five voters, a 50 000 µs tick, a
//! 30-tick heal budget (so the deadline is 2 500 000 µs after a heal that starts at 1 000 000),
//! and a builder that writes only the records those checkers read.

use serde_json::{Value, json};

use crate::heal::Heal;

const TICK_US: u64 = 50_000;
const HEAL_AT: u64 = 1_000_000;
/// The last `time` at which the heal budget still holds.
const DEADLINE: u64 = HEAL_AT + 30 * TICK_US;

/// A document under construction.
pub struct Run {
    mode: &'static str,
    records: Vec<Value>,
    lost: Vec<u64>,
}

/// What a variant of the healthy marker run changes.
struct Marker {
    nodes: Vec<(u64, &'static str)>,
    applied_at: u64,
    committed_at: Option<u64>,
    quiesce: bool,
    heal_phase: bool,
    resolution: &'static str,
    resolved_at: u64,
}

impl Marker {
    fn healthy() -> Self {
        Self {
            nodes: (1..=5).map(|node| (node, "d")).collect(),
            applied_at: 1_200_000,
            committed_at: Some(1_100_000),
            quiesce: true,
            heal_phase: true,
            resolution: "executed",
            resolved_at: 1_350_000,
        }
    }
}

impl Run {
    /// The write the runs make.
    pub const WRITE: &'static str = "000000000000000000000000000000a1";
    /// Another request ID.
    pub const OTHER: &'static str = "000000000000000000000000000000a2";

    fn record(&mut self, value: Value) {
        self.records.push(value);
    }

    fn phase(&mut self, phase: &str, time: u64) {
        self.record(json!({"kind": "phase", "seq": 0, "time": time, "phase": phase}));
    }

    fn invoke(&mut self, attempt: u64, op: &str, id: Option<&str>, time: u64) {
        let operation = if op == "status" {
            json!({"op": "status", "request_id": id})
        } else {
            json!({"op": op, "key": "k"})
        };
        let top = if op == "status" { None } else { id };
        self.record(
            json!({"kind": "invoke", "seq": 0, "time": time, "attempt": attempt,
                           "request_id": top, "operation": operation}),
        );
    }

    fn response(&mut self, attempt: u64, node: u64, time: u64) {
        self.record(
            json!({"kind": "response", "seq": 0, "time": time, "attempt": attempt,
                           "from": {"node": node, "incarnation": 0}}),
        );
    }

    fn committed(&mut self, index: u64, time: u64) {
        self.record(json!({"kind": "committed", "seq": 0, "time": time, "index": index}));
    }

    fn marker_record(&mut self, node: u64, incarnation: u64, index: u64, digest: &str, time: u64) {
        self.record(json!({"kind": "marker", "seq": 0, "time": time,
                           "at": {"node": node, "incarnation": incarnation},
                           "index": index, "state_digest": digest}));
    }

    fn resolution(&mut self, id: &str, resolved: &str, evidence: &[u64], time: u64) {
        let index = if resolved == "executed" {
            json!(2)
        } else {
            Value::Null
        };
        self.record(
            json!({"kind": "resolution", "seq": 0, "time": time, "request_id": id,
                           "resolved": resolved, "index": index, "evidence": evidence}),
        );
    }

    fn start(mode: &'static str) -> Self {
        Self {
            mode,
            records: Vec::new(),
            lost: Vec::new(),
        }
    }

    fn marker_run(mode: &'static str, variant: &Marker) -> Self {
        let mut run = Self::start(mode);
        run.phase("setup", 0);
        run.phase("baseline", 50_000);
        run.phase("chaos", 100_000);
        run.invoke(1, "put", Some(Self::WRITE), 200_000);
        run.response(1, 1, 250_000);
        run.committed(2, 250_000);
        if variant.heal_phase {
            run.phase("heal", HEAL_AT);
        }
        if let Some(time) = variant.committed_at {
            run.committed(3, time);
        }
        for (node, digest) in &variant.nodes {
            run.marker_record(*node, 0, 3, digest, variant.applied_at);
        }
        if variant.quiesce {
            run.record(json!({"kind": "quiescence", "seq": 0, "time": 1_300_000}));
        }
        run.phase("check", 1_400_000);
        match variant.resolution {
            "none" => {}
            "unresolved" => run.resolution(Self::WRITE, "unresolved", &[], variant.resolved_at),
            other => run.resolution(Self::WRITE, other, &[1], variant.resolved_at),
        }
        run
    }

    /// A healthy marker heal: every voter applies the marker at index 3 with one digest.
    pub fn marker() -> Self {
        Self::marker_run("marker", &Marker::healthy())
    }

    /// A marker heal where `skipped` never apply the marker.
    pub fn marker_without(skipped: &[u64]) -> Self {
        let mut variant = Marker::healthy();
        variant.nodes.retain(|(node, _)| !skipped.contains(node));
        Self::marker_run("marker", &variant)
    }

    /// A pre-created-marker heal where `skipped` never apply the marker.
    pub fn precreated_without(skipped: &[u64]) -> Self {
        let mut variant = Marker::healthy();
        variant.nodes.retain(|(node, _)| !skipped.contains(node));
        Self::marker_run("precreated_marker", &variant)
    }

    /// A marker heal whose nodes hold the given (digest, node) pairs at the marker.
    pub fn marker_with_digests(pairs: &[(&'static str, u64)]) -> Self {
        let mut variant = Marker::healthy();
        variant.nodes = pairs
            .iter()
            .map(|(digest, node)| (*node, *digest))
            .collect();
        Self::marker_run("marker", &variant)
    }

    /// A marker heal whose write is left unresolved.
    pub fn marker_unresolved() -> Self {
        let mut variant = Marker::healthy();
        variant.resolution = "unresolved";
        Self::marker_run("marker", &variant)
    }

    /// A marker heal whose write has no resolution record.
    pub fn marker_unrecorded() -> Self {
        let mut variant = Marker::healthy();
        variant.resolution = "none";
        Self::marker_run("marker", &variant)
    }

    /// A marker heal where every node applies the marker exactly at the deadline.
    pub fn marker_applied_at_deadline() -> Self {
        let mut variant = Marker::healthy();
        variant.applied_at = DEADLINE;
        variant.committed_at = Some(DEADLINE);
        Self::marker_run("marker", &variant)
    }

    /// A marker heal whose marker commits one microsecond after the deadline.
    pub fn marker_committed_late() -> Self {
        let mut variant = Marker::healthy();
        variant.committed_at = Some(DEADLINE + 1);
        Self::marker_run("marker", &variant)
    }

    /// A marker heal whose marker index never appears among the committed entries.
    pub fn marker_uncommitted() -> Self {
        let mut variant = Marker::healthy();
        variant.committed_at = None;
        Self::marker_run("marker", &variant)
    }

    /// A marker heal whose write resolves one microsecond after the deadline.
    pub fn marker_resolved_late() -> Self {
        let mut variant = Marker::healthy();
        variant.resolved_at = DEADLINE + 1;
        Self::marker_run("marker", &variant)
    }

    /// A drain heal that committed `index`, which the drain did not apply.
    pub fn drain_with_unapplied_commit(index: u64) -> Self {
        let mut run = Self::drain();
        run.committed(index, 1_200_000);
        run
    }

    /// A marker heal with no `quiescence` record.
    pub fn marker_without_quiescence() -> Self {
        let mut variant = Marker::healthy();
        variant.quiesce = false;
        Self::marker_run("marker", &variant)
    }

    /// A marker heal with no `heal` phase record.
    pub fn marker_without_heal_phase() -> Self {
        let mut variant = Marker::healthy();
        variant.heal_phase = false;
        Self::marker_run("marker", &variant)
    }

    /// Node `node`'s incarnation `incarnation` applies the marker at `index`.
    pub fn marker_at(&mut self, node: u64, incarnation: u64, index: u64, digest: &str) {
        self.marker_record(node, incarnation, index, digest, 1_200_000);
    }

    /// As [`Run::marker_at`], one microsecond after the deadline.
    pub fn marker_late(&mut self, node: u64, incarnation: u64, index: u64, digest: &str) {
        self.marker_record(node, incarnation, index, digest, DEADLINE + 1);
    }

    /// Records `node` as permanently lost, on top of those already lost.
    pub fn lose(&mut self, node: u64) {
        self.lost.push(node);
        let nodes = self.lost.clone();
        self.record(json!({"kind": "lost", "seq": 0, "time": 1_150_000, "nodes": nodes}));
    }

    /// Invokes a write after quiescence.
    pub fn late_put(&mut self, attempt: u64, id: &str) {
        self.invoke(attempt, "put", Some(id), 1_500_000);
    }

    /// Invokes a status lookup after quiescence.
    pub fn late_status(&mut self, attempt: u64, id: &str) {
        self.invoke(attempt, "status", Some(id), 1_500_000);
    }

    fn drain_run(
        rows: &[(u64, u64, u64, u64, &str)],
        quiet_ticks: u64,
        drain_time: Option<u64>,
        absence: Option<(&[u64], bool)>,
    ) -> Self {
        let mut run = Self::start("drain");
        run.phase("setup", 0);
        run.phase("baseline", 50_000);
        run.phase("chaos", 100_000);
        run.invoke(1, "put", Some(Self::WRITE), 200_000);
        run.phase("heal", HEAL_AT);
        run.record(json!({"kind": "quiescence", "seq": 0, "time": 1_100_000}));
        let lookups = |run: &mut Self, nodes: &[u64]| {
            for node in nodes {
                let attempt = 10 + node;
                run.invoke(attempt, "status", Some(Self::WRITE), 1_150_000);
                run.response(attempt, *node, 1_160_000);
            }
        };
        if let Some((nodes, after)) = absence
            && !after
        {
            lookups(&mut run, nodes);
        }
        if let Some(time) = drain_time {
            let nodes: Vec<Value> = rows
                .iter()
                .map(|(node, last, commit, applied, digest)| {
                    json!({"node": node, "last_index": last, "commit": commit,
                           "applied": applied, "state_digest": digest})
                })
                .collect();
            run.record(
                json!({"kind": "drain", "seq": 0, "time": time, "leader": 1, "term": 2,
                              "quiet_ticks": quiet_ticks, "nodes": nodes}),
            );
        }
        match absence {
            Some((nodes, after)) => {
                if after {
                    lookups(&mut run, nodes);
                }
                let evidence: Vec<u64> = nodes.iter().map(|node| 10 + node).collect();
                run.resolution(Self::WRITE, "not_executed", &evidence, 1_350_000);
            }
            None => run.resolution(Self::WRITE, "executed", &[1], 1_350_000),
        }
        run.phase("check", 1_400_000);
        run
    }

    fn settled(digest: &'static str) -> Vec<(u64, u64, u64, u64, &'static str)> {
        (1..=5).map(|node| (node, 5, 5, 5, digest)).collect()
    }

    /// A healthy drain heal.
    pub fn drain() -> Self {
        Self::drain_run(&Self::settled("d"), 4, Some(1_300_000), None)
    }

    /// A drain heal that never certifies a drain.
    pub fn drain_none() -> Self {
        Self::drain_run(&Self::settled("d"), 4, None, None)
    }

    /// A drain heal whose drain row for each node is (node, last, commit, applied, digest).
    pub fn drain_rows(rows: &[(u64, u64, u64, u64, &str)]) -> Self {
        Self::drain_run(rows, 4, Some(1_300_000), None)
    }

    /// A drain heal whose drain held `quiet_ticks` ticks.
    pub fn drain_quiet(quiet_ticks: u64) -> Self {
        Self::drain_run(&Self::settled("d"), quiet_ticks, Some(1_300_000), None)
    }

    /// A drain heal certified one microsecond after the deadline.
    pub fn drain_late() -> Self {
        Self::drain_run(&Self::settled("d"), 4, Some(DEADLINE + 1), None)
    }

    /// A drain heal where the write is resolved `not_executed` on status lookups answered by
    /// `nodes`, after the drain if `after_drain`, before it otherwise.
    pub fn drain_with_absence(nodes: &[u64], after_drain: bool) -> Self {
        Self::drain_run(
            &Self::settled("d"),
            4,
            Some(1_300_000),
            Some((nodes, after_drain)),
        )
    }

    /// The document.
    pub fn doc(&self) -> Value {
        json!({
            "schema": "keyval.observation/v1",
            "manifest": {"heal": self.mode, "tick_us": TICK_US, "chaos_ticks": 18,
                         "heal_budget_ticks": 30, "quiet_ticks": 4},
            "voters": [1, 2, 3, 4, 5],
            "records": self.records,
        })
    }

    /// The document as the heal view.
    pub fn heal(&self) -> Heal {
        Heal::from_value(&self.doc()).unwrap()
    }
}
