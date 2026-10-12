//! Swarm configurations (PLAN.md §10.4, W37.2): which fault types a seed enables and at what
//! intensity.
//!
//! "Swarm testing" varies the mix: each seed chooses a **non-empty subset** of the §9.1 fault
//! types and an intensity (low, medium or high), because a run that enables every fault at
//! once rarely lets the system reach the states a narrower mix does. A **no-fault control**
//! runs too: [`Swarm::Control`] enables nothing, so a run's own scenario is the baseline the
//! faulty ones are compared with.
//!
//! The choice is data ([`Swarm`]), drawn from one stream ([`Swarm::draw`]), written to the run
//! manifest ([`Swarm::manifest`]) and to the scenario file ([`crate::scenario`]), and read back
//! from it ([`Swarm::from_json`]).

use serde_json::{Value, json};

use crate::entropy::SplitMix64;

/// A fault type of §9.1's table, as a swarm enables it. Several kinds share one §9.1 row
/// where the row has several mechanisms (message loss, delay and duplication are one network
/// row each).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FaultKind {
    /// A node crashes and restarts, keeping a seed-chosen part of its unsynced bytes. The
    /// crash is timed, or armed at a crash point (before a sync, after a sync, before a reply).
    CrashRestart,
    /// Every node crashes at once and restarts, disks intact.
    AllNodeCrash,
    /// Up to two nodes are removed for good.
    PermanentLoss,
    /// The voters split into groups that reach only their own members (3/2, 4/1, 2/2/1).
    SymmetricPartition,
    /// Directed links drop everything: one link, all in, all out, or a directed cycle.
    AsymmetricPartition,
    /// Messages dropped with a probability, on peer links and client paths.
    MessageLoss,
    /// Wide random per-message delay, so delivery follows delay and not send order.
    DelayReorder,
    /// Messages delivered more than once.
    Duplication,
    /// A node stops taking turns for a while and then resumes.
    Pause,
}

impl FaultKind {
    /// Every kind, in the fixed order the file and the manifest list them.
    pub const ALL: [Self; 9] = [
        Self::CrashRestart,
        Self::AllNodeCrash,
        Self::PermanentLoss,
        Self::SymmetricPartition,
        Self::AsymmetricPartition,
        Self::MessageLoss,
        Self::DelayReorder,
        Self::Duplication,
        Self::Pause,
    ];

    /// The kind's name in scenario files and manifests.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CrashRestart => "crash_restart",
            Self::AllNodeCrash => "all_node_crash",
            Self::PermanentLoss => "permanent_loss",
            Self::SymmetricPartition => "symmetric_partition",
            Self::AsymmetricPartition => "asymmetric_partition",
            Self::MessageLoss => "message_loss",
            Self::DelayReorder => "delay_reorder",
            Self::Duplication => "duplication",
            Self::Pause => "pause",
        }
    }

    /// The kind called `name`, if there is one.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// The kind's bit in a [`FaultSet`].
    const fn bit(self) -> u16 {
        match self {
            Self::CrashRestart => 1,
            Self::AllNodeCrash => 1 << 1,
            Self::PermanentLoss => 1 << 2,
            Self::SymmetricPartition => 1 << 3,
            Self::AsymmetricPartition => 1 << 4,
            Self::MessageLoss => 1 << 5,
            Self::DelayReorder => 1 << 6,
            Self::Duplication => 1 << 7,
            Self::Pause => 1 << 8,
        }
    }
}

/// A non-empty set of [`FaultKind`]s.
///
/// Invariant: at least one kind (CD-2): the constructors refuse the empty set, so a faulty
/// swarm cannot enable nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultSet(u16);

impl FaultSet {
    /// The set of `kinds`, or `None` if there are none.
    #[must_use]
    pub fn new(kinds: &[FaultKind]) -> Option<Self> {
        let bits = kinds.iter().fold(0, |bits, kind| bits | kind.bit());
        (bits != 0).then_some(Self(bits))
    }

    /// Whether `kind` is enabled.
    #[must_use]
    pub const fn contains(self, kind: FaultKind) -> bool {
        self.0 & kind.bit() != 0
    }

    /// The enabled kinds, in [`FaultKind::ALL`] order.
    #[must_use]
    pub fn kinds(self) -> Vec<FaultKind> {
        FaultKind::ALL
            .into_iter()
            .filter(|kind| self.contains(*kind))
            .collect()
    }
}

/// How hard the enabled faults hit (§10.4): how many fault episodes the chaos phase holds,
/// how long it lasts and how severe a network fault is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Intensity {
    /// A few brief episodes.
    Low,
    /// A moderate number.
    Medium,
    /// Many, overlapping.
    High,
}

impl Intensity {
    /// Every intensity, mildest first.
    pub const ALL: [Self; 3] = [Self::Low, Self::Medium, Self::High];

    /// The intensity's name in files and manifests.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// The intensity called `name`, if there is one.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|intensity| intensity.name() == name)
    }

    /// The fault episodes the chaos phase holds.
    #[must_use]
    pub const fn episodes(self) -> u64 {
        match self {
            Self::Low => 2,
            Self::Medium => 5,
            Self::High => 10,
        }
    }

    /// The chaos phase's length, in virtual milliseconds.
    #[must_use]
    pub const fn chaos_ms(self) -> u64 {
        match self {
            Self::Low => 3_000,
            Self::Medium => 6_000,
            Self::High => 10_000,
        }
    }

    /// The probability a message is lost during a loss episode, in parts per million.
    #[must_use]
    pub const fn loss_ppm(self) -> u32 {
        match self {
            Self::Low => 50_000,
            Self::Medium => 150_000,
            Self::High => 300_000,
        }
    }

    /// The probability a message is duplicated during a duplication episode, in parts per
    /// million.
    #[must_use]
    pub const fn duplicate_ppm(self) -> u32 {
        match self {
            Self::Low => 100_000,
            Self::Medium => 300_000,
            Self::High => 600_000,
        }
    }

    /// The longest message delay during a delay episode, in virtual microseconds.
    #[must_use]
    pub const fn max_delay_us(self) -> u64 {
        match self {
            Self::Low => 20_000,
            Self::Medium => 100_000,
            Self::High => 400_000,
        }
    }
}

/// A seed's swarm configuration.
///
/// Invariant: a faulty swarm's fault set is non-empty ([`FaultSet`]); the control has none
/// (CD-2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Swarm {
    /// The no-fault control: nothing is injected.
    Control,
    /// Some fault types, at an intensity.
    Faulty {
        /// The enabled fault types.
        faults: FaultSet,
        /// How hard they hit.
        intensity: Intensity,
    },
}

impl Swarm {
    /// A swarm drawn from `rng`: each fault type is enabled with probability one half, and
    /// if none came up one is enabled anyway; the intensity is uniform.
    #[must_use]
    pub fn draw(rng: &mut SplitMix64) -> Self {
        let mut kinds: Vec<FaultKind> = FaultKind::ALL
            .into_iter()
            .filter(|_| rng.next_u64() & 1 == 1)
            .collect();
        if kinds.is_empty() {
            let at = usize::try_from(rng.next_u64() % FaultKind::ALL.len() as u64).unwrap_or(0);
            kinds.push(FaultKind::ALL[at]);
        }
        let at = usize::try_from(rng.next_u64() % Intensity::ALL.len() as u64).unwrap_or(0);
        FaultSet::new(&kinds).map_or(Self::Control, |faults| Self::Faulty {
            faults,
            intensity: Intensity::ALL[at],
        })
    }

    /// A swarm of exactly `kinds` (a fault-dedicated profile's), at an intensity drawn from
    /// `rng`; the control if `kinds` is empty.
    #[must_use]
    pub fn dedicated(rng: &mut SplitMix64, kinds: &[FaultKind]) -> Self {
        let at = usize::try_from(rng.next_u64() % Intensity::ALL.len() as u64).unwrap_or(0);
        FaultSet::new(kinds).map_or(Self::Control, |faults| Self::Faulty {
            faults,
            intensity: Intensity::ALL[at],
        })
    }

    /// Whether `kind` is enabled.
    #[must_use]
    pub const fn enables(self, kind: FaultKind) -> bool {
        match self {
            Self::Control => false,
            Self::Faulty { faults, .. } => faults.contains(kind),
        }
    }

    /// The manifest's record of the swarm (§10.4): whether it is the control, the enabled
    /// fault types and the intensity.
    #[must_use]
    pub fn manifest(self) -> Value {
        match self {
            Self::Control => json!({"control": true, "faults": [], "intensity": null}),
            Self::Faulty { faults, intensity } => json!({
                "control": false,
                "faults": faults.kinds().into_iter().map(FaultKind::name).collect::<Vec<_>>(),
                "intensity": intensity.name(),
            }),
        }
    }

    /// The swarm a [`Self::manifest`] records.
    ///
    /// # Errors
    ///
    /// A message naming what is wrong: a missing field, an unknown fault type or intensity,
    /// or a faulty swarm with no fault types, or a control that lists some.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        if let Some(key) = value.as_object().and_then(|object| {
            object
                .keys()
                .find(|key| !["control", "faults", "intensity"].contains(&key.as_str()))
        }) {
            return Err(format!("swarm has an unknown field {key:?}"));
        }
        let control = value["control"]
            .as_bool()
            .ok_or("swarm.control must be a boolean")?;
        let names = value["faults"]
            .as_array()
            .ok_or("swarm.faults must be an array")?;
        let mut kinds = Vec::new();
        for name in names {
            let name = name.as_str().ok_or("swarm.faults holds only strings")?;
            kinds.push(
                FaultKind::parse(name).ok_or_else(|| format!("unknown fault type {name:?}"))?,
            );
        }
        if control {
            return if kinds.is_empty() && value["intensity"].is_null() {
                Ok(Self::Control)
            } else {
                Err("a control swarm enables no faults and has no intensity".to_owned())
            };
        }
        let intensity = value["intensity"]
            .as_str()
            .ok_or("swarm.intensity must be a string")?;
        let intensity = Intensity::parse(intensity)
            .ok_or_else(|| format!("unknown intensity {intensity:?}"))?;
        let faults =
            FaultSet::new(&kinds).ok_or("a faulty swarm enables at least one fault type")?;
        Ok(Self::Faulty { faults, intensity })
    }
}
