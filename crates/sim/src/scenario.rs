//! Scenarios and scenario files (PLAN.md §10.2, §10.4, T-10, W37.2).
//!
//! A seed generates a **scenario**, which serialises to a human-readable **scenario file**.
//!
//! A scenario plus its seed fully determines a run: the seed still supplies the remaining
//! randomness (message delays, the unsynced bytes a crash keeps, the clients' retry timing),
//! every stream split from it (§10.2). [`Scenario::start`] builds the cluster and the clients
//! of a scenario, with its faults scheduled; the caller drives them (the run phases are
//! W37.3's).
//!
//! **The healthy envelope by construction (§9.4).** [`Scenario::generate`] draws faults only
//! as *episodes* that undo themselves before the chaos phase ends: a crash (a timed one, or an
//! armed crash point that fires on a state, at most once) is followed by its restart, which
//! also disarms an armed crash that has not fired, a pause by its resume, a partition or cut by a heal or mend, a network fault by
//! the healthy profile. At most two nodes are lost for good, so three voters are up, recovered
//! and mutually connected when chaos ends, on links that deliver within 50 ms. The workload
//! is at most [`MAX_CLIENTS`] logical writes outstanding, one per client. [`validate`] states
//! these bounds as rules and judges any scenario, drawn or read from a file, by replaying its
//! schedule against the cluster's own fault semantics; a violation is *scenario invalid*
//! (§10.16), which is distinct from a liveness failure of the system. Generation asserts it
//! (CD-7), so a drawn scenario cannot be invalid.
//!
//! **The file** is JSON, pretty-printed, schema [`SCHEMA`]. Times are virtual milliseconds
//! from the start of the run.
//!
//! ```text
//! { "schema": "keyval.scenario/v1", "seed": 7, "workload": "put_get", "chaos_ms": 6000,
//!   "swarm": { "control": false, "faults": ["pause"], "intensity": "medium" },
//!   "faults": [ { "at_ms": 900, "fault": "pause", "node": 2 },
//!               { "at_ms": 1700, "fault": "resume", "node": 2 } ],
//!   "plan": { "hot": 1, "keys": ["a", …],
//!             "clients": [ { "client": 0, "ops": [
//!               { "think_ms": 12, "op": "put", "request_id": "…", "key": "a", "value": "v1" },
//!               { "think_ms": 40, "op": "get", "key": "a" } ] } ] } }
//! ```
//!
//! **Scope of reproduction (T-10).** A file reproduces its seed's run when read by the same
//! build under the same configuration with the same driver: it records the plan, the swarm
//! and every scheduled step, not the configuration or the driver's own choices. An *armed*
//! crash point (`Cluster::arm`: before a sync, after a sync, before a reply) is a step like
//! any other (`"fault": "arm"`, with its `node`, `point` and `survival`): it fires on a state
//! and not at a time, at most once, and a restart of the node, running or not, disarms it, so
//! [`validate`] requires a restart after every arm. A file with armed crashes therefore
//! reproduces its run. Arming is not logged as a fault; the crash is, when it fires. Armed
//! crashes of overlapping episodes on one node can cancel one another (a restart disarms).
//!
//! A file is read back with [`Scenario::parse`], which checks every key and value against the
//! configured limits and every rule above, so a hand-edited file cannot smuggle in a state
//! the generator would never build (CD-3).

use std::collections::BTreeSet;
use std::fmt;

use serde_json::{Map, Value, json};
use types::{Config, Key, KeyValueLimits, NodeId, PageSize, RequestId, ScanRange, Value as Val};

use crate::clients::{
    ClientPlan, Clients, Intent, KEYS, MAX_CLIENTS, MAX_THINK_MS, OPS, Plan, Planned,
    Profile as Workload,
};
use crate::cluster::{Cluster, ClusterError};
use crate::disk::Survival;
use crate::entropy::{MasterSeed, SplitMix64, Subsystem};
use crate::nemesis::{CrashPoint, Fault, MAX_LOST, NemesisError, Partition};
use crate::net::{LinkProfile, NetConfig, Ppm, QueueBound};
use crate::scheduler::{EventBudget, VirtualTime};
use crate::swarm::{FaultKind, Intensity, Swarm};
use crate::trace::ClientId;

/// The identifier of a scenario file.
pub const SCHEMA: &str = "keyval.scenario/v1";

/// The voters of the cluster a scenario drives: nodes 1 to 5.
pub const VOTERS: u64 = 5;

/// No fault starts before this many virtual milliseconds, so the first election happens in
/// peace.
pub const LEAD_MS: u64 = 200;

/// The shortest and longest an episode lasts, in virtual milliseconds. The longest exceeds an
/// election timeout, so a paused or cut-off leader is deposed.
pub const EPISODE_MS: (u64, u64) = (300, 1_500);

/// The longest chaos phase a scenario may have, in virtual milliseconds: the harshest
/// intensity's. A longer one in a file is refused, so no time computed from it can overflow.
pub const MAX_CHAOS_MS: u64 = 10_000;

/// The most steps a scenario schedules, so a file cannot overrun the cluster's fault queue.
pub const MAX_STEPS: usize = 256;

/// The slowest message a healthy link delivers (§9.4), in virtual microseconds.
pub const HEALTHY_MAX_DELAY_US: u64 = 50_000;

/// How many times a network episode is placed before it is dropped for overlapping another.
const PLACEMENTS: usize = 8;

/// One virtual millisecond, in microseconds.
const MS: u64 = 1_000;

/// A link's settings, as numbers a file can hold; [`LinkSpec::profile`] builds the link.
///
/// Invariant: none by itself; [`LinkSpec::profile`] refuses a range whose shortest delay
/// exceeds its longest, and a probability above one million parts per million.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkSpec {
    /// The shortest delay, in virtual microseconds.
    pub min_delay_us: u64,
    /// The longest delay, in virtual microseconds.
    pub max_delay_us: u64,
    /// The chance a message is lost, in parts per million.
    pub loss_ppm: u32,
    /// The chance a message is delivered twice, in parts per million.
    pub duplicate_ppm: u32,
}

/// The peer links of a healthy run: delay 0.2 to 2 ms, nothing lost or duplicated.
pub const HEALTHY_PEER: LinkSpec = LinkSpec {
    min_delay_us: 200,
    max_delay_us: 2_000,
    loss_ppm: 0,
    duplicate_ppm: 0,
};

/// The client paths of a healthy run: 0.2 ms, nothing lost or duplicated.
pub const HEALTHY_CLIENT: LinkSpec = LinkSpec {
    min_delay_us: 200,
    max_delay_us: 200,
    loss_ppm: 0,
    duplicate_ppm: 0,
};

impl LinkSpec {
    /// The link these settings describe, or `None` for a backwards delay range or a
    /// probability above one.
    #[must_use]
    pub fn profile(self) -> Option<LinkProfile> {
        LinkProfile::new(
            self.min_delay_us,
            self.max_delay_us,
            Ppm::new(self.loss_ppm)?,
            Ppm::new(self.duplicate_ppm)?,
        )
    }

    /// Whether a link with these settings is healthy (§9.4): it loses nothing and delivers
    /// within [`HEALTHY_MAX_DELAY_US`].
    #[must_use]
    pub const fn is_healthy(self) -> bool {
        self.loss_ppm == 0 && self.max_delay_us <= HEALTHY_MAX_DELAY_US
    }

    fn to_json(self) -> Value {
        json!({
            "min_delay_us": self.min_delay_us,
            "max_delay_us": self.max_delay_us,
            "loss_ppm": self.loss_ppm,
            "duplicate_ppm": self.duplicate_ppm,
        })
    }

    fn from_json(value: &Value, what: &str) -> Result<Self, ScenarioError> {
        let ppm = |field: &str| {
            u32::try_from(u64_of(value, field, what)?)
                .map_err(|_| malformed(format!("{what}.{field} is too large")))
        };
        only_fields(
            value,
            &["min_delay_us", "max_delay_us", "loss_ppm", "duplicate_ppm"],
            what,
        )?;
        Ok(Self {
            min_delay_us: u64_of(value, "min_delay_us", what)?,
            max_delay_us: u64_of(value, "max_delay_us", what)?,
            loss_ppm: ppm("loss_ppm")?,
            duplicate_ppm: ppm("duplicate_ppm")?,
        })
    }
}

/// One scheduled fault or recovery, as a scenario file spells it (§9.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// `node` crashes, keeping `survival` of its unsynced bytes.
    Crash {
        /// The node.
        node: NodeId,
        /// What its disk keeps.
        survival: Survival,
    },
    /// `node` is armed to crash at `point`, keeping `survival` of its unsynced bytes: the crash
    /// fires when the node next reaches that point, once, or never (a crash, a restart or the
    /// heal disarms it). It does nothing to a node that is down.
    Arm {
        /// The node.
        node: NodeId,
        /// Where the crash fires.
        point: CrashPoint,
        /// What its disk keeps.
        survival: Survival,
    },
    /// Every running node crashes at once.
    CrashAll {
        /// What each disk keeps.
        survival: Survival,
    },
    /// `node` restarts from the bytes its disk kept.
    Restart {
        /// The node.
        node: NodeId,
    },
    /// `node` is lost for good.
    Lose {
        /// The node.
        node: NodeId,
    },
    /// `node` stops taking turns.
    Pause {
        /// The node.
        node: NodeId,
    },
    /// `node` runs again.
    Resume {
        /// The node.
        node: NodeId,
    },
    /// The voters split into groups that reach only their own members.
    Partition {
        /// The disjoint groups.
        groups: Vec<Vec<NodeId>>,
    },
    /// These directed links `(from, to)` are cut.
    Cuts {
        /// The links.
        links: Vec<(NodeId, NodeId)>,
    },
    /// The directed link `from → to` is cut.
    Cut {
        /// The sender.
        from: NodeId,
        /// The receiver.
        to: NodeId,
    },
    /// The directed link `from → to` is restored.
    Mend {
        /// The sender.
        from: NodeId,
        /// The receiver.
        to: NodeId,
    },
    /// Every link is restored.
    Heal,
    /// The peer and client path profiles change.
    Net {
        /// Node-to-node links.
        peer: LinkSpec,
        /// Paths to and from clients.
        client: LinkSpec,
    },
}

/// Why a [`Step`] cannot become a [`Fault`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepError {
    /// The partition or cut is malformed.
    Nemesis(NemesisError),
    /// A link's delay range is backwards or a probability exceeds one.
    Link,
}

impl Step {
    /// The simulator fault this step schedules.
    ///
    /// # Errors
    ///
    /// [`StepError`] for a malformed partition, cut or link.
    pub fn fault(&self) -> Result<Fault, StepError> {
        let nemesis = StepError::Nemesis;
        Ok(match self {
            Self::Crash { node, survival } => Fault::Crash {
                node: *node,
                survival: *survival,
            },
            Self::Arm {
                node,
                point,
                survival,
            } => Fault::Arm {
                node: *node,
                point: *point,
                survival: *survival,
            },
            Self::CrashAll { survival } => Fault::CrashAll {
                survival: *survival,
            },
            Self::Restart { node } => Fault::Restart { node: *node },
            Self::Lose { node } => Fault::Lose { node: *node },
            Self::Pause { node } => Fault::Pause { node: *node },
            Self::Resume { node } => Fault::Resume { node: *node },
            Self::Partition { groups } => {
                let groups: Vec<&[NodeId]> = groups.iter().map(Vec::as_slice).collect();
                Fault::Partition(Partition::symmetric(&groups).map_err(nemesis)?)
            }
            Self::Cuts { links } => Fault::Partition(Partition::cuts(links).map_err(nemesis)?),
            Self::Cut { from, to } => Fault::Cut {
                from: *from,
                to: *to,
            },
            Self::Mend { from, to } => Fault::Mend {
                from: *from,
                to: *to,
            },
            Self::Heal => Fault::Heal,
            Self::Net { peer, client } => Fault::Profiles {
                peer: peer.profile().ok_or(StepError::Link)?,
                client: client.profile().ok_or(StepError::Link)?,
            },
        })
    }

    /// The §9.1 fault type this step *injects*, or `None` for a recovery (restart, resume,
    /// mend, heal, or the healthy network profile).
    #[must_use]
    pub const fn kind(&self) -> Option<FaultKind> {
        match self {
            Self::Crash { .. } | Self::Arm { .. } => Some(FaultKind::CrashRestart),
            Self::CrashAll { .. } => Some(FaultKind::AllNodeCrash),
            Self::Lose { .. } => Some(FaultKind::PermanentLoss),
            Self::Pause { .. } => Some(FaultKind::Pause),
            Self::Partition { .. } => Some(FaultKind::SymmetricPartition),
            Self::Cuts { .. } | Self::Cut { .. } => Some(FaultKind::AsymmetricPartition),
            Self::Net { peer, client } => {
                if peer.loss_ppm > 0 || client.loss_ppm > 0 {
                    Some(FaultKind::MessageLoss)
                } else if peer.duplicate_ppm > 0 || client.duplicate_ppm > 0 {
                    Some(FaultKind::Duplication)
                } else if peer.max_delay_us > HEALTHY_PEER.max_delay_us
                    || client.max_delay_us > HEALTHY_CLIENT.max_delay_us
                {
                    Some(FaultKind::DelayReorder)
                } else {
                    None
                }
            }
            Self::Restart { .. } | Self::Resume { .. } | Self::Mend { .. } | Self::Heal => None,
        }
    }
}

/// A step and when it happens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timed {
    /// Virtual milliseconds from the start of the run.
    pub at_ms: u64,
    /// What happens.
    pub step: Step,
}

/// Why a scenario is outside the healthy envelope or otherwise unusable (§9.4, §10.16).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    /// The control swarm schedules a fault.
    ControlInjects {
        /// When.
        at_ms: u64,
    },
    /// A step injects a fault type the swarm has not enabled.
    DisabledFault {
        /// When.
        at_ms: u64,
        /// The fault type.
        kind: FaultKind,
    },
    /// The schedule is not in time order.
    OutOfOrder {
        /// The step that is earlier than the one before it.
        at_ms: u64,
    },
    /// A step is at or after the end of the chaos phase, leaving no margin to settle.
    AfterChaos {
        /// When.
        at_ms: u64,
        /// The chaos phase's end.
        chaos_ms: u64,
    },
    /// A step names a node that is not one of the five voters.
    UnknownNode {
        /// The node.
        node: u64,
    },
    /// More than [`MAX_LOST`] nodes are lost.
    TooManyLost {
        /// How many.
        lost: usize,
    },
    /// A step cannot be built.
    BadStep {
        /// When.
        at_ms: u64,
        /// Why.
        error: StepError,
    },
    /// A surviving node is still down when chaos ends.
    NodeLeftDown {
        /// The node.
        node: u64,
    },
    /// A surviving node is still paused when chaos ends.
    NodeLeftPaused {
        /// The node.
        node: u64,
    },
    /// Directed links are still cut when chaos ends.
    LinksLeftCut {
        /// How many.
        links: usize,
    },
    /// The links are not healthy when chaos ends (§9.4: within 50 ms, nothing lost).
    NetworkNotHealed,
    /// The chaos phase is longer than [`MAX_CHAOS_MS`].
    ChaosTooLong {
        /// Its length.
        chaos_ms: u64,
    },
    /// More than [`MAX_STEPS`] steps.
    TooManySteps {
        /// How many.
        steps: usize,
    },
    /// Two steps at one instant touch the same node, link set or network profile and do not
    /// commute: the scheduler orders simultaneous events by its own tie-break, not by the
    /// file's order, so the outcome would be undefined.
    SimultaneousSteps {
        /// When.
        at_ms: u64,
    },
    /// A plan that breaks the shape clients generate: no clients, a client without 6 to 20
    /// operations, a think time over the limit, or an operation on a key outside the plan.
    BadPlan {
        /// Which rule.
        rule: &'static str,
    },
    /// More clients than [`MAX_CLIENTS`], so more writes outstanding than the envelope allows.
    TooManyClients {
        /// How many.
        clients: usize,
    },
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "scenario invalid (§9.4): {self:?}")
    }
}

/// Why a scenario could not be built, read or started.
#[derive(Debug)]
pub enum ScenarioError {
    /// A scenario file that is not a scenario: what is wrong.
    Malformed(String),
    /// A scenario outside the healthy envelope (§10.16).
    Invalid(Violation),
    /// The cluster refused the scenario's setup.
    Setup(&'static str),
    /// The cluster failed while the scenario was being scheduled.
    Cluster(ClusterError),
}

impl fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "malformed scenario: {what}"),
            Self::Invalid(violation) => violation.fmt(f),
            Self::Setup(what) => write!(f, "scenario setup: {what}"),
            Self::Cluster(error) => write!(f, "scenario scheduling failed: {error:?}"),
        }
    }
}

impl std::error::Error for ScenarioError {}

fn malformed(what: impl Into<String>) -> ScenarioError {
    ScenarioError::Malformed(what.into())
}

/// Judges a scenario's schedule and workload against the healthy envelope (§9.4).
///
/// The schedule is replayed with the cluster's own fault semantics: a crash takes a running
/// node down and clears its pause, a restart brings a down node up unless it is lost, a pause
/// stalls a running node, a lost node never returns. When chaos ends every surviving node
/// must be running and unpaused, no link may be cut and both network paths must be healthy,
/// and at most [`MAX_LOST`] nodes may be lost: at least three voters are then up, recovered
/// and mutually connected.
///
/// # Errors
///
/// The first [`Violation`].
pub fn validate(
    swarm: Swarm,
    chaos_ms: u64,
    faults: &[Timed],
    plan: &Plan,
) -> Result<(), Violation> {
    if chaos_ms > MAX_CHAOS_MS {
        return Err(Violation::ChaosTooLong { chaos_ms });
    }
    if faults.len() > MAX_STEPS {
        return Err(Violation::TooManySteps {
            steps: faults.len(),
        });
    }
    check_plan(plan)?;
    let mut replay = Replay::new();
    let mut previous = 0;
    for (at, timed) in faults.iter().enumerate() {
        let at_ms = timed.at_ms;
        if at_ms < previous {
            return Err(Violation::OutOfOrder { at_ms });
        }
        previous = at_ms;
        if at_ms >= chaos_ms {
            return Err(Violation::AfterChaos { at_ms, chaos_ms });
        }
        if faults[..at]
            .iter()
            .rev()
            .take_while(|earlier| earlier.at_ms == at_ms)
            .any(|earlier| conflict(&earlier.step, &timed.step))
        {
            return Err(Violation::SimultaneousSteps { at_ms });
        }
        timed
            .step
            .fault()
            .map_err(|error| Violation::BadStep { at_ms, error })?;
        check_enabled(swarm, at_ms, &timed.step)?;
        replay.apply(&timed.step)?;
    }
    replay.settled()
}

/// The plan's shape: what [`Plan::generate`] builds (its documented invariants).
fn check_plan(plan: &Plan) -> Result<(), Violation> {
    let bad = |rule| Err(Violation::BadPlan { rule });
    let clients = usize::try_from(MAX_CLIENTS).unwrap_or(usize::MAX);
    if plan.clients.is_empty() {
        return bad("a plan has at least one client");
    }
    if plan.clients.len() > clients {
        return Err(Violation::TooManyClients {
            clients: plan.clients.len(),
        });
    }
    let ops = usize::try_from(OPS.0).unwrap_or(0)..=usize::try_from(OPS.1).unwrap_or(usize::MAX);
    for client in &plan.clients {
        if !ops.contains(&client.ops.len()) {
            return bad("a client has 6 to 20 operations");
        }
        for planned in &client.ops {
            if planned.think_ms > MAX_THINK_MS {
                return bad("a think time is at most 300 ms");
            }
            let key = match &planned.intent {
                Intent::Put { key, .. }
                | Intent::Delete { key, .. }
                | Intent::Cas { key, .. }
                | Intent::Get { key } => Some(key),
                Intent::Scan { .. } => None,
            };
            if key.is_some_and(|key| !plan.keys.contains(key)) {
                return bad("every operation's key is one of the plan's keys");
            }
        }
    }
    Ok(())
}

/// What a step acts on, for judging whether two simultaneous steps can disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Resource {
    Node(NodeId),
    Links,
    Net,
}

impl Step {
    fn resources(&self) -> Vec<Resource> {
        match self {
            Self::Crash { node, .. }
            | Self::Arm { node, .. }
            | Self::Restart { node }
            | Self::Lose { node }
            | Self::Pause { node }
            | Self::Resume { node } => vec![Resource::Node(*node)],
            Self::CrashAll { .. } => (1..=VOTERS)
                .filter_map(|raw| NodeId::new(raw).ok())
                .map(Resource::Node)
                .collect(),
            Self::Partition { .. }
            | Self::Cuts { .. }
            | Self::Cut { .. }
            | Self::Mend { .. }
            | Self::Heal => vec![Resource::Links],
            Self::Net { .. } => vec![Resource::Net],
        }
    }

    /// A recovery: restart, resume, mend, heal or the healthy profile. (Exactly the healthy profile: a mild, still-healthy
    /// one is a change like any other.) Recoveries commute
    /// with each other in any order, so two of them may share an instant.
    fn is_recovery(&self) -> bool {
        match self {
            Self::Restart { .. } | Self::Resume { .. } | Self::Mend { .. } | Self::Heal => true,
            Self::Net { peer, client } => *peer == HEALTHY_PEER && *client == HEALTHY_CLIENT,
            Self::Crash { .. }
            | Self::Arm { .. }
            | Self::CrashAll { .. }
            | Self::Lose { .. }
            | Self::Pause { .. }
            | Self::Partition { .. }
            | Self::Cuts { .. }
            | Self::Cut { .. } => false,
        }
    }
}

/// Whether two steps at one instant could give different results in different orders: they
/// touch a common resource and are not both recoveries.
fn conflict(first: &Step, second: &Step) -> bool {
    if first.is_recovery() && second.is_recovery() {
        return false;
    }
    let mine = first.resources();
    second
        .resources()
        .iter()
        .any(|resource| mine.contains(resource))
}

/// Whether `swarm` allows `step`: the control allows nothing, a faulty swarm only the fault
/// types it enables (recoveries are always allowed to a faulty swarm).
const fn check_enabled(swarm: Swarm, at_ms: u64, step: &Step) -> Result<(), Violation> {
    match (swarm, step.kind()) {
        (Swarm::Control, _) => Err(Violation::ControlInjects { at_ms }),
        (Swarm::Faulty { faults, .. }, Some(kind)) if !faults.contains(kind) => {
            Err(Violation::DisabledFault { at_ms, kind })
        }
        (Swarm::Faulty { .. }, _) => Ok(()),
    }
}

/// The cluster's state while a schedule is replayed against it.
struct Replay {
    nodes: Vec<NodeId>,
    down: BTreeSet<NodeId>,
    /// Nodes with an armed crash that a restart has not yet disarmed: it may have fired, so
    /// the node must be restarted before chaos ends.
    armed: BTreeSet<NodeId>,
    paused: BTreeSet<NodeId>,
    lost: BTreeSet<NodeId>,
    cut: BTreeSet<(NodeId, NodeId)>,
    peer: LinkSpec,
    client: LinkSpec,
}

impl Replay {
    fn new() -> Self {
        Self {
            nodes: (1..=VOTERS)
                .filter_map(|raw| NodeId::new(raw).ok())
                .collect(),
            down: BTreeSet::new(),
            armed: BTreeSet::new(),
            paused: BTreeSet::new(),
            lost: BTreeSet::new(),
            cut: BTreeSet::new(),
            peer: HEALTHY_PEER,
            client: HEALTHY_CLIENT,
        }
    }

    fn known(&self, node: NodeId) -> Result<NodeId, Violation> {
        if self.nodes.contains(&node) {
            Ok(node)
        } else {
            Err(Violation::UnknownNode { node: node.get() })
        }
    }

    fn link(&self, from: NodeId, to: NodeId) -> Result<(NodeId, NodeId), Violation> {
        Ok((self.known(from)?, self.known(to)?))
    }

    fn apply(&mut self, step: &Step) -> Result<(), Violation> {
        match step {
            Step::Crash { node, .. } => {
                let node = self.known(*node)?;
                if !self.down.contains(&node) && !self.lost.contains(&node) {
                    self.down.insert(node);
                    self.paused.remove(&node);
                }
                self.armed.remove(&node);
            }
            // An armed crash may fire at any later time or never: the node is left armed
            // until a restart disarms it, and chaos may not end while it is.
            Step::Arm { node, .. } => {
                let node = self.known(*node)?;
                if !self.down.contains(&node) && !self.lost.contains(&node) {
                    self.armed.insert(node);
                }
            }
            Step::CrashAll { .. } => {
                for node in &self.nodes {
                    if !self.lost.contains(node) {
                        self.down.insert(*node);
                    }
                }
                self.paused.clear();
                self.armed.clear();
            }
            Step::Restart { node } => {
                let node = self.known(*node)?;
                if !self.lost.contains(&node) {
                    self.down.remove(&node);
                }
                self.armed.remove(&node);
            }
            Step::Lose { node } => {
                let node = self.known(*node)?;
                self.lost.insert(node);
                self.down.insert(node);
                self.armed.remove(&node);
                self.paused.remove(&node);
                if self.lost.len() > MAX_LOST {
                    return Err(Violation::TooManyLost {
                        lost: self.lost.len(),
                    });
                }
            }
            Step::Pause { node } => {
                let node = self.known(*node)?;
                if !self.down.contains(&node) && !self.lost.contains(&node) {
                    self.paused.insert(node);
                }
            }
            Step::Resume { node } => {
                let node = self.known(*node)?;
                self.paused.remove(&node);
            }
            Step::Partition { groups } => self.partition(groups)?,
            Step::Cuts { links } => {
                for (from, to) in links {
                    let link = self.link(*from, *to)?;
                    self.cut.insert(link);
                }
            }
            Step::Cut { from, to } => {
                let link = self.link(*from, *to)?;
                self.cut.insert(link);
            }
            Step::Mend { from, to } => {
                let link = self.link(*from, *to)?;
                self.cut.remove(&link);
            }
            Step::Heal => self.cut.clear(),
            Step::Net { peer, client } => {
                self.peer = *peer;
                self.client = *client;
            }
        }
        Ok(())
    }

    /// Every directed link between two different groups is cut.
    fn partition(&mut self, groups: &[Vec<NodeId>]) -> Result<(), Violation> {
        for group in groups {
            for node in group {
                self.known(*node)?;
            }
        }
        for (i, from_group) in groups.iter().enumerate() {
            for (j, to_group) in groups.iter().enumerate() {
                if i != j {
                    for from in from_group {
                        for to in to_group {
                            self.cut.insert((*from, *to));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// The state when chaos ends must be healthy.
    fn settled(&self) -> Result<(), Violation> {
        if let Some(node) = self.nodes.iter().find(|node| {
            (self.down.contains(node) || self.armed.contains(node)) && !self.lost.contains(node)
        }) {
            return Err(Violation::NodeLeftDown { node: node.get() });
        }
        if let Some(node) = self.paused.iter().next() {
            return Err(Violation::NodeLeftPaused { node: node.get() });
        }
        if !self.cut.is_empty() {
            return Err(Violation::LinksLeftCut {
                links: self.cut.len(),
            });
        }
        if !self.peer.is_healthy() || !self.client.is_healthy() {
            return Err(Violation::NetworkNotHealed);
        }
        Ok(())
    }
}
/// A scenario: everything a run is made of except the seed's remaining randomness.
///
/// Invariant: [`validate`] accepts its parts, and its plan's client numbers are its positions
/// (CD-2): the constructors [`Self::generate`], [`Self::control`], [`Self::from_parts`] and
/// [`Self::parse`] refuse anything else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scenario {
    seed: u64,
    swarm: Swarm,
    workload: Workload,
    chaos_ms: u64,
    faults: Vec<Timed>,
    plan: Plan,
}

fn below(rng: &mut SplitMix64, bound: u64) -> u64 {
    rng.next_u64() % bound.max(1)
}

fn pick(rng: &mut SplitMix64, len: usize) -> usize {
    usize::try_from(below(rng, u64::try_from(len).unwrap_or(u64::MAX))).unwrap_or(0)
}

/// `nodes` in an order drawn from `rng` (Fisher–Yates).
fn shuffled(rng: &mut SplitMix64, mut nodes: Vec<NodeId>) -> Vec<NodeId> {
    for i in (1..nodes.len()).rev() {
        nodes.swap(i, pick(rng, i + 1));
    }
    nodes
}

const SURVIVALS: [Survival; 5] = [
    Survival::Seeded,
    Survival::Nothing,
    Survival::Partial,
    Survival::PartialThenGarbage,
    Survival::All,
];

impl Scenario {
    /// The scenario of `seed`: a swarm drawn from the seed's fault stream, a fault schedule
    /// for it and the seed's workload. The same seed gives the same scenario (T-10).
    #[must_use]
    pub fn generate(seed: MasterSeed, limits: KeyValueLimits, workload: Workload) -> Self {
        let mut rng = seed.stream(Subsystem::Faults);
        let swarm = Swarm::draw(&mut rng);
        Self::build(seed, swarm, &mut rng, limits, workload)
    }

    /// The no-fault control of `seed` (§10.4): the seed's workload and nothing injected.
    #[must_use]
    pub fn control(seed: MasterSeed, limits: KeyValueLimits, workload: Workload) -> Self {
        let mut rng = seed.stream(Subsystem::Faults);
        Self::build(seed, Swarm::Control, &mut rng, limits, workload)
    }

    fn build(
        seed: MasterSeed,
        swarm: Swarm,
        rng: &mut SplitMix64,
        limits: KeyValueLimits,
        workload: Workload,
    ) -> Self {
        let plan = Plan::generate(&mut seed.stream(Subsystem::Clients), limits, workload);
        let (chaos_ms, mut faults) = match swarm {
            Swarm::Control => (Intensity::Low.chaos_ms(), Vec::new()),
            Swarm::Faulty { faults, intensity } => {
                let chaos_ms = intensity.chaos_ms();
                (chaos_ms, schedule(rng, faults.kinds(), intensity, chaos_ms))
            }
        };
        separate(&mut faults);
        let verdict = validate(swarm, chaos_ms, &faults, &plan);
        types::invariant!(
            verdict.is_ok(),
            "SWARM-ENVELOPE",
            "seed {} swarm {swarm:?}: a drawn scenario must be inside the healthy envelope; {verdict:?}",
            seed.get()
        );
        Self {
            seed: seed.get(),
            swarm,
            workload,
            chaos_ms,
            faults,
            plan,
        }
    }

    /// A scenario of these parts.
    ///
    /// # Errors
    ///
    /// [`ScenarioError::Invalid`] if [`validate`] refuses them; [`ScenarioError::Malformed`]
    /// if a client's number is not its position in the plan.
    pub fn from_parts(
        seed: u64,
        swarm: Swarm,
        workload: Workload,
        chaos_ms: u64,
        faults: Vec<Timed>,
        plan: Plan,
    ) -> Result<Self, ScenarioError> {
        for (at, client) in plan.clients.iter().enumerate() {
            if usize::try_from(client.client.get()).ok() != Some(at) {
                return Err(malformed(format!(
                    "client {} is at position {at} of the plan; clients are numbered from 0 in order",
                    client.client.get()
                )));
            }
        }
        validate(swarm, chaos_ms, &faults, &plan).map_err(ScenarioError::Invalid)?;
        Ok(Self {
            seed,
            swarm,
            workload,
            chaos_ms,
            faults,
            plan,
        })
    }

    /// The master seed.
    #[must_use]
    pub const fn seed(&self) -> MasterSeed {
        MasterSeed::new(self.seed)
    }

    /// The swarm configuration.
    #[must_use]
    pub const fn swarm(&self) -> Swarm {
        self.swarm
    }

    /// The operation mix the plan was drawn under.
    #[must_use]
    pub const fn workload(&self) -> Workload {
        self.workload
    }

    /// The chaos phase's length, in virtual milliseconds.
    #[must_use]
    pub const fn chaos_ms(&self) -> u64 {
        self.chaos_ms
    }

    /// The fault schedule, in time order.
    #[must_use]
    pub fn faults(&self) -> &[Timed] {
        &self.faults
    }

    /// The clients' operation plan.
    #[must_use]
    pub const fn plan(&self) -> &Plan {
        &self.plan
    }

    /// The end of the chaos phase.
    #[must_use]
    pub const fn chaos_end(&self) -> VirtualTime {
        VirtualTime::from_micros(self.chaos_ms * MS)
    }

    /// The manifest's record of the scenario (§10.2, §10.4): seed, scenario version, swarm
    /// configuration, workload, chaos length, and how many faults and operations it holds.
    #[must_use]
    pub fn manifest(&self) -> Value {
        json!({
            "seed": self.seed,
            "scenario_schema": SCHEMA,
            "swarm": self.swarm.manifest(),
            "workload": workload_name(self.workload),
            "chaos_ms": self.chaos_ms,
            "scheduled_steps": self.faults.len(),
            "planned_operations": self.plan.clients.iter().map(|client| client.ops.len()).sum::<usize>(),
        })
    }

    /// The network a scenario's run starts with: healthy links.
    ///
    /// # Errors
    ///
    /// [`ScenarioError::Setup`] only if the healthy profiles were malformed.
    pub fn net_config(config: &Config) -> Result<NetConfig, ScenarioError> {
        Ok(NetConfig {
            peer: HEALTHY_PEER.profile().ok_or(ScenarioError::Setup(
                "the healthy peer profile is malformed",
            ))?,
            client: HEALTHY_CLIENT.profile().ok_or(ScenarioError::Setup(
                "the healthy client profile is malformed",
            ))?,
            queue: QueueBound::of(config),
            max_in_flight: EventBudget::DEFAULT,
        })
    }

    /// The cluster and clients of this scenario, with every scheduled step queued. The caller
    /// drives them; nothing has run yet.
    ///
    /// # Errors
    ///
    /// [`ScenarioError::Setup`] if `config` does not have voters 1 to 5;
    /// [`ScenarioError::Cluster`] if the cluster cannot be built or a step cannot be queued.
    pub fn start(&self, config: &Config) -> Result<(Cluster, Clients), ScenarioError> {
        let (mut cluster, clients) = self.begin(config)?;
        self.inject_faults(&mut cluster, VirtualTime::ZERO)?;
        Ok((cluster, clients))
    }

    /// The cluster and clients of this scenario with no fault scheduled, so a driver can run a
    /// baseline first and schedule the faults with [`Self::inject_faults`] when chaos begins
    /// (W37.3).
    ///
    /// # Errors
    ///
    /// As [`Self::start`], except for the steps.
    pub fn begin(&self, config: &Config) -> Result<(Cluster, Clients), ScenarioError> {
        let voters: Vec<u64> = config.voters().map(NodeId::get).collect();
        if voters != (1..=VOTERS).collect::<Vec<_>>() {
            return Err(ScenarioError::Setup("the scenario drives voters 1 to 5"));
        }
        let seed = self.seed();
        let cluster = Cluster::new(
            config.clone(),
            seed,
            Self::net_config(config)?,
            EventBudget::DEFAULT,
        )
        .map_err(ScenarioError::Cluster)?;
        let clients = Clients::new(self.plan.clone(), config, seed.stream(Subsystem::Clients));
        Ok((cluster, clients))
    }

    /// Schedules every step of the schedule on `cluster`, each `offset` after its own time:
    /// chaos begins at `offset`.
    ///
    /// # Errors
    ///
    /// [`ScenarioError::Invalid`] for a step that is not a fault; [`ScenarioError::Cluster`]
    /// if a step cannot be queued (also for a time before the cluster's now).
    pub fn inject_faults(
        &self,
        cluster: &mut Cluster,
        offset: VirtualTime,
    ) -> Result<(), ScenarioError> {
        for timed in &self.faults {
            let fault = timed.step.fault().map_err(|error| {
                ScenarioError::Invalid(Violation::BadStep {
                    at_ms: timed.at_ms,
                    error,
                })
            })?;
            cluster
                .inject_at(
                    VirtualTime::from_micros(offset.as_micros() + timed.at_ms * MS),
                    fault,
                )
                .map_err(ScenarioError::Cluster)?;
        }
        Ok(())
    }

    /// The scenario file's text: pretty-printed JSON, ending in a newline.
    #[must_use]
    pub fn to_text(&self) -> String {
        format!("{:#}\n", self.to_json())
    }

    /// The scenario file as a JSON value.
    #[must_use]
    pub fn to_json(&self) -> Value {
        json!({
            "schema": SCHEMA,
            "seed": self.seed,
            "workload": workload_name(self.workload),
            "chaos_ms": self.chaos_ms,
            "swarm": self.swarm.manifest(),
            "faults": self.faults.iter().map(timed_to_json).collect::<Vec<_>>(),
            "plan": plan_to_json(&self.plan),
        })
    }

    /// Reads a scenario file, checking every key and value against `limits` and every rule of
    /// [`validate`].
    ///
    /// # Errors
    ///
    /// [`ScenarioError::Malformed`] naming what is wrong with the text, or
    /// [`ScenarioError::Invalid`] for a scenario outside the healthy envelope.
    pub fn parse(text: &str, limits: KeyValueLimits) -> Result<Self, ScenarioError> {
        let value: Value =
            serde_json::from_str(text).map_err(|error| malformed(format!("not JSON: {error}")))?;
        only_fields(
            &value,
            &[
                "schema", "seed", "workload", "chaos_ms", "swarm", "faults", "plan",
            ],
            "scenario",
        )?;
        if value["schema"] != SCHEMA {
            return Err(malformed(format!("schema must be {SCHEMA:?}")));
        }
        let workload = match str_of(&value, "workload", "scenario")? {
            "put_get" => Workload::PutGet,
            "mixed" => Workload::Mixed,
            other => return Err(malformed(format!("unknown workload {other:?}"))),
        };
        let swarm = Swarm::from_json(&value["swarm"]).map_err(malformed)?;
        let faults = array_of(&value, "faults", "scenario")?
            .iter()
            .map(timed_from_json)
            .collect::<Result<Vec<_>, _>>()?;
        let plan = plan_from_json(&value["plan"], limits)?;
        Self::from_parts(
            u64_of(&value, "seed", "scenario")?,
            swarm,
            workload,
            u64_of(&value, "chaos_ms", "scenario")?,
            faults,
            plan,
        )
    }
}

/// The name of a workload in files and manifests.
const fn workload_name(workload: Workload) -> &'static str {
    match workload {
        Workload::PutGet => "put_get",
        Workload::Mixed => "mixed",
    }
}

/// Sorts the steps by time and moves a step that would share an instant with a conflicting
/// one a millisecond earlier, until none do: the scheduler orders simultaneous events by its
/// own tie-break, so a schedule must not depend on it (see [`conflict`]). Only a step that is
/// not a recovery moves, so an episode only ever grows.
fn separate(steps: &mut [Timed]) {
    loop {
        steps.sort_by_key(|timed| timed.at_ms);
        let tied = (1..steps.len()).find_map(|later| {
            (0..later).find_map(|earlier| {
                (steps[earlier].at_ms == steps[later].at_ms
                    && conflict(&steps[earlier].step, &steps[later].step))
                .then_some((earlier, later))
            })
        });
        let Some((earlier, later)) = tied else {
            return;
        };
        let moved = if steps[later].step.is_recovery() {
            earlier
        } else {
            later
        };
        types::invariant!(
            steps[moved].at_ms > LEAD_MS && !steps[moved].step.is_recovery(),
            "SWARM-SEPARATE",
            "only an episode's opening step moves, and not before the lead; step {:?} at {} ms",
            steps[moved].step,
            steps[moved].at_ms
        );
        steps[moved].at_ms -= 1;
    }
}

/// The fault schedule for `kinds` at `intensity`: each kind's episodes, drawn until the
/// intensity's count is reached, and the permanent losses.
fn schedule(
    rng: &mut SplitMix64,
    kinds: Vec<FaultKind>,
    intensity: Intensity,
    chaos_ms: u64,
) -> Vec<Timed> {
    let all: Vec<NodeId> = (1..=VOTERS)
        .filter_map(|raw| NodeId::new(raw).ok())
        .collect();
    let mut steps = Vec::new();
    // The nodes lost for good, chosen first: no other episode targets them.
    let mut lost = Vec::new();
    if kinds.contains(&FaultKind::PermanentLoss) {
        let count = if intensity == Intensity::Low {
            1
        } else {
            1 + pick(rng, MAX_LOST)
        };
        lost = shuffled(rng, all.clone()).into_iter().take(count).collect();
        for node in &lost {
            let at_ms = LEAD_MS + below(rng, chaos_ms - LEAD_MS);
            steps.push(Timed {
                at_ms,
                step: Step::Lose { node: *node },
            });
        }
    }
    let live: Vec<NodeId> = all
        .iter()
        .copied()
        .filter(|node| !lost.contains(node))
        .collect();
    let episodic: Vec<FaultKind> = kinds
        .into_iter()
        .filter(|kind| *kind != FaultKind::PermanentLoss)
        .collect();
    if episodic.is_empty() {
        return steps;
    }
    // Network episodes replace both profiles, so two that overlap would cut each other short:
    // each is placed clear of the others, or dropped after a few tries.
    let mut net_busy: Vec<(u64, u64)> = Vec::new();
    for _ in 0..intensity.episodes() {
        let kind = episodic[pick(rng, episodic.len())];
        let is_net = [
            FaultKind::MessageLoss,
            FaultKind::DelayReorder,
            FaultKind::Duplication,
        ]
        .contains(&kind);
        let mut placed = None;
        for _ in 0..PLACEMENTS {
            let length = EPISODE_MS.0 + below(rng, EPISODE_MS.1 - EPISODE_MS.0 + 1);
            let start = LEAD_MS + below(rng, chaos_ms - length - LEAD_MS);
            let end = start + length;
            if !is_net || net_busy.iter().all(|(a, b)| end < *a || *b < start) {
                placed = Some((start, end));
                break;
            }
        }
        let Some(window) = placed else {
            continue;
        };
        if is_net {
            net_busy.push(window);
        }
        episode(rng, kind, intensity, window, &all, &live, &mut steps);
    }
    steps
}

/// One episode of `kind` between `(start, end)` milliseconds: the fault, then its undoing.
fn episode(
    rng: &mut SplitMix64,
    kind: FaultKind,
    intensity: Intensity,
    (start, end): (u64, u64),
    all: &[NodeId],
    live: &[NodeId],
    steps: &mut Vec<Timed>,
) {
    let mut push = |at_ms: u64, step: Step| steps.push(Timed { at_ms, step });
    let survival = SURVIVALS[pick(rng, SURVIVALS.len())];
    let node = live[pick(rng, live.len())];
    match kind {
        FaultKind::CrashRestart => {
            // Every fourth start time keeps the timed crash; the others arm a crash point
            // (state-triggered: lost and stale sync completions, a reply never sent). The
            // choice reads values already drawn, so no other draw moves.
            let arm = POINTS.get(usize::try_from(start % 4).unwrap_or(POINTS.len()));
            match arm {
                Some(point) => push(
                    start,
                    Step::Arm {
                        node,
                        point: *point,
                        survival,
                    },
                ),
                None => push(start, Step::Crash { node, survival }),
            }
            push(end, Step::Restart { node });
        }
        FaultKind::AllNodeCrash => {
            push(start, Step::CrashAll { survival });
            for node in all {
                push(end, Step::Restart { node: *node });
            }
        }
        FaultKind::PermanentLoss => {}
        FaultKind::Pause => {
            push(start, Step::Pause { node });
            push(end, Step::Resume { node });
        }
        FaultKind::SymmetricPartition => {
            push(start, symmetric_partition(rng, all));
            push(end, Step::Heal);
        }
        FaultKind::AsymmetricPartition => {
            let (cut, undo) = asymmetric_partition(rng, all, node);
            push(start, cut);
            push(end, undo);
        }
        FaultKind::MessageLoss => {
            let loss_ppm = intensity.loss_ppm();
            let peer = LinkSpec {
                loss_ppm,
                ..HEALTHY_PEER
            };
            let client = LinkSpec {
                loss_ppm,
                ..HEALTHY_CLIENT
            };
            push(start, Step::Net { peer, client });
            push(end, healthy());
        }
        FaultKind::DelayReorder => {
            let max_delay_us = intensity.max_delay_us();
            let peer = LinkSpec {
                min_delay_us: 100,
                max_delay_us,
                ..HEALTHY_PEER
            };
            let client = LinkSpec {
                max_delay_us,
                ..HEALTHY_CLIENT
            };
            push(start, Step::Net { peer, client });
            push(end, healthy());
        }
        FaultKind::Duplication => {
            let duplicate_ppm = intensity.duplicate_ppm();
            let peer = LinkSpec {
                duplicate_ppm,
                ..HEALTHY_PEER
            };
            let client = LinkSpec {
                duplicate_ppm,
                ..HEALTHY_CLIENT
            };
            push(start, Step::Net { peer, client });
            push(end, healthy());
        }
    }
}

/// A split of the voters into groups of 3 and 2, 4 and 1, or 2, 2 and 1, in a drawn order.
fn symmetric_partition(rng: &mut SplitMix64, all: &[NodeId]) -> Step {
    let order = shuffled(rng, all.to_vec());
    let shape: &[usize] = [&[3, 2][..], &[4, 1], &[2, 2, 1]][pick(rng, 3)];
    let mut groups = Vec::new();
    let mut from = 0;
    for size in shape {
        groups.push(order[from..from + size].to_vec());
        from += size;
    }
    Step::Partition { groups }
}

/// A one-way cut and the step that undoes it: one link, all of `node`'s links in one
/// direction, or a directed cycle through three to five voters.
fn asymmetric_partition(rng: &mut SplitMix64, all: &[NodeId], node: NodeId) -> (Step, Step) {
    let others: Vec<NodeId> = all.iter().copied().filter(|other| *other != node).collect();
    let other = others[pick(rng, others.len())];
    match pick(rng, 4) {
        0 => (
            Step::Cut {
                from: node,
                to: other,
            },
            Step::Mend {
                from: node,
                to: other,
            },
        ),
        1 => (
            // The node can send but hears nothing.
            Step::Cuts {
                links: others.iter().map(|peer| (*peer, node)).collect(),
            },
            Step::Heal,
        ),
        2 => (
            // The node hears but cannot be heard.
            Step::Cuts {
                links: others.iter().map(|peer| (node, *peer)).collect(),
            },
            Step::Heal,
        ),
        _ => {
            let size = 3 + pick(rng, all.len() - 2);
            let cycle: Vec<NodeId> = shuffled(rng, all.to_vec()).into_iter().take(size).collect();
            match Partition::directed_cycle(&cycle) {
                Ok(Partition::Cuts(links)) => (Step::Cuts { links }, Step::Heal),
                Ok(Partition::Groups(_)) | Err(_) => {
                    types::invariant!(
                        false,
                        "SWARM-CYCLE",
                        "a directed cycle through {size} distinct voters is a set of cuts"
                    );
                    (Step::Heal, Step::Heal)
                }
            }
        }
    }
}
/// The step that restores healthy links.
const fn healthy() -> Step {
    Step::Net {
        peer: HEALTHY_PEER,
        client: HEALTHY_CLIENT,
    }
}

// ---- the file's JSON ----

const fn point_name(point: CrashPoint) -> &'static str {
    match point {
        CrashPoint::BeforeSync => "before_sync",
        CrashPoint::AfterSync => "after_sync",
        CrashPoint::BeforeReply => "before_reply",
    }
}

fn point_of(name: &str) -> Result<CrashPoint, ScenarioError> {
    POINTS
        .into_iter()
        .find(|point| point_name(*point) == name)
        .ok_or_else(|| malformed(format!("unknown crash point {name:?}")))
}

/// The crash points an armed crash can name, in file order.
const POINTS: [CrashPoint; 3] = [
    CrashPoint::BeforeSync,
    CrashPoint::AfterSync,
    CrashPoint::BeforeReply,
];

const fn survival_name(survival: Survival) -> &'static str {
    match survival {
        Survival::Seeded => "seeded",
        Survival::Nothing => "nothing",
        Survival::Partial => "partial",
        Survival::PartialThenGarbage => "partial_then_garbage",
        Survival::All => "all",
    }
}

fn survival_of(name: &str) -> Result<Survival, ScenarioError> {
    SURVIVALS
        .into_iter()
        .find(|survival| survival_name(*survival) == name)
        .ok_or_else(|| malformed(format!("unknown survival {name:?}")))
}

fn timed_to_json(timed: &Timed) -> Value {
    let mut map = Map::new();
    map.insert("at_ms".to_owned(), json!(timed.at_ms));
    let node = |node: &NodeId| json!(node.get());
    let mut put = |key: &str, value: Value| {
        map.insert(key.to_owned(), value);
    };
    match &timed.step {
        Step::Crash { node: n, survival } => {
            put("fault", json!("crash"));
            put("node", node(n));
            put("survival", json!(survival_name(*survival)));
        }
        Step::Arm {
            node: n,
            point,
            survival,
        } => {
            put("fault", json!("arm"));
            put("node", node(n));
            put("point", json!(point_name(*point)));
            put("survival", json!(survival_name(*survival)));
        }
        Step::CrashAll { survival } => {
            put("fault", json!("crash_all"));
            put("survival", json!(survival_name(*survival)));
        }
        Step::Restart { node: n } => {
            put("fault", json!("restart"));
            put("node", node(n));
        }
        Step::Lose { node: n } => {
            put("fault", json!("lose"));
            put("node", node(n));
        }
        Step::Pause { node: n } => {
            put("fault", json!("pause"));
            put("node", node(n));
        }
        Step::Resume { node: n } => {
            put("fault", json!("resume"));
            put("node", node(n));
        }
        Step::Partition { groups } => {
            put("fault", json!("partition"));
            put(
                "groups",
                json!(
                    groups
                        .iter()
                        .map(|group| group.iter().map(|node| node.get()).collect::<Vec<_>>())
                        .collect::<Vec<_>>()
                ),
            );
        }
        Step::Cuts { links } => {
            put("fault", json!("cuts"));
            put(
                "links",
                json!(
                    links
                        .iter()
                        .map(|(from, to)| [from.get(), to.get()])
                        .collect::<Vec<_>>()
                ),
            );
        }
        Step::Cut { from, to } => {
            put("fault", json!("cut"));
            put("from", node(from));
            put("to", node(to));
        }
        Step::Mend { from, to } => {
            put("fault", json!("mend"));
            put("from", node(from));
            put("to", node(to));
        }
        Step::Heal => put("fault", json!("heal")),
        Step::Net { peer, client } => {
            put("fault", json!("net"));
            put("peer", peer.to_json());
            put("client", client.to_json());
        }
    }
    Value::Object(map)
}

fn timed_from_json(value: &Value) -> Result<Timed, ScenarioError> {
    let at_ms = u64_of(value, "at_ms", "fault")?;
    let node = |field: &str| node_of(&value[field], field);
    let fault = str_of(value, "fault", "fault")?;
    let fields: &[&str] = match fault {
        "crash" => &["at_ms", "fault", "node", "survival"],
        "arm" => &["at_ms", "fault", "node", "point", "survival"],
        "crash_all" => &["at_ms", "fault", "survival"],
        "restart" | "lose" | "pause" | "resume" => &["at_ms", "fault", "node"],
        "partition" => &["at_ms", "fault", "groups"],
        "cuts" => &["at_ms", "fault", "links"],
        "cut" | "mend" => &["at_ms", "fault", "from", "to"],
        "net" => &["at_ms", "fault", "peer", "client"],
        _ => &["at_ms", "fault"],
    };
    only_fields(value, fields, "fault")?;
    let step = match fault {
        "crash" => Step::Crash {
            node: node("node")?,
            survival: survival_of(str_of(value, "survival", "crash")?)?,
        },
        "arm" => Step::Arm {
            node: node("node")?,
            point: point_of(str_of(value, "point", "arm")?)?,
            survival: survival_of(str_of(value, "survival", "arm")?)?,
        },
        "crash_all" => Step::CrashAll {
            survival: survival_of(str_of(value, "survival", "crash_all")?)?,
        },
        "restart" => Step::Restart {
            node: node("node")?,
        },
        "lose" => Step::Lose {
            node: node("node")?,
        },
        "pause" => Step::Pause {
            node: node("node")?,
        },
        "resume" => Step::Resume {
            node: node("node")?,
        },
        "partition" => Step::Partition {
            groups: array_of(value, "groups", "partition")?
                .iter()
                .map(|group| {
                    group
                        .as_array()
                        .ok_or_else(|| malformed("a partition group must be an array"))?
                        .iter()
                        .map(|member| node_of(member, "groups"))
                        .collect()
                })
                .collect::<Result<_, _>>()?,
        },
        "cuts" => Step::Cuts {
            links: array_of(value, "links", "cuts")?
                .iter()
                .map(|link| match link.as_array().map(Vec::as_slice) {
                    Some([from, to]) => Ok((node_of(from, "links")?, node_of(to, "links")?)),
                    Some(_) | None => Err(malformed("a link is a [from, to] pair")),
                })
                .collect::<Result<_, _>>()?,
        },
        "cut" => Step::Cut {
            from: node("from")?,
            to: node("to")?,
        },
        "mend" => Step::Mend {
            from: node("from")?,
            to: node("to")?,
        },
        "heal" => Step::Heal,
        "net" => Step::Net {
            peer: LinkSpec::from_json(&value["peer"], "net.peer")?,
            client: LinkSpec::from_json(&value["client"], "net.client")?,
        },
        other => return Err(malformed(format!("unknown fault {other:?}"))),
    };
    Ok(Timed { at_ms, step })
}

fn plan_to_json(plan: &Plan) -> Value {
    let op = |planned: &Planned| {
        let mut map = Map::new();
        map.insert("think_ms".to_owned(), json!(planned.think_ms));
        let mut put = |key: &str, value: Value| {
            map.insert(key.to_owned(), value);
        };
        match &planned.intent {
            Intent::Put {
                request_id,
                key,
                value,
            } => {
                put("op", json!("put"));
                put("request_id", json!(request_id.to_string()));
                put("key", json!(key.as_str()));
                put("value", json!(value.as_str()));
            }
            Intent::Delete { request_id, key } => {
                put("op", json!("delete"));
                put("request_id", json!(request_id.to_string()));
                put("key", json!(key.as_str()));
            }
            Intent::Cas {
                request_id,
                key,
                value,
            } => {
                put("op", json!("cas"));
                put("request_id", json!(request_id.to_string()));
                put("key", json!(key.as_str()));
                put("value", json!(value.as_str()));
            }
            Intent::Get { key } => {
                put("op", json!("get"));
                put("key", json!(key.as_str()));
            }
            Intent::Scan { range } => {
                put("op", json!("scan"));
                for (field, bound) in [
                    ("start", range.start()),
                    ("end", range.end()),
                    ("after", range.after()),
                ] {
                    if let Some(bound) = bound {
                        put(field, json!(bound.as_str()));
                    }
                }
                put("limit", json!(range.limit().get()));
            }
        }
        Value::Object(map)
    };
    json!({
        "hot": plan.hot,
        "keys": plan.keys.iter().map(Key::as_str).collect::<Vec<_>>(),
        "clients": plan.clients.iter().map(|client| json!({
            "client": client.client.get(),
            "ops": client.ops.iter().map(op).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

fn plan_from_json(value: &Value, limits: KeyValueLimits) -> Result<Plan, ScenarioError> {
    let key = |text: &Value| -> Result<Key, ScenarioError> {
        let text = text
            .as_str()
            .ok_or_else(|| malformed("a key must be a string"))?;
        Key::parse(text.to_owned(), limits)
            .map_err(|error| malformed(format!("key {text:?}: {error:?}")))
    };
    only_fields(value, &["hot", "keys", "clients"], "plan")?;
    let keys = array_of(value, "keys", "plan")?
        .iter()
        .map(key)
        .collect::<Result<Vec<_>, _>>()?;
    let distinct: BTreeSet<&Key> = keys.iter().collect();
    let wanted =
        usize::try_from(KEYS.0).unwrap_or(0)..=usize::try_from(KEYS.1).unwrap_or(usize::MAX);
    if distinct.len() != keys.len() || !wanted.contains(&keys.len()) {
        return Err(malformed(format!(
            "a plan has {} to {} distinct keys; this one has {} ({} distinct)",
            KEYS.0,
            KEYS.1,
            keys.len(),
            distinct.len()
        )));
    }
    let hot = usize::try_from(u64_of(value, "hot", "plan")?).unwrap_or(usize::MAX);
    if hot == 0 || hot > keys.len() {
        return Err(malformed(format!(
            "plan.hot must be 1 to {}; it is {hot}",
            keys.len()
        )));
    }
    let mut seen: BTreeSet<RequestId> = BTreeSet::new();
    let mut clients = Vec::new();
    for client in array_of(value, "clients", "plan")? {
        only_fields(client, &["client", "ops"], "client")?;
        let number = u64_of(client, "client", "client")?;
        let mut ops = Vec::new();
        for op in array_of(client, "ops", "client")? {
            let request_id = || -> Result<RequestId, ScenarioError> {
                let id = RequestId::parse(str_of(op, "request_id", "op")?)
                    .map_err(|error| malformed(format!("request_id: {error:?}")))?;
                if seen.contains(&id) {
                    return Err(malformed(format!("request ID {id} is used twice")));
                }
                Ok(id)
            };
            let text = || -> Result<Val, ScenarioError> {
                let raw = str_of(op, "value", "op")?;
                Val::parse(raw.to_owned(), limits)
                    .map_err(|error| malformed(format!("value: {error:?}")))
            };
            let kind = str_of(op, "op", "op")?;
            let fields: &[&str] = match kind {
                "put" | "cas" => &["think_ms", "op", "request_id", "key", "value"],
                "delete" => &["think_ms", "op", "request_id", "key"],
                "get" => &["think_ms", "op", "key"],
                "scan" => &["think_ms", "op", "start", "end", "after", "limit"],
                _ => &["think_ms", "op"],
            };
            only_fields(op, fields, "op")?;
            let intent = match kind {
                "put" => Intent::Put {
                    request_id: request_id()?,
                    key: key(&op["key"])?,
                    value: text()?,
                },
                "delete" => Intent::Delete {
                    request_id: request_id()?,
                    key: key(&op["key"])?,
                },
                "cas" => Intent::Cas {
                    request_id: request_id()?,
                    key: key(&op["key"])?,
                    value: text()?,
                },
                "get" => Intent::Get {
                    key: key(&op["key"])?,
                },
                "scan" => Intent::Scan {
                    range: scan_from_json(op, &key)?,
                },
                other => return Err(malformed(format!("unknown op {other:?}"))),
            };
            if let Some(id) = intent.request_id() {
                seen.insert(id);
            }
            ops.push(Planned {
                intent,
                think_ms: u64_of(op, "think_ms", "op")?,
            });
        }
        clients.push(ClientPlan {
            client: ClientId::new(number),
            ops,
        });
    }
    Ok(Plan { clients, keys, hot })
}

/// Refuses a JSON object that has a field outside `allowed`, so a misspelt optional field is
/// an error rather than silently ignored.
fn only_fields(value: &Value, allowed: &[&str], what: &str) -> Result<(), ScenarioError> {
    let object = value
        .as_object()
        .ok_or_else(|| malformed(format!("{what} must be an object")))?;
    object
        .keys()
        .find(|key| !allowed.contains(&key.as_str()))
        .map_or(Ok(()), |key| {
            Err(malformed(format!("{what} has an unknown field {key:?}")))
        })
}

/// A SCAN's range: the optional `start`, `end` and `after` keys and the page `limit`.
fn scan_from_json(
    op: &Value,
    key: &dyn Fn(&Value) -> Result<Key, ScenarioError>,
) -> Result<ScanRange, ScenarioError> {
    let bound = |field: &str| match &op[field] {
        Value::Null => Ok(None),
        text => key(text).map(Some),
    };
    let limit = match &op["limit"] {
        Value::Null => PageSize::DEFAULT,
        raw => raw
            .as_u64()
            .and_then(|raw| PageSize::new(raw).ok())
            .ok_or_else(|| malformed("scan.limit must be a page size"))?,
    };
    ScanRange::new(bound("start")?, bound("end")?, bound("after")?, limit)
        .map_err(|error| malformed(format!("scan: {error:?}")))
}

fn u64_of(value: &Value, field: &str, what: &str) -> Result<u64, ScenarioError> {
    value[field]
        .as_u64()
        .ok_or_else(|| malformed(format!("{what}.{field} must be a non-negative integer")))
}

fn str_of<'a>(value: &'a Value, field: &str, what: &str) -> Result<&'a str, ScenarioError> {
    value[field]
        .as_str()
        .ok_or_else(|| malformed(format!("{what}.{field} must be a string")))
}

fn array_of<'a>(
    value: &'a Value,
    field: &str,
    what: &str,
) -> Result<&'a Vec<Value>, ScenarioError> {
    value[field]
        .as_array()
        .ok_or_else(|| malformed(format!("{what}.{field} must be an array")))
}

fn node_of(value: &Value, what: &str) -> Result<NodeId, ScenarioError> {
    value
        .as_u64()
        .and_then(|raw| NodeId::new(raw).ok())
        .ok_or_else(|| malformed(format!("{what} must name a node (a positive integer)")))
}
