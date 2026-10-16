//! The journal-exhaustion scenario (PLAN.md §6 DM-7, §9.4, D-61; W43.3): one node reaches its
//! local journal hard stop and the cluster shows what the plan says a node at the stop does.
//!
//! One seed, one script, five production node cores ([`crate::cluster`]):
//! 1. **Elect** a leader and write once, so every journal holds real frames.
//! 2. **Limit:** a follower, the victim, gets a journal hard stop of its current length plus
//!    [`HEADROOM_BYTES`] and restarts under it ([`Cluster::limit_journal`]); the limit is
//!    local and below what the margin formula would allow a configuration, which is the point.
//! 3. **Exhaust:** the leader takes writes until the victim stops appending. The victim is
//!    then `failed` (visible as its lifecycle) with a [`LoopError::HardStop`] naming a frame
//!    that did not fit, and its journal is no longer than its stop.
//! 4. **Failed behaviour:** the victim drops and counts peer messages, answers a client
//!    `recovering`, and does not move; the other four nodes keep serving reads and take
//!    writes, and agree on one state.
//! 5. **Restart:** the victim restarts under the same stop, starts without being `failed`
//!    (it recovers normally), and then fails again at the first append that would exceed the
//!    stop: its journal never passes it.
//! 6. **Audit:** every running node's log mirror is checked against its disk (safety still
//!    runs on a run with an exhausted node).
//!
//! **Outside the healthy envelope (§9.4).** A node at its stop is an explicitly accepted v1
//! failure mode, so this scenario sits outside the healthy envelope and claims only what the
//! steps above observe. In particular it exhausts one node of five and never a quorum, so it
//! makes no liveness claim about a quorum at the stop: if a quorum ever reached it, writes
//! would stop until v2 compaction (DM-7). [`Facts::OUTSIDE_HEALTHY_ENVELOPE`] says so in the
//! report.
//!
//! Each step waits on what the run shows within a virtual deadline, like the slice scenario
//! ([`crate::slice`]), and a step that never happens fails the run.

use node::Failure;
use raft_adapter::LoopError;
use serde_json::{Value as Json, json};
use types::{
    AttemptId, ByteCount, ClientOp, Config, Key, KeyValueLimits, Lifecycle, Mutation, NodeId,
    NotReadyLifecycle, Refusal, RequestId, Response, Value,
};

use crate::cluster::{Cluster, ClusterError};
use crate::entropy::MasterSeed;
use crate::net::{LinkProfile, NetConfig, Ppm, QueueBound};
use crate::scheduler::{EventBudget, VirtualTime};
use crate::slice::Verdict;
use crate::trace::{ClientId, INCOMPLETE_MARKER, TraceHash, marks_incomplete};

/// The identifier of this scenario's report.
pub const SCHEMA: &str = "keyval.dev.exhaustion-run/v1";

/// The scenario's version.
pub const SCENARIO_VERSION: u64 = 1;

/// What the run says it is (P6).
pub const LABEL: &str = "simulated journal exhaustion: five production node cores over a \
                         simulated disk, network, clock and entropy in one process, one node \
                         at its journal hard stop; outside the healthy envelope (§9.4); not a \
                         real server or cluster (P6)";

/// How far above its journal's length at the start the victim's hard stop is set: room for a
/// few frames, so it works for a while and then stops.
pub const HEADROOM_BYTES: u64 = 400;

/// The most writes the exhaust step sends before it fails the run.
pub const MAX_WRITES: u64 = 64;

/// One virtual millisecond, in microseconds.
const MS: u64 = 1_000;

/// One virtual second, in microseconds.
const SECOND: u64 = 1_000 * MS;

/// The client every request of the script comes from.
const CLIENT: ClientId = ClientId::new(1);

/// The failure the victim showed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stopped {
    /// The journal's length when the frame was refused.
    pub end: u64,
    /// The refused frame's length.
    pub frame_len: u64,
    /// The hard stop.
    pub limit: u64,
}

impl Stopped {
    /// Whether the refused frame really did not fit under the stop.
    #[must_use]
    pub const fn did_not_fit(&self) -> bool {
        self.end.saturating_add(self.frame_len) > self.limit
    }
}

/// What the victim showed while it was `failed`. Its health is its lifecycle: the node's
/// only health surface in the simulator is `Lifecycle::Failed` (the server reports it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailedNode {
    /// The first stop.
    pub stopped: Stopped,
    /// The victim's journal length when first seen failed.
    pub journal_len_at_failure: u64,
    /// Its journal length after the quiet period: a failed node stops feeding Raft, so it
    /// has not moved.
    pub journal_len: u64,
    /// Peer envelopes it had dropped as a failed node when it was first seen failed.
    pub peers_dropped_at_failure: u64,
    /// The same count after a further virtual second: the failed node keeps counting.
    pub peers_dropped_later: u64,
    /// A client write sent to it was answered `recovering` (A-2).
    pub answered_recovering: bool,
    /// A client read sent to it was answered `recovering` too.
    pub read_answered_recovering: bool,
}

/// The victim after its restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Restarted {
    /// It was not `failed` when it started: it recovered normally.
    pub started_unfailed: bool,
    /// It failed again, at a frame that did not fit.
    pub refailed: Option<Stopped>,
    /// Its journal's length after failing again.
    pub journal_len: u64,
}

/// What the run showed, step by step; a step that did not happen is `None` or false.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// The leader.
    pub leader: Option<NodeId>,
    /// The node given the small hard stop.
    pub victim: Option<NodeId>,
    /// The hard stop it ran under.
    pub limit: u64,
    /// Step 3 and 4: the victim `failed`.
    pub failed: Option<FailedNode>,
    /// The leader took a write and answered a read while the victim was failed.
    pub others_served: bool,
    /// The other four nodes held one state digest.
    pub others_agree: bool,
    /// Step 5.
    pub restarted: Option<Restarted>,
    /// Step 6.
    pub mirrors_audited: bool,
}

impl Facts {
    /// The scenario is outside the healthy envelope (§9.4) and claims no quorum-exhaustion
    /// liveness (DM-7).
    pub const OUTSIDE_HEALTHY_ENVELOPE: bool = true;

    /// Whether every step was observed and held.
    #[must_use]
    pub fn all_observed(&self) -> bool {
        let failed = self.failed.is_some_and(|failed| {
            failed.stopped.did_not_fit()
                && failed.stopped.limit == self.limit
                && failed.journal_len <= self.limit
                && failed.journal_len == failed.stopped.end
                && failed.journal_len == failed.journal_len_at_failure
                && failed.peers_dropped_at_failure < failed.peers_dropped_later
                && failed.answered_recovering
                && failed.read_answered_recovering
        });
        let restarted = self.restarted.is_some_and(|restarted| {
            restarted.started_unfailed
                && restarted.journal_len <= self.limit
                && restarted
                    .refailed
                    .is_some_and(|stopped| stopped.did_not_fit() && stopped.limit == self.limit)
        });
        self.leader.is_some()
            && self.victim.is_some()
            && failed
            && self.others_served
            && self.others_agree
            && restarted
            && self.mirrors_audited
    }
}

/// One run of the scenario.
#[derive(Clone, Debug)]
pub struct Run {
    /// The seed.
    pub seed: MasterSeed,
    /// The run's event budget.
    pub budget: EventBudget,
    /// The overall verdict.
    pub verdict: Verdict,
    /// Why the run failed, step by step; empty when it passed or when the harness stopped
    /// before any step failed.
    pub failures: Vec<String>,
    /// Why the harness stopped the run early, if it did.
    pub error: Option<String>,
    /// What the run showed.
    pub facts: Facts,
    /// The canonical trace.
    pub trace: String,
    /// Events dispatched.
    pub events: u64,
    /// The virtual time the run ended at.
    pub end: VirtualTime,
}

impl Run {
    /// The canonical trace's hash.
    #[must_use]
    pub fn hash(&self) -> TraceHash {
        TraceHash::of(&self.trace)
    }

    /// The run's report, `keyval.dev.exhaustion-run/v1`.
    #[must_use]
    pub fn report(&self) -> Json {
        let facts = &self.facts;
        json!({
            "schema": SCHEMA,
            "ok": self.verdict == Verdict::Pass,
            "label": LABEL,
            "outside_healthy_envelope": Facts::OUTSIDE_HEALTHY_ENVELOPE,
            "liveness_claim": "none: one node of five is exhausted, never a quorum (DM-7)",
            "manifest": {
                "scenario_version": SCENARIO_VERSION,
                "seed": self.seed.get(),
                "max_events": self.budget.get(),
                "victim_journal_limit": facts.limit,
            },
            "verdict": self.verdict.as_str(),
            "failures": self.failures,
            "error": self.error,
            "facts": {
                "leader": facts.leader.map(NodeId::get),
                "victim": facts.victim.map(NodeId::get),
                "failed": facts.failed.map(|failed| json!({
                    "end": failed.stopped.end,
                    "frame_len": failed.stopped.frame_len,
                    "limit": failed.stopped.limit,
                    "journal_len": failed.journal_len,
                    "journal_len_at_failure": failed.journal_len_at_failure,
                    "peers_dropped_at_failure": failed.peers_dropped_at_failure,
                    "peers_dropped": failed.peers_dropped_later,
                    "answered_recovering": failed.answered_recovering,
                    "read_answered_recovering": failed.read_answered_recovering,
                })),
                "others_served": facts.others_served,
                "others_agree": facts.others_agree,
                "restarted": facts.restarted.map(|restarted| json!({
                    "started_unfailed": restarted.started_unfailed,
                    "refailed": restarted.refailed.is_some(),
                    "journal_len": restarted.journal_len,
                })),
                "mirrors_audited": facts.mirrors_audited,
            },
            "trace": {
                "hash": format!("{:016x}", self.hash().get()),
                "events": self.events,
                "complete": self.error.is_none() && !marks_incomplete(&self.trace),
            },
            "virtual_time_us": self.end.as_micros(),
        })
    }
}

/// Runs the scenario under `config` with `seed` and `budget`.
#[must_use]
pub fn run(config: &Config, seed: MasterSeed, budget: EventBudget) -> Run {
    run_with(config, seed, budget, HEADROOM_BYTES)
}

/// Runs the scenario with the victim's hard stop `headroom` bytes above its journal.
#[must_use]
pub fn run_with(config: &Config, seed: MasterSeed, budget: EventBudget, headroom: u64) -> Run {
    let base = Run {
        seed,
        budget,
        verdict: Verdict::Incomplete,
        failures: Vec::new(),
        error: None,
        facts: Facts::default(),
        trace: format!("{INCOMPLETE_MARKER}\n"),
        events: 0,
        end: VirtualTime::ZERO,
    };
    let peer = LinkProfile::new(200, 2 * MS, Ppm::NEVER, Ppm::NEVER);
    let client = LinkProfile::new(100, MS, Ppm::NEVER, Ppm::NEVER);
    let (Some(peer), Some(client)) = (peer, client) else {
        return Run {
            error: Some("the scenario's link profiles are malformed".to_owned()),
            ..base
        };
    };
    let net = NetConfig {
        peer,
        client,
        queue: QueueBound::of(config),
        max_in_flight: budget,
    };
    let cluster = match Cluster::new(config.clone(), seed, net, budget) {
        Ok(cluster) => cluster,
        Err(error) => return stopped(base, &error, Vec::new(), &Facts::default()),
    };
    let mut script = Script {
        cluster,
        facts: Facts::default(),
        failures: Vec::new(),
        writes: 0,
        headroom,
    };
    let result = script.exhaustion();
    let trace = script.cluster.trace().canonical();
    let events = script.cluster.events().get();
    let end = script.cluster.now();
    let finished = match result {
        Ok(()) => verdict(base, script.failures, &script.facts),
        Err(error) => stopped(base, &error, script.failures, &script.facts),
    };
    Run {
        facts: script.facts,
        trace,
        events,
        end,
        ..finished
    }
}

/// `base` with the verdict of a script that ended on its own with `failures` and `facts`.
fn verdict(base: Run, mut failures: Vec<String>, facts: &Facts) -> Run {
    if failures.is_empty() && !facts.all_observed() {
        failures.push(format!(
            "the scenario ended without observing every step: {facts:?}"
        ));
    }
    Run {
        verdict: if failures.is_empty() {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        failures,
        ..base
    }
}

/// `base` for a script a driving call stopped: failed on a finding about the system under
/// test, incomplete (P10) if the harness ran out or was misdriven.
fn stopped(base: Run, error: &ClusterError, mut failures: Vec<String>, facts: &Facts) -> Run {
    if error.is_finding() {
        failures.push(format!("the run stopped on a finding: {error}"));
        verdict(base, failures, facts)
    } else {
        let verdict = if failures.is_empty() {
            Verdict::Incomplete
        } else {
            Verdict::Fail
        };
        Run {
            verdict,
            failures,
            error: Some(error.to_string()),
            ..base
        }
    }
}

/// The script's state: the cluster, what it showed and what went wrong.
struct Script {
    cluster: Cluster,
    facts: Facts,
    failures: Vec<String>,
    /// Writes sent so far; their request IDs and keys are numbered by it.
    writes: u64,
    /// Bytes between the victim's journal and its hard stop.
    headroom: u64,
}

impl Script {
    /// The scenario, step by step; a step that does not happen ends it with a failure.
    fn exhaustion(&mut self) -> Result<(), ClusterError> {
        let Some(leader) = self.elect()? else {
            return Ok(());
        };
        self.facts.leader = Some(leader);
        let Some(victim) = self.cluster.nodes().find(|id| *id != leader) else {
            self.fail("the cluster has no follower to exhaust");
            return Ok(());
        };
        self.facts.victim = Some(victim);
        if !self.write(leader)? {
            return Ok(());
        }
        if !self.limit(victim)? || !self.exhaust(leader, victim)? {
            return Ok(());
        }
        self.failed_behaviour(leader, victim)?;
        if self.restart(leader, victim)? {
            match self.cluster.audit_mirrors() {
                Ok(()) => self.facts.mirrors_audited = true,
                Err(error) => self.fail(&format!("the mirror audit failed: {error}")),
            }
        }
        Ok(())
    }

    /// Step 1: runs until one running node leads and every running node knows it.
    fn elect(&mut self) -> Result<Option<NodeId>, ClusterError> {
        let agreed = |cluster: &Cluster| {
            cluster.leader().filter(|leader| {
                cluster.nodes().all(|id| {
                    cluster
                        .controller(id)
                        .is_none_or(|ctl| ctl.status().leader == Some(*leader))
                })
            })
        };
        if !self.until(10 * MS, 10 * SECOND, |cluster| agreed(cluster).is_some())? {
            self.fail("no leader that every node knows within 10 virtual seconds");
            return Ok(None);
        }
        Ok(agreed(&self.cluster))
    }

    /// Step 2: sets the victim's hard stop above its journal's length now, and restarts it
    /// under that stop. Returns whether it is running again, not failed.
    fn limit(&mut self, victim: NodeId) -> Result<bool, ClusterError> {
        let length = self.journal_len(victim);
        let limit = length.saturating_add(self.headroom);
        self.facts.limit = limit;
        self.cluster.limit_journal(victim, ByteCount::new(limit))?;
        if !self.restart_node(victim)? {
            return Ok(false);
        }
        let running = self
            .cluster
            .controller(victim)
            .is_some_and(|ctl| match ctl.lifecycle() {
                Lifecycle::Failed => false,
                Lifecycle::Starting | Lifecycle::Recovering | Lifecycle::Ready => true,
            });
        if !running {
            self.fail("the victim did not start under its hard stop");
        }
        Ok(running)
    }

    /// Step 3: the leader takes writes until the victim is `failed`.
    fn exhaust(&mut self, leader: NodeId, victim: NodeId) -> Result<bool, ClusterError> {
        while !self.is_failed(victim) {
            if self.writes >= MAX_WRITES {
                self.fail(&format!(
                    "{MAX_WRITES} writes did not take the victim n{victim} to its hard stop \
                     of {}",
                    self.facts.limit
                ));
                return Ok(false);
            }
            if !self.write(leader)? {
                return Ok(false);
            }
            self.until(MS, 200 * MS, |cluster| failed(cluster, victim))?;
        }
        Ok(true)
    }

    /// Step 4: what the failed victim does and what the other nodes do meanwhile.
    fn failed_behaviour(&mut self, leader: NodeId, victim: NodeId) -> Result<(), ClusterError> {
        let Some(stopped) = self.stop_of(victim) else {
            self.fail("the victim is failed but not by the journal's hard stop");
            return Ok(());
        };
        let dropped = self.dropped(victim);
        let journal_len_at_failure = self.journal_len(victim);
        // A write and a read sent to the victim itself are answered `recovering`.
        let (key, value) = (key("exh/direct"), value("direct"));
        let (Some(key), Some(value)) = (key, value) else {
            self.fail("the script's keys and values do not parse");
            return Ok(());
        };
        self.writes += 1;
        let direct_write = self.submit(victim, write_op(self.writes, &key, &value))?;
        let direct_read = self.submit(victim, ClientOp::Get { key })?;
        let written = self.answer(direct_write, 5 * SECOND)?;
        let read = self.answer(direct_read, 5 * SECOND)?;
        // The rest keep serving: one more write through the leader, and a read from it.
        let served = self.write(leader)? && self.read_from(leader)?;
        // The victim keeps counting the traffic it drops.
        self.run_for(SECOND)?;
        let others: Vec<NodeId> = self.cluster.nodes().filter(|id| *id != victim).collect();
        self.facts.others_served = served;
        self.facts.others_agree = self.agree(&others);
        if !is_recovering(written.as_ref()) {
            self.fail(&format!(
                "a write sent to the failed n{victim} was answered {written:?}, not recovering"
            ));
        }
        if !is_recovering(read.as_ref()) {
            self.fail(&format!(
                "a read sent to the failed n{victim} was answered {read:?}, not recovering"
            ));
        }
        self.facts.failed = Some(FailedNode {
            stopped,
            journal_len_at_failure,
            journal_len: self.journal_len(victim),
            peers_dropped_at_failure: dropped,
            peers_dropped_later: self.dropped(victim),
            answered_recovering: is_recovering(written.as_ref()),
            read_answered_recovering: is_recovering(read.as_ref()),
        });
        Ok(())
    }

    /// Step 5: the victim restarts under the same stop, starts unfailed and fails again at the
    /// first append that would pass it.
    fn restart(&mut self, leader: NodeId, victim: NodeId) -> Result<bool, ClusterError> {
        // The leader has entries the victim never appended: it stays behind, so the next
        // append the leader sends is one the stop refuses.
        if !self.write(leader)? || !self.restart_node(victim)? {
            return Ok(false);
        }
        let started_unfailed = !self.is_failed(victim);
        let refailed = self.until(MS, 10 * SECOND, |cluster| failed(cluster, victim))?;
        if !refailed {
            self.fail("the restarted victim did not fail again within 10 virtual seconds");
        }
        let refailed = self.stop_of(victim);
        if refailed.is_none() && self.is_failed(victim) {
            self.fail("the restarted victim failed again, but not by the journal's hard stop");
        }
        self.facts.restarted = Some(Restarted {
            started_unfailed,
            refailed,
            journal_len: self.journal_len(victim),
        });
        Ok(true)
    }

    /// Crashes `node` and restarts it a virtual 100 ms later; returns whether the restart
    /// happened (the run is at the instant it started).
    fn restart_node(&mut self, node: NodeId) -> Result<bool, ClusterError> {
        let now = self.cluster.now();
        self.cluster.crash_at(now, node)?;
        self.cluster.run_until(now)?;
        let at = VirtualTime::from_micros(now.as_micros() + 100 * MS);
        self.cluster.restart_at(at, node)?;
        self.cluster.run_until(at)?;
        let up = self.cluster.controller(node).is_some();
        if !up {
            self.fail(&format!("node n{node} did not restart"));
        }
        Ok(up)
    }

    /// A PUT through `leader` under the next request ID, answered `written`.
    fn write(&mut self, leader: NodeId) -> Result<bool, ClusterError> {
        self.writes += 1;
        let (Some(key), Some(value)) = (key(&format!("exh/k{}", self.writes)), value("v")) else {
            self.fail("the script's keys and values do not parse");
            return Ok(false);
        };
        let attempt = self.submit(leader, write_op(self.writes, &key, &value))?;
        match self.answer(attempt, 5 * SECOND)? {
            Some(Response::Written { .. }) => Ok(true),
            other => {
                self.fail(&format!(
                    "write {} through the leader n{leader} was answered {other:?}",
                    self.writes
                ));
                Ok(false)
            }
        }
    }

    /// A GET of the first write's key from `node`, answered with a value.
    fn read_from(&mut self, node: NodeId) -> Result<bool, ClusterError> {
        let Some(key) = key("exh/k1") else {
            self.fail("the script's keys do not parse");
            return Ok(false);
        };
        let attempt = self.submit(node, ClientOp::Get { key })?;
        let found = matches!(
            self.answer(attempt, 5 * SECOND)?,
            Some(Response::Found { .. })
        );
        if !found {
            self.fail(&format!(
                "a GET from n{node} found nothing while the victim was failed"
            ));
        }
        Ok(found)
    }

    /// Whether `nodes` all hold one state digest.
    fn agree(&self, nodes: &[NodeId]) -> bool {
        let digests: Vec<_> = nodes
            .iter()
            .filter_map(|id| self.cluster.controller(*id))
            .map(|ctl| ctl.state().digest())
            .collect();
        digests.len() == nodes.len() && digests.windows(2).all(|pair| pair[0] == pair[1])
    }

    /// Whether `node` is running and `failed`.
    fn is_failed(&self, node: NodeId) -> bool {
        failed(&self.cluster, node)
    }

    /// The hard stop that failed `node`, if that is why it failed.
    fn stop_of(&self, node: NodeId) -> Option<Stopped> {
        match self.cluster.controller(node)?.failure()? {
            Failure::Loop(LoopError::HardStop {
                end,
                frame_len,
                limit,
            }) => Some(Stopped {
                end: *end,
                frame_len: *frame_len,
                limit: *limit,
            }),
            Failure::Loop(_) | Failure::Storage(_) => None,
        }
    }

    /// The peer envelopes `node` has dropped as a failed node.
    fn dropped(&self, node: NodeId) -> u64 {
        self.cluster
            .controller(node)
            .map_or(0, |ctl| ctl.counters().peers_failed)
    }

    /// The length of `node`'s journal on its disk.
    fn journal_len(&self, node: NodeId) -> u64 {
        self.cluster
            .disk(node)
            .and_then(crate::disk::SimDisk::journal)
            .map_or(0, |bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
    }

    /// Submits `op` from the script's client to `node`, a virtual millisecond from now.
    fn submit(&mut self, node: NodeId, op: ClientOp) -> Result<AttemptId, ClusterError> {
        let at = VirtualTime::from_micros(self.cluster.now().as_micros() + MS);
        self.cluster.submit(at, CLIENT, node, op)
    }

    /// Runs until `attempt` is answered, at most `within` virtual microseconds.
    fn answer(
        &mut self,
        attempt: AttemptId,
        within: u64,
    ) -> Result<Option<Response>, ClusterError> {
        let first = |cluster: &Cluster| {
            cluster
                .answers()
                .find(|answer| answer.attempt == attempt)
                .map(|answer| answer.response.clone())
        };
        self.until(MS, within, |cluster| first(cluster).is_some())?;
        Ok(first(&self.cluster))
    }

    /// Runs `micros` virtual microseconds.
    fn run_for(&mut self, micros: u64) -> Result<(), ClusterError> {
        let until = VirtualTime::from_micros(self.cluster.now().as_micros() + micros);
        self.cluster.run_until(until)
    }

    /// Runs `step` virtual microseconds at a time until `done` holds, at most `within`.
    /// Returns whether it held.
    fn until(
        &mut self,
        step: u64,
        within: u64,
        done: impl Fn(&Cluster) -> bool,
    ) -> Result<bool, ClusterError> {
        let deadline = self.cluster.now().as_micros() + within;
        loop {
            if done(&self.cluster) {
                return Ok(true);
            }
            let now = self.cluster.now().as_micros();
            if now >= deadline {
                return Ok(false);
            }
            let next = VirtualTime::from_micros((now + step).min(deadline));
            self.cluster.run_until(next)?;
        }
    }

    fn fail(&mut self, why: &str) {
        self.failures.push(why.to_owned());
    }
}

/// Whether `node` is running and `failed`.
fn failed(cluster: &Cluster, node: NodeId) -> bool {
    cluster
        .controller(node)
        .is_some_and(|ctl| match ctl.lifecycle() {
            Lifecycle::Failed => true,
            Lifecycle::Starting | Lifecycle::Recovering | Lifecycle::Ready => false,
        })
}

/// Whether `response` is the `recovering` refusal of a `failed` node.
const fn is_recovering(response: Option<&Response>) -> bool {
    match response {
        Some(Response::Refused(Refusal::Recovering { lifecycle })) => match lifecycle {
            NotReadyLifecycle::Failed => true,
            NotReadyLifecycle::Starting | NotReadyLifecycle::Recovering => false,
        },
        Some(
            Response::Written { .. }
            | Response::Conflict { .. }
            | Response::Found { .. }
            | Response::NotFound { .. }
            | Response::Page(_)
            | Response::Status { .. }
            | Response::Refused(_),
        )
        | None => false,
    }
}

/// A key of the script's, if it parses.
fn key(text: &str) -> Option<Key> {
    Key::parse(text.to_owned(), KeyValueLimits::MAXIMA).ok()
}

/// A value of the script's, if it parses.
fn value(text: &str) -> Option<Value> {
    Value::parse(text.to_owned(), KeyValueLimits::MAXIMA).ok()
}

/// A PUT of `value` at `key` under the request ID numbered `n`.
fn write_op(n: u64, key: &Key, value: &Value) -> ClientOp {
    let mut bytes = [0xE5; 16];
    bytes[..8].copy_from_slice(&n.to_be_bytes());
    ClientOp::Write {
        request_id: RequestId::from_bytes(bytes),
        mutation: Mutation::Put {
            key: key.clone(),
            value: value.clone(),
        },
    }
}
