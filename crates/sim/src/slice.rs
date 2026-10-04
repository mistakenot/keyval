//! The slice scenario (PLAN.md §12.2 W17, §10.2): the first five-node simulated slice,
//! which `./dev sim run --seed N --profile slice` runs through the `keyval-sim` binary.
//!
//! One seed, one script, five production node cores ([`crate::cluster`]):
//! 1. **Elect:** the cluster elects a leader that every running node knows.
//! 2. **PUT:** a client writes a key through the leader; it is a first execution, and it
//!    applies on every node.
//! 3. **GET:** the client reads the value back from the leader.
//! 4. **Crash:** a second PUT goes to the leader, and a follower crashes while its journal
//!    holds unsynced bytes: the crash model keeps a seed-chosen prefix of them (DUR-9).
//! 5. **Restart:** the follower restarts as its next incarnation from the bytes its disk
//!    kept (DUR-3) and catches up: the second write applied, its applied index and state
//!    digest the leader's.
//! 6. **Audit:** every running node's log mirror is checked against its disk.
//!
//! Each step waits on what the run shows, within a virtual deadline, so the script is the
//! same for every seed and the seed decides only how the run unfolds. A step that never
//! happens fails the run, and the steps after it do not run. A driving call that fails
//! stops the run and marks its trace incomplete (P10). The run then failed if the error is
//! a finding about the system under test ([`ClusterError::is_finding`]), and is incomplete
//! if the harness ran out or was misdriven. A run passes only with every step observed.
//!
//! The report is the `keyval.dev.sim-run/v1` document that `./dev sim run --json`
//! re-emits. Its `label` says what the run is (P6), and `./dev sim run` prints it as the
//! first line of its text: a simulated slice, five production node cores over a simulated
//! disk, network, clock and entropy in one process, never a real server or cluster. Its
//! manifest is §10.2's without the build ID, which W42's campaign runner adds (gate
//! receipts record it meanwhile).

use serde_json::{Value as Json, json};
use types::{
    AttemptId, ClientOp, Config, Incarnation, Key, KeyValueLimits, Lifecycle, LogIndex, Mutation,
    NodeId, RequestId, Response, Role, Term, Value,
};

use crate::cluster::{Cluster, ClusterError};
use crate::disk::{Crash, Fill};
use crate::entropy::MasterSeed;
use crate::net::{LinkProfile, NetConfig, Ppm, QueueBound};
use crate::scheduler::{EventBudget, VirtualTime};
use crate::trace::{ClientId, INCOMPLETE_MARKER, TraceHash, marks_incomplete};

/// The identifier of a run's report, which `./dev sim run --json` re-emits.
pub const SCHEMA: &str = "keyval.dev.sim-run/v1";

/// The run manifest's scenario version (§10.2): the script below is version 1.
pub const SCENARIO_VERSION: u64 = 1;

/// The pseudo-random generator every stream uses (§10.2's manifest `prng`).
pub const PRNG: &str = "splitmix64";

/// What every run says it is, before anything else (P6).
pub const LABEL: &str = "simulated slice: five production node cores over a simulated disk, \
                         network, clock and entropy in one process; not a real server or \
                         cluster (P6)";

/// One virtual millisecond, in microseconds.
const MS: u64 = 1_000;

/// One virtual second, in microseconds.
const SECOND: u64 = 1_000 * MS;

/// The client every request of the script comes from.
const CLIENT: ClientId = ClientId::new(1);

/// A run profile: what the scenario does. W42 adds the campaign profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    /// W17's slice: elect, PUT and GET, a follower crash with unsynced bytes, its restart
    /// and catch-up, and the mirror audit.
    Slice,
}

impl Profile {
    /// The profile named `name`, if there is one.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "slice" => Some(Self::Slice),
            _ => None,
        }
    }

    /// The profile's name, as `--profile` takes it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Slice => "slice",
        }
    }
}

/// A run's overall verdict (§10.2 outputs). Only `Pass` passes: `Incomplete` fails (P10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Every step happened and every check held.
    Pass,
    /// A step did not happen, a check did not hold, or a finding stopped the run.
    Fail,
    /// The run stopped before its end; its trace is marked incomplete.
    Incomplete,
}

impl Verdict {
    /// The verdict's name in the report.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Incomplete => "incomplete",
        }
    }
}

/// The leader the cluster elected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Elected {
    /// The leader.
    pub node: NodeId,
    /// Its term.
    pub term: Term,
    /// When every running node knew it.
    pub at: VirtualTime,
}

/// The first PUT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Put {
    /// The log index of its first execution.
    pub index: LogIndex,
}

/// The follower's crash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crashed {
    /// The follower.
    pub node: NodeId,
    /// The incarnation that crashed.
    pub incarnation: u64,
    /// The unsynced bytes its journal held.
    pub unsynced_bytes: u64,
    /// How many of them survived.
    pub kept_bytes: u64,
    /// Whether zeros or garbage followed the surviving prefix.
    pub damaged_tail: bool,
}

/// The follower's restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Restarted {
    /// The follower.
    pub node: NodeId,
    /// Its new incarnation.
    pub incarnation: u64,
    /// The applied index it caught up to, the leader's.
    pub applied: LogIndex,
}

/// What the run showed, step by step; a step that did not happen is `None`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// Step 1.
    pub elected: Option<Elected>,
    /// Step 2.
    pub put: Option<Put>,
    /// Step 3: the GET read the value back.
    pub read_back: bool,
    /// Step 4.
    pub crashed: Option<Crashed>,
    /// Step 5.
    pub restarted: Option<Restarted>,
    /// Step 6: every running node's mirror agreed with its disk.
    pub mirrors_audited: bool,
}

impl Facts {
    /// Whether every step of the scenario was observed.
    #[must_use]
    pub const fn all_observed(&self) -> bool {
        self.elected.is_some()
            && self.put.is_some()
            && self.read_back
            && self.crashed.is_some()
            && self.restarted.is_some()
            && self.mirrors_audited
    }
}

/// One run of a profile.
#[derive(Clone, Debug)]
pub struct Run {
    /// The seed.
    pub seed: MasterSeed,
    /// The profile.
    pub profile: Profile,
    /// The run's event budget.
    pub budget: EventBudget,
    /// The configuration's semantic fingerprint, 64 hex digits.
    pub config_fingerprint: String,
    /// The overall verdict.
    pub verdict: Verdict,
    /// Why the run failed, step by step; empty when it passed or is incomplete.
    pub failures: Vec<String>,
    /// Why the harness stopped the run early, if it did (the run is incomplete).
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

    /// The run's report: `keyval.dev.sim-run/v1`, with `trace_path` where the canonical
    /// trace was written, if it was.
    #[must_use]
    pub fn report(&self, trace_path: Option<&str>) -> Json {
        let facts = &self.facts;
        json!({
            "schema": SCHEMA,
            "ok": self.verdict == Verdict::Pass,
            "label": LABEL,
            "manifest": {
                "scenario_version": SCENARIO_VERSION,
                "seed": self.seed.get(),
                "profile": self.profile.name(),
                "prng": PRNG,
                "max_events": self.budget.get(),
                "config_fingerprint": self.config_fingerprint,
            },
            "verdict": self.verdict.as_str(),
            "failures": self.failures,
            "error": self.error,
            "facts": {
                "elected": facts.elected.map(|elected| json!({
                    "node": elected.node.get(),
                    "term": elected.term.get(),
                    "at_us": elected.at.as_micros(),
                })),
                "put": facts.put.map(|put| json!({ "index": put.index.get() })),
                "read_back": facts.read_back,
                "crashed": facts.crashed.map(|crashed| json!({
                    "node": crashed.node.get(),
                    "incarnation": crashed.incarnation,
                    "unsynced_bytes": crashed.unsynced_bytes,
                    "kept_bytes": crashed.kept_bytes,
                    "damaged_tail": crashed.damaged_tail,
                })),
                "restarted": facts.restarted.map(|restarted| json!({
                    "node": restarted.node.get(),
                    "incarnation": restarted.incarnation,
                    "applied": restarted.applied.get(),
                })),
                "mirrors_audited": facts.mirrors_audited,
            },
            "trace": {
                "hash": format!("{:016x}", self.hash().get()),
                "lines": self.trace.lines().count(),
                "events": self.events,
                "complete": self.error.is_none() && !marks_incomplete(&self.trace),
                "path": trace_path,
            },
            "virtual_time_us": self.end.as_micros(),
        })
    }
}

/// Runs `profile` under `config` with `seed` and `budget` (the default budget is
/// [`EventBudget::DEFAULT`]).
#[must_use]
pub fn run(config: &Config, seed: MasterSeed, profile: Profile, budget: EventBudget) -> Run {
    let config_fingerprint = config.semantic_fingerprint().to_string();
    let base = Run {
        seed,
        profile,
        budget,
        config_fingerprint,
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
            error: Some("the slice's link profiles are malformed".to_owned()),
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
        Err(error) => {
            // A core that would not recover or start is a finding; setup that ran out is not.
            let (verdict, failures, error) = judge(Some(&error), Vec::new(), &Facts::default());
            return Run {
                verdict,
                failures,
                error,
                ..base
            };
        }
    };
    let mut script = Script {
        cluster,
        facts: Facts::default(),
        failures: Vec::new(),
    };
    let stopped = match profile {
        Profile::Slice => script.slice().err(),
    };
    let trace = script.cluster.trace().canonical();
    let (verdict, failures, error) = judge(stopped.as_ref(), script.failures, &script.facts);
    Run {
        verdict,
        failures,
        error,
        facts: script.facts,
        events: script.cluster.events().get(),
        end: script.cluster.now(),
        trace,
        ..base
    }
}

/// The verdict of a run that ended with `failures` and `facts`, stopped by `stopped` if a
/// driving call failed; with the failures and the error the report carries.
///
/// - Stopped by the harness running out or being misdriven: incomplete (P10), with the
///   error.
/// - Stopped by a finding about the system under test: failed, the finding among the
///   failures.
/// - Otherwise failed if a step failed or went unobserved, and passed only with every step
///   observed.
#[must_use]
pub fn judge(
    stopped: Option<&ClusterError>,
    mut failures: Vec<String>,
    facts: &Facts,
) -> (Verdict, Vec<String>, Option<String>) {
    match stopped {
        Some(error) if !error.is_finding() => {
            return (Verdict::Incomplete, failures, Some(error.to_string()));
        }
        Some(error) => failures.push(format!("the run stopped on a finding: {error}")),
        None => {}
    }
    if failures.is_empty() && !facts.all_observed() {
        failures.push(format!(
            "the scenario ended without observing every step: {facts:?}"
        ));
    }
    let verdict = if failures.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Fail
    };
    (verdict, failures, None)
}

/// The script's state: the cluster, what it showed and what went wrong.
struct Script {
    cluster: Cluster,
    facts: Facts,
    failures: Vec<String>,
}

impl Script {
    /// The slice, step by step; a step that does not happen ends it with a failure.
    fn slice(&mut self) -> Result<(), ClusterError> {
        let Some(leader) = self.elect()? else {
            return Ok(());
        };
        let (Some(key), Some(first), Some(second_key), Some(second_value)) =
            (key("slice/k"), value("v1"), key("slice/k2"), value("v2"))
        else {
            self.fail("the script's keys and values do not parse");
            return Ok(());
        };
        if !self.put(leader, &key, &first)? || !self.get(leader, &key, &first)? {
            return Ok(());
        }
        let second = (second_key, second_value);
        let Some(follower) = self.crash(leader, &second)? else {
            return Ok(());
        };
        if self.restart(follower, &second)? {
            self.audit();
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
        let Some(leader) = agreed(&self.cluster) else {
            return Ok(None);
        };
        let term = self
            .cluster
            .controller(leader)
            .map_or(Term::ZERO, |ctl| ctl.status().term);
        self.facts.elected = Some(Elected {
            node: leader,
            term,
            at: self.cluster.now(),
        });
        Ok(Some(leader))
    }

    /// Step 2: a PUT through `leader`, a first execution that applies on every node.
    fn put(&mut self, leader: NodeId, key: &Key, value: &Value) -> Result<bool, ClusterError> {
        let attempt = self.submit(leader, write(1, key, value))?;
        let Some(response) = self.answer(attempt, 5 * SECOND)? else {
            self.fail("the PUT got no response within 5 virtual seconds");
            return Ok(false);
        };
        let Some(index) = first_execution(&response) else {
            self.fail(&format!(
                "the PUT was answered {response:?}, not a first execution"
            ));
            return Ok(false);
        };
        let everywhere = self.until(MS, 5 * SECOND, |cluster| {
            cluster
                .nodes()
                .all(|id| held(cluster, id, key).as_ref() == Some(value))
        })?;
        if !everywhere {
            self.fail("the PUT did not apply on every node within 5 virtual seconds");
            return Ok(false);
        }
        self.facts.put = Some(Put { index });
        Ok(true)
    }

    /// Step 3: a GET from `leader` reads `value` back.
    fn get(&mut self, leader: NodeId, key: &Key, value: &Value) -> Result<bool, ClusterError> {
        let attempt = self.submit(leader, ClientOp::Get { key: key.clone() })?;
        let Some(response) = self.answer(attempt, 5 * SECOND)? else {
            self.fail("the GET got no response within 5 virtual seconds");
            return Ok(false);
        };
        let found = match &response {
            Response::Found { value: found, .. } => found == value,
            Response::Written { .. }
            | Response::Conflict { .. }
            | Response::NotFound { .. }
            | Response::Page(_)
            | Response::Status { .. }
            | Response::Refused(_) => false,
        };
        if !found {
            self.fail(&format!(
                "the GET was answered {response:?}, not the value written"
            ));
            return Ok(false);
        }
        self.facts.read_back = true;
        Ok(true)
    }

    /// Step 4: a second PUT through `leader`; a follower crashes while its journal holds
    /// unsynced bytes. Returns the follower.
    fn crash(
        &mut self,
        leader: NodeId,
        (key, value): &(Key, Value),
    ) -> Result<Option<NodeId>, ClusterError> {
        let Some(follower) = self.cluster.nodes().find(|id| *id != leader) else {
            self.fail("the cluster has no follower");
            return Ok(None);
        };
        self.submit(leader, write(2, key, value))?;
        let unsynced = self.until(50, SECOND, |cluster| unsynced(cluster, follower) > 0)?;
        if !unsynced {
            self.fail(&format!(
                "follower n{follower} never held unsynced bytes within a virtual second"
            ));
            return Ok(None);
        }
        let incarnation = self
            .cluster
            .incarnation(follower)
            .map_or(0, Incarnation::get);
        let now = self.cluster.now();
        self.cluster.crash_at(now, follower)?;
        self.cluster.run_until(now)?;
        let crashed = match self.cluster.crashes().last() {
            Some((
                node,
                _,
                Crash::Kept {
                    prefix,
                    region,
                    fill,
                    ..
                },
            )) if *node == follower => Crashed {
                node: follower,
                incarnation,
                unsynced_bytes: region.get(),
                kept_bytes: prefix.get(),
                damaged_tail: match fill {
                    Fill::None => false,
                    Fill::Zeros | Fill::Garbage => true,
                },
            },
            Some((_, _, Crash::Kept { .. } | Crash::NoJournal | Crash::FileLost)) | None => {
                self.fail(&format!("follower n{follower}'s crash kept no journal"));
                return Ok(None);
            }
        };
        self.facts.crashed = Some(crashed);
        Ok(Some(follower))
    }

    /// Step 5: `follower` restarts from what its disk kept and catches up: the second
    /// write applied, and its applied index and state digest the leader's.
    fn restart(
        &mut self,
        follower: NodeId,
        (key, value): &(Key, Value),
    ) -> Result<bool, ClusterError> {
        let at = VirtualTime::from_micros(self.cluster.now().as_micros() + 100 * MS);
        self.cluster.restart_at(at, follower)?;
        let caught_up = |cluster: &Cluster| {
            let (Some(ctl), Some(leader)) = (
                cluster.controller(follower),
                cluster
                    .leader()
                    .and_then(|leader| cluster.controller(leader)),
            ) else {
                return false;
            };
            let ready = match ctl.lifecycle() {
                Lifecycle::Ready => true,
                Lifecycle::Starting | Lifecycle::Recovering | Lifecycle::Failed => false,
            };
            ready
                && ctl.status().role == Role::Follower
                && held(cluster, follower, key).as_ref() == Some(value)
                && ctl.state().applied_index() == leader.state().applied_index()
                && ctl.state().digest() == leader.state().digest()
        };
        if !self.until(MS, 5 * SECOND, caught_up)? {
            self.fail(&format!(
                "follower n{follower} did not restart and catch up within 5 virtual seconds"
            ));
            return Ok(false);
        }
        let incarnation = self
            .cluster
            .incarnation(follower)
            .map_or(0, Incarnation::get);
        let applied = self
            .cluster
            .controller(follower)
            .map_or(LogIndex::ZERO, |ctl| ctl.state().applied_index());
        self.facts.restarted = Some(Restarted {
            node: follower,
            incarnation,
            applied,
        });
        Ok(true)
    }

    /// Step 6: every running node's mirror agrees with its disk.
    fn audit(&mut self) {
        match self.cluster.audit_mirrors() {
            Ok(()) => self.facts.mirrors_audited = true,
            Err(error) => self.fail(&format!("the mirror audit failed: {error}")),
        }
    }

    /// Submits `op` from the script's client to `node`, a virtual millisecond from now.
    fn submit(&mut self, node: NodeId, op: ClientOp) -> Result<AttemptId, ClusterError> {
        let at = VirtualTime::from_micros(self.cluster.now().as_micros() + MS);
        self.cluster.submit(at, CLIENT, node, op)
    }

    /// Runs until `attempt` is answered, at most `within` virtual microseconds: the first
    /// response to arrive.
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

/// A key of the script's, if it parses.
fn key(text: &str) -> Option<Key> {
    Key::parse(text.to_owned(), KeyValueLimits::MAXIMA).ok()
}

/// A value of the script's, if it parses.
fn value(text: &str) -> Option<Value> {
    Value::parse(text.to_owned(), KeyValueLimits::MAXIMA).ok()
}

/// A PUT of `value` at `key` with the request ID numbered `n`.
fn write(n: u8, key: &Key, value: &Value) -> ClientOp {
    ClientOp::Write {
        request_id: RequestId::from_bytes([n; 16]),
        mutation: Mutation::Put {
            key: key.clone(),
            value: value.clone(),
        },
    }
}

/// The index of a write's first execution, if `response` reports one.
fn first_execution(response: &Response) -> Option<LogIndex> {
    match response {
        Response::Written {
            result, replayed, ..
        } => (!replayed).then_some(result.index()),
        Response::Conflict { .. }
        | Response::Found { .. }
        | Response::NotFound { .. }
        | Response::Page(_)
        | Response::Status { .. }
        | Response::Refused(_) => None,
    }
}

/// The value `node` holds at `key`, if it runs and holds one.
fn held(cluster: &Cluster, node: NodeId, key: &Key) -> Option<Value> {
    cluster
        .controller(node)?
        .state()
        .get(key)
        .map(|stored| stored.value().clone())
}

/// The bytes `node`'s journal holds that its disk has not made durable yet.
fn unsynced(cluster: &Cluster, node: NodeId) -> u64 {
    let Some(disk) = cluster.disk(node) else {
        return 0;
    };
    let len = |bytes: Option<Vec<u8>>| bytes.map_or(0, |bytes| bytes.len());
    u64::try_from(len(disk.journal()).saturating_sub(len(disk.durable()))).unwrap_or(u64::MAX)
}
