//! The run phases and the heal protocol (PLAN.md §10.2, §10.4, §9.4; W37.3).
//!
//! A scenario run has five phases, each marked in the trace ([`Phases`]):
//! 1. **Setup:** the cluster and clients are built from the scenario.
//! 2. **Baseline:** the cluster elects a leader and a baseline client completes
//!    [`BASELINE_WRITES`] writes, before any fault is scheduled. The plan's clients wait.
//! 3. **Chaos:** the scenario's faults are scheduled relative to chaos's start, the plan's
//!    clients are released, and the run goes on for the scenario's chaos length.
//! 4. **Heal:** faults stop. Armed crash points are disarmed, every recoverable node restarts
//!    and resumes, every link and profile is restored, and clients start nothing new. Each
//!    client then resolves its outstanding logical writes by retrying with the write's own
//!    request ID. Then one uniquely identified PUT, the catch-up marker, is written, and the
//!    heal waits until every recoverable node has applied it. The heal never edits a log,
//!    forces an election, teleports state or drops an outstanding request: it only restores
//!    what chaos broke and waits, so what it shows is the system's own recovery (§10.2). Its
//!    budget is [`HEAL_BUDGET_TICKS`] ticks (§9.4).
//! 5. **Check:** the run's observations are complete and the checkers judge them (W38+).
//!
//! A capacity profile adapts the heal (W43.2; §10.2, D-53). A live-quota run (DM-6) creates
//! the marker key in the baseline, so the marker overwrites it with a value of the same size
//! and cannot be refused. A history run (DM-7) writes no marker, which a full history would
//! refuse: it waits for a certified drain ([`crate::drain`]) and then resolves each
//! outstanding write by status lookup on every recoverable node.
//!
//! The heal ends two ways. [`Heal::Converged`] carries what K-7 compares (each recoverable
//! node's applied index and state digest once the marker is applied everywhere) and what K-9
//! times. [`Heal::Stalled`] is the run's own finding that the heal did not finish within its
//! budget, and says which stage it was in; whether that is a liveness failure or an invalid
//! scenario is K-9's judgment of the §9.4 envelope, never this driver's.
//!
//! The nemesis quorum guard (§10.4): the heal starts only when at least three voters are
//! recoverable, which [`validate`](crate::scenario::validate) guarantees for every scenario
//! the generator draws and a hand-edited file would have to honour; otherwise the run is
//! refused as [`RunError::QuorumLost`] rather than judged. No fault is injected during the
//! heal beyond its own restores, and the run fails with [`RunError::FaultDuringHeal`] if any
//! other fault fires.
//!
//! **Bounds (CD-9).** The heal's budget is [`HEAL_BUDGET_TICKS`]. Parked writes in the
//! clients are bounded by the plan's writes, at most `MAX_CLIENTS` times `OPS`'s upper
//! bound, plus the baseline's and the marker's. The restores are at most five per node.

use statemachine::StateDigest;
use types::{Config, Key, KeyValueLimits, LogIndex, NodeId, RequestId, Value};

use crate::clients::{Clients, End, HealMode, Intent, Planned};
use crate::cluster::{Cluster, ClusterError};
use crate::drain::{Certified, QUIET_TICKS, QuietClock, Sample};
use crate::nemesis::Fault;
use crate::observe::{Mark, Phases};
use crate::scenario::{HEALTHY_CLIENT, HEALTHY_PEER, Scenario, ScenarioError};
use crate::scheduler::VirtualTime;

/// How many writes the baseline completes before any fault.
pub const BASELINE_WRITES: usize = 3;

/// The longest the baseline may take, in virtual milliseconds: an election and a few writes
/// on healthy links.
pub const BASELINE_BUDGET_MS: u64 = 10_000;

/// The heal's budget in ticks (§10.2, §9.4): 30 virtual seconds at a 50 ms tick.
pub const HEAL_BUDGET_TICKS: u64 = 600;

/// How many voters the heal needs recoverable to wait for a quorum (§9.4).
pub const QUORUM: usize = 3;

/// One virtual millisecond, in microseconds.
const MS: u64 = 1_000;

/// How far a driving loop advances between looks at the run, in virtual milliseconds.
const SLICE_MS: u64 = 100;

/// Byte 8 of the baseline writes' request IDs. A plan's request ID has a zero there (its
/// client number's top byte), so these never collide with one.
const BASELINE_ROLE: u8 = 0xB0;

/// Byte 8 of the catch-up marker's request ID.
const MARKER_ROLE: u8 = 0xC0;

/// Why a run could not be driven: a harness or scenario problem, never a finding about the
/// system under test.
#[derive(Debug)]
pub enum RunError {
    /// The scenario could not be started or its faults not scheduled.
    Scenario(ScenarioError),
    /// Driving the cluster failed (its trace is marked incomplete).
    Cluster(ClusterError),
    /// The baseline did not finish: the cluster did not elect a leader and complete its
    /// writes on healthy links within [`BASELINE_BUDGET_MS`].
    Baseline {
        /// The baseline writes that ended in a terminal answer.
        done: usize,
    },
    /// Fewer than [`QUORUM`] voters are recoverable when the heal begins.
    QuorumLost {
        /// The recoverable voters.
        survivors: usize,
    },
    /// The catch-up marker was refused before proposal (`history_full`): a capacity
    /// condition the capacity-profile heal adaptations (W43) own, not a liveness finding.
    MarkerRefused,
    /// A fault other than the heal's own restores fired during the heal.
    FaultDuringHeal {
        /// How many.
        faults: usize,
    },
}

impl From<ClusterError> for RunError {
    fn from(error: ClusterError) -> Self {
        Self::Cluster(error)
    }
}

impl From<ScenarioError> for RunError {
    fn from(error: ScenarioError) -> Self {
        Self::Scenario(error)
    }
}

/// What a node holds once the marker is applied everywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeEnd {
    /// The node.
    pub node: NodeId,
    /// Its applied index.
    pub applied: LogIndex,
    /// Its state digest.
    pub digest: StateDigest,
}

/// The catch-up marker a heal wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkerEnd {
    /// The marker's request ID.
    pub id: RequestId,
    /// The marker's key.
    pub key: Key,
    /// The marker's value.
    pub value: Value,
    /// Whether the key was created in the baseline, so the marker overwrote a value of the
    /// same size (DM-6).
    pub pre_created: bool,
}

/// What proved a heal converged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Completion {
    /// The marker was applied on every recoverable node.
    Marker(MarkerEnd),
    /// No marker: the drain was certified and every outstanding write resolved by status
    /// lookup on every recoverable node (DM-7).
    Drain(Certified),
}

/// A heal that finished: every outstanding write resolved and the cluster proven converged
/// by its [`Completion`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Converged {
    /// How convergence was proved.
    pub completion: Completion,
    /// How many ticks the heal took, from its start to its completion.
    pub ticks: u64,
    /// Each recoverable node, in node order, with its state at completion.
    pub nodes: Vec<NodeEnd>,
    /// The nodes lost for good, in node order.
    pub lost: Vec<NodeId>,
    /// How many unresolved writes the heal resumed (counting a write each time).
    pub resumed: usize,
}

impl Converged {
    /// The catch-up marker's request ID, if the heal wrote one.
    #[must_use]
    pub const fn marker(&self) -> Option<RequestId> {
        match &self.completion {
            Completion::Marker(marker) => Some(marker.id),
            Completion::Drain(_) => None,
        }
    }
}

/// The stage a stalled heal was in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Some logical write was still unresolved.
    Resolving,
    /// Every write was resolved but the marker had no terminal answer.
    Marker,
    /// The marker was answered but a recoverable node had not applied it.
    Applying,
}

/// A heal that used its whole budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stalled {
    /// Where it stood.
    pub stage: Stage,
    /// The ticks used: the budget.
    pub ticks: u64,
    /// Writes in flight or parked when it stopped.
    pub unresolved: usize,
    /// The recoverable nodes that had not applied the marker (every one before it is
    /// written), in node order.
    pub lagging: Vec<NodeId>,
}

/// How the heal ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Heal {
    /// It finished within its budget.
    Converged(Converged),
    /// It did not.
    Stalled(Stalled),
}

/// A finished run: the cluster and clients as they ended, when each phase began and how the
/// heal ended.
#[derive(Debug)]
pub struct Finished {
    /// The cluster.
    pub cluster: Cluster,
    /// The clients: the plan's, the baseline client and the marker's.
    pub clients: Clients,
    /// When each phase began.
    pub phases: Phases,
    /// How the heal ended.
    pub heal: Heal,
    /// The chaos phase's length in ticks.
    pub chaos_ticks: u64,
}

/// Runs `scenario` through its phases on `config`.
///
/// # Errors
///
/// [`RunError`] for a harness or scenario problem; a heal that does not finish is
/// [`Heal::Stalled`], not an error.
pub fn run(config: &Config, scenario: &Scenario) -> Result<Finished, RunError> {
    run_with(config, scenario, HEAL_BUDGET_TICKS)
}

/// [`run`] with a heal budget of `budget_ticks`, for tests that starve the heal on purpose.
///
/// # Errors
///
/// As [`run`].
pub fn run_with(
    config: &Config,
    scenario: &Scenario,
    budget_ticks: u64,
) -> Result<Finished, RunError> {
    let (mut cluster, mut clients) = scenario.begin(config)?;
    let limits = config.key_value_limits();
    let tick_ms = config.tick_ms();
    let setup = Mark::of(&cluster);

    // Baseline: no fault is scheduled, and the plan's clients wait.
    let baseline = Mark::of(&cluster);
    clients.hold_plan();
    let mode = scenario.workload().heal_mode();
    let mut writes = baseline_writes(scenario, limits);
    if mode == HealMode::PreCreatedMarker {
        // DM-6: the marker key exists before chaos, so the heal's marker only overwrites it
        // with a value of the same size and a full live quota cannot refuse it.
        let origin = Marker::origin(config, scenario);
        types::invariant!(
            origin.is_some(),
            "HEAL-ORIGIN",
            "a pre-created-marker profile has a marker to pre-create: seed={}",
            scenario.seed().get()
        );
        writes.extend(origin);
    }
    let expected = writes.len();
    let client = clients.add_client(writes, 0);
    let deadline = VirtualTime::from_micros(BASELINE_BUDGET_MS * MS);
    while cluster.now() < deadline && !baseline_done(&clients, client_index(client.get()), expected)
    {
        advance(&mut cluster, &mut clients, SLICE_MS)?;
    }
    let done = clients
        .outcomes()
        .iter()
        .filter(|outcome| outcome.client == client && matches_done(&outcome.end))
        .count();
    if done != expected {
        return Err(RunError::Baseline { done });
    }

    // Chaos: the faults are relative to its start, and the plan's clients are released.
    let started = cluster.now();
    let chaos = Mark::of(&cluster);
    scenario.inject_faults(&mut cluster, started)?;
    clients.release_plan(ms_of(started));
    let chaos_end = VirtualTime::from_micros(started.as_micros() + scenario.chaos_ms() * MS);
    while cluster.now() < chaos_end {
        advance_to(&mut cluster, &mut clients, chaos_end)?;
    }

    // Heal.
    let heal_mark = Mark::of(&cluster);
    let heal = heal(
        config,
        scenario,
        mode,
        &mut cluster,
        &mut clients,
        budget_ticks,
    )?;
    let check = Mark::of(&cluster);
    Ok(Finished {
        cluster,
        clients,
        phases: Phases {
            setup,
            baseline,
            chaos,
            heal: heal_mark,
            check,
        },
        heal,
        chaos_ticks: scenario.chaos_ms() / tick_ms,
    })
}

/// The baseline client's writes: [`BASELINE_WRITES`] PUTs of keys no plan uses.
pub(crate) fn baseline_writes(scenario: &Scenario, limits: KeyValueLimits) -> Vec<Planned> {
    (0..BASELINE_WRITES)
        .filter_map(|n| {
            let key = Key::parse(format!("baseline-{n}"), limits).ok()?;
            let value = Value::parse(format!("baseline-{n}"), limits).ok()?;
            Some(Planned {
                intent: Intent::Put {
                    request_id: role_request_id(scenario, BASELINE_ROLE, n),
                    key,
                    value,
                },
                think_ms: 0,
            })
        })
        .collect()
}

/// The request ID of the `n`th write of `role`: the seed's eight bytes, the role, and `n`.
fn role_request_id(scenario: &Scenario, role: u8, n: usize) -> RequestId {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&scenario.seed().get().to_be_bytes());
    bytes[8] = role;
    bytes[12..].copy_from_slice(&u32::try_from(n).unwrap_or(u32::MAX).to_be_bytes());
    RequestId::from_bytes(bytes)
}

/// The position of a client number in the clients' table.
fn client_index(number: u64) -> usize {
    usize::try_from(number).unwrap_or(usize::MAX)
}

/// Whether the baseline client has ended all `expected` operations.
fn baseline_done(clients: &Clients, client: usize, expected: usize) -> bool {
    clients
        .outcomes()
        .iter()
        .filter(|outcome| client_index(outcome.client.get()) == client)
        .count()
        >= expected
}

/// Whether an end is a terminal answer (not a refusal and not unresolved).
const fn matches_done(end: &End) -> bool {
    match end {
        End::Done(_) => true,
        End::Refused | End::Unresolved(_) | End::NotExecuted(_) => false,
    }
}

/// `at` in whole virtual milliseconds.
const fn ms_of(at: VirtualTime) -> u64 {
    at.as_micros() / MS
}

/// Runs the cluster and clients for `ms` more virtual milliseconds.
fn advance(cluster: &mut Cluster, clients: &mut Clients, ms: u64) -> Result<(), RunError> {
    let to = VirtualTime::from_micros(cluster.now().as_micros() + ms * MS);
    advance_to(cluster, clients, to)
}

/// Runs the cluster and clients until `to`. The clients stop the run early once they have
/// finished, so the cluster is then advanced alone.
fn advance_to(
    cluster: &mut Cluster,
    clients: &mut Clients,
    to: VirtualTime,
) -> Result<(), RunError> {
    clients.run(cluster, to)?;
    if cluster.now() < to {
        cluster.run_until(to)?;
    }
    Ok(())
}

/// The catch-up marker: one uniquely identified PUT.
struct Marker {
    id: RequestId,
    key: Key,
    value: Value,
}

impl Marker {
    /// The marker of `scenario`, or `None` if it does not fit `config`'s limits.
    fn of(config: &Config, scenario: &Scenario) -> Option<Self> {
        let limits = config.key_value_limits();
        Some(Self {
            id: role_request_id(scenario, MARKER_ROLE, 0),
            key: Key::parse("heal-marker".to_owned(), limits).ok()?,
            value: Value::parse(format!("marker-{}", scenario.seed().get()), limits).ok()?,
        })
    }

    /// The baseline write that creates the marker's key with a value of the marker's length
    /// (DM-6): `origin-<seed>` against `marker-<seed>`.
    fn origin(config: &Config, scenario: &Scenario) -> Option<Planned> {
        let marker = Self::of(config, scenario)?;
        let limits = config.key_value_limits();
        let value = Value::parse(format!("origin-{}", scenario.seed().get()), limits).ok()?;
        types::invariant!(
            value.byte_len() == marker.value.byte_len(),
            "HEAL-ORIGIN",
            "the pre-created marker value has the marker's size: {} vs {}",
            value.byte_len(),
            marker.value.byte_len()
        );
        Some(Planned {
            intent: Intent::Put {
                request_id: role_request_id(scenario, MARKER_ROLE, 1),
                key: marker.key,
                value,
            },
            think_ms: 0,
        })
    }

    /// The marker as a planned write.
    fn planned(&self) -> Planned {
        Planned {
            intent: Intent::Put {
                request_id: self.id,
                key: self.key.clone(),
                value: self.value.clone(),
            },
            think_ms: 0,
        }
    }
}

/// Stops the faults and restores what they broke: resumes paused nodes, restarts crashed
/// ones, restores every link and both profiles. Returns the faults it scheduled, which are
/// the only ones the heal may see.
fn restore(cluster: &mut Cluster, recoverable: &[NodeId]) -> Result<Vec<Fault>, RunError> {
    cluster.disarm_all();
    let mut restores = Vec::new();
    for node in recoverable {
        if cluster.is_paused(*node) {
            restores.push(Fault::Resume { node: *node });
        } else if cluster.controller(*node).is_none() {
            restores.push(Fault::Restart { node: *node });
        }
    }
    restores.push(Fault::Heal);
    let (peer, client) = (HEALTHY_PEER.profile(), HEALTHY_CLIENT.profile());
    types::invariant!(
        peer.is_some() && client.is_some(),
        "HEAL-PROFILES",
        "the healthy profiles are valid"
    );
    if let (Some(peer), Some(client)) = (peer, client) {
        restores.push(Fault::Profiles { peer, client });
    }
    let now = cluster.now();
    for fault in &restores {
        cluster.inject_at(now, fault.clone())?;
    }
    Ok(restores)
}

/// The heal (§10.2). Returns how it ended.
///
/// Its clients retry among the recoverable nodes only: a node lost for good is not an
/// endpoint an operator retrying a write would try again, and the plan's clients, which do not
/// know, have already spent their sequences by the time the heal resumes their writes.
fn heal(
    config: &Config,
    scenario: &Scenario,
    mode: HealMode,
    cluster: &mut Cluster,
    clients: &mut Clients,
    budget_ticks: u64,
) -> Result<Heal, RunError> {
    let tick_ms = config.tick_ms();
    let began = cluster.now();
    let voters: Vec<NodeId> = config.voters().collect();
    let (lost, recoverable): (Vec<NodeId>, Vec<NodeId>) = voters
        .iter()
        .copied()
        .partition(|node| cluster.is_lost(*node));
    if recoverable.len() < QUORUM {
        return Err(RunError::QuorumLost {
            survivors: recoverable.len(),
        });
    }
    clients.stop_starting();
    clients.retry_among(&recoverable);
    let faults_before = cluster.faults().count();
    let restores = restore(cluster, &recoverable)?;

    let deadline = VirtualTime::from_micros(began.as_micros() + budget_ticks * tick_ms * MS);
    let outcome = match mode {
        HealMode::Drain => drain_heal(
            cluster,
            clients,
            (began, deadline),
            tick_ms,
            lost,
            &recoverable,
        )?,
        HealMode::Marker | HealMode::PreCreatedMarker => marker_heal(
            (config, scenario),
            mode == HealMode::PreCreatedMarker,
            cluster,
            clients,
            (began, deadline),
            lost,
            &recoverable,
        )?,
    };

    // Exactly the restores were scheduled since the heal began, each once.
    let scheduled = cluster.faults().count() - faults_before;
    let extra = scheduled.abs_diff(restores.len()).max(
        cluster
            .faults()
            .skip(faults_before)
            .filter(|(_, fault)| !restores.contains(fault))
            .count(),
    );
    if extra > 0 {
        return Err(RunError::FaultDuringHeal { faults: extra });
    }
    Ok(outcome)
}

/// The marker heal: resume the unresolved writes, write the marker, wait until every
/// recoverable node has applied it.
fn marker_heal(
    (config, scenario): (&Config, &Scenario),
    pre_created: bool,
    cluster: &mut Cluster,
    clients: &mut Clients,
    (began, deadline): (VirtualTime, VirtualTime),
    lost: Vec<NodeId>,
    recoverable: &[NodeId],
) -> Result<Heal, RunError> {
    let tick_ms = config.tick_ms();
    let mut resumed = 0;
    let mut marker: Option<Marker> = None;
    loop {
        resumed += clients.resume_parked(cluster)?;
        let stage = stage_of(cluster, clients, recoverable, marker.as_ref());
        if let Some(marker) = &marker {
            let refused = clients.outcomes().iter().any(|outcome| {
                outcome.request_id == Some(marker.id) && outcome.end == End::Refused
            });
            if refused {
                return Err(RunError::MarkerRefused);
            }
        }
        if stage.is_none() {
            if let Some(marker) = marker {
                return Ok(Heal::Converged(Converged {
                    completion: Completion::Marker(MarkerEnd {
                        id: marker.id,
                        key: marker.key,
                        value: marker.value,
                        pre_created,
                    }),
                    ticks: ticks_since(cluster, began, tick_ms),
                    nodes: node_ends(cluster, recoverable),
                    lost,
                    resumed,
                }));
            }
            // Every write is resolved: the marker goes out now, as its own client.
            let Some(made) = Marker::of(config, scenario) else {
                types::invariant!(false, "HEAL-MARKER", "the marker fits the limits");
                return Ok(stalled(
                    Stage::Marker,
                    cluster,
                    began,
                    tick_ms,
                    clients,
                    recoverable,
                    None,
                ));
            };
            clients.add_client(vec![made.planned()], ms_of(cluster.now()));
            marker = Some(made);
        }
        if cluster.now() >= deadline {
            return Ok(stalled(
                stage.unwrap_or(Stage::Marker),
                cluster,
                began,
                tick_ms,
                clients,
                recoverable,
                marker.as_ref().map(|marker| marker.id),
            ));
        }
        let to = VirtualTime::from_micros(
            (cluster.now().as_micros() + SLICE_MS * MS).min(deadline.as_micros()),
        );
        advance_to(cluster, clients, to)?;
    }
}

/// The drain heal (DM-7, §10.2, D-53): no marker, because a full history would refuse it.
/// Clients have stopped; in-flight operations run to their ends. Once the cluster has shown
/// a drain for the whole quiet period, every write left unresolved is looked up on every
/// recoverable node, and the heal converges when each lookup resolved. It never resumes a
/// write: a resend could be refused or executed, and the lookup is what decides.
fn drain_heal(
    cluster: &mut Cluster,
    clients: &mut Clients,
    (began, deadline): (VirtualTime, VirtualTime),
    tick_ms: u64,
    lost: Vec<NodeId>,
    recoverable: &[NodeId],
) -> Result<Heal, RunError> {
    // Packets still in flight from the chaos phase are bounded by the longest chaos delay;
    // the quiet period must outlast it, or "no change" could hide a delivery yet to come.
    types::invariant!(
        QUIET_TICKS.saturating_mul(tick_ms).saturating_mul(1_000)
            > crate::swarm::Intensity::High.max_delay_us(),
        "DRAIN-QUIET",
        "the quiet period outlasts the longest chaos delay: ticks={QUIET_TICKS} tick_ms={tick_ms}"
    );
    let mut clock = QuietClock::new(QUIET_TICKS, tick_ms);
    let mut certified: Option<Certified> = None;
    loop {
        if certified.is_none() {
            let sample = Sample::of(cluster, recoverable, clients.in_flight());
            certified = clock.observe(cluster.now(), &sample.judge());
            if let Some(drained) = &certified {
                clients.begin_lookups(cluster, drained, recoverable)?;
            }
        }
        if let Some(drained) = certified
            && clients.in_flight() == 0
            && clients.parked() == 0
            && clients.lookups_open() == 0
        {
            return Ok(Heal::Converged(Converged {
                completion: Completion::Drain(drained),
                ticks: ticks_since(cluster, began, tick_ms),
                nodes: node_ends(cluster, recoverable),
                lost,
                resumed: 0,
            }));
        }
        if cluster.now() >= deadline {
            return Ok(stalled(
                Stage::Resolving,
                cluster,
                began,
                tick_ms,
                clients,
                recoverable,
                None,
            ));
        }
        let to = VirtualTime::from_micros(
            (cluster.now().as_micros() + SLICE_MS * MS).min(deadline.as_micros()),
        );
        advance_to(cluster, clients, to)?;
    }
}

/// Whole ticks from `began` to now, rounded up.
fn ticks_since(cluster: &Cluster, began: VirtualTime, tick_ms: u64) -> u64 {
    let elapsed = cluster.now().as_micros() - began.as_micros();
    elapsed.div_ceil(tick_ms * MS)
}

/// Where the heal stands: `None` once the marker is written and applied everywhere.
fn stage_of(
    cluster: &Cluster,
    clients: &Clients,
    recoverable: &[NodeId],
    marker: Option<&Marker>,
) -> Option<Stage> {
    let Some(Marker { id, .. }) = marker else {
        return if clients.in_flight() == 0 && clients.parked() == 0 {
            None
        } else {
            Some(Stage::Resolving)
        };
    };
    if clients.in_flight() > 0 || clients.parked() > 0 {
        return Some(Stage::Marker);
    }
    if lagging(cluster, recoverable, *id).is_empty() {
        None
    } else {
        Some(Stage::Applying)
    }
}

/// The recoverable nodes that have not applied the write `id`: a node that is down has not.
fn lagging(cluster: &Cluster, recoverable: &[NodeId], id: RequestId) -> Vec<NodeId> {
    recoverable
        .iter()
        .copied()
        .filter(|node| {
            cluster
                .controller(*node)
                .is_none_or(|ctl| ctl.state().history().get(&id).is_none())
        })
        .collect()
}

/// Each recoverable node's applied index and state digest.
fn node_ends(cluster: &Cluster, recoverable: &[NodeId]) -> Vec<NodeEnd> {
    recoverable
        .iter()
        .filter_map(|node| {
            cluster.controller(*node).map(|ctl| NodeEnd {
                node: *node,
                applied: ctl.state().applied_index(),
                digest: ctl.state().digest(),
            })
        })
        .collect()
}

/// A heal that used its budget.
fn stalled(
    stage: Stage,
    cluster: &Cluster,
    began: VirtualTime,
    tick_ms: u64,
    clients: &Clients,
    recoverable: &[NodeId],
    marker: Option<RequestId>,
) -> Heal {
    Heal::Stalled(Stalled {
        stage,
        ticks: ticks_since(cluster, began, tick_ms),
        unresolved: clients.in_flight() + clients.parked() + clients.lookups_open(),
        lagging: marker.map_or_else(Vec::new, |id| lagging(cluster, recoverable, id)),
    })
}
