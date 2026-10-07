//! keyval's deterministic simulator, the simulation composition root (PLAN.md §7.13,
//! §10.2).
//!
//! One thread drives five real node cores through simulated time. Every source of
//! nondeterminism is a seeded stream split from one master seed, so the same seed and
//! the same build give the same execution (P4, T-3).
//!
//! - [`entropy`]: the `SplitMix64` generator and the independent streams split from the
//!   master seed, per subsystem and per node incarnation.
//! - [`scheduler`]: the discrete-event queue, ordered by virtual time, seeded tie
//!   priority and insertion sequence, with the run's event budget.
//! - [`trace`]: the canonical trace, its hash and the first-difference comparison.
//! - [`god_view`]: every node's Raft, journal and applied state after every step, and
//!   the trace records for what changed.
//! - [`disk`]: the simulated disk, volatile and durable images per node with the crash
//!   model (DUR-7, DUR-9); its handle is the simulation's byte-storage adapter.
//! - [`net`]: the simulated network: seeded per-message delay, loss and duplication on
//!   peer links and client paths, a directed link matrix for partitions, and bounded link
//!   queues, carrying the same sealed peer envelopes as production (§9.1, W16).
//! - [`nemesis`]: the fault vocabulary of §9.1 (W37.1): crash points, permanent loss, pauses,
//!   partitions and link profiles as values the cluster schedules as events.
//! - [`swarm`] and [`scenario`]: swarm configurations (a non-empty fault subset and an
//!   intensity per seed, plus the no-fault control) and the scenario a seed generates inside
//!   the §9.4 healthy envelope, with its human-readable scenario file (W37.2, T-10).
//! - [`replay`]: checker K-14, replay identity: the same seed in fresh OS processes gives
//!   byte-identical canonical traces, and a mismatch reports its first differing event.
//!
//! - [`cluster`]: the composition root itself (W17.1): every voter as a real core over
//!   these adapters, crash and restart as new incarnations, every step recorded.
//! - [`slice`]: the slice scenario (W17.2) that `./dev sim run --profile slice` runs through
//!   the `keyval-sim` binary: elect, PUT and GET, a follower crash with unsynced bytes and
//!   its restart and catch-up, an audit of every mirror; its report labels it a simulated
//!   slice (P6).
//! - [`observe`]: the observation recording (W36.2): a run's client-side `keyval.observation/v1`
//!   document built from the cluster's attempt log and the clients' sends and outcomes.

pub mod clients;
#[cfg(test)]
mod clients_tests;
pub mod cluster;
#[cfg(test)]
mod cluster_tests;
pub mod disk;
pub mod entropy;
pub mod god_view;
pub mod nemesis;
#[cfg(test)]
mod nemesis_tests;
pub mod net;
#[cfg(test)]
mod net_profile_tests;
#[cfg(test)]
mod net_tests;
pub mod observe;
#[cfg(test)]
mod observe_tests;
pub mod replay;
pub mod scenario;
#[cfg(test)]
mod scenario_tests;
pub mod scheduler;
pub mod slice;
#[cfg(test)]
mod slice_tests;
pub mod swarm;
#[cfg(test)]
mod swarm_tests;
pub mod trace;

pub use clients::{
    ClientPlan, Clients, End, Intent, Outcome, POLL_MS, Plan, Planned, Profile, SentAttempt,
};
pub use cluster::{Answer, Cluster, ClusterError, Happening, Observed};
pub use disk::{Crash, DiskHandle, Fill, SimDisk, Survival};
pub use entropy::{MasterSeed, NodeStreams, SplitMix64, Subsystem};
pub use god_view::{Changes, GodView, JournalView, NodeView};
pub use nemesis::{CrashPoint, Fault, MAX_LOST, NemesisError, Partition};
pub use net::{
    Arrival, Delivery, DropReason, LinkMatrix, LinkProfile, NetConfig, NetCounts, Network,
    PacketId, Ppm, QueueBound, Sent,
};
pub use replay::{
    Divergence, MachineryRun, PROCESS_TIMEOUT, ReplayError, Report, RunError, fresh_processes,
    machinery, trace_path,
};
pub use scheduler::{
    Event, EventBudget, EventSeq, NodeIncarnation, ScheduleError, Scheduler, Shortfall, Step,
    VirtualTime,
};
pub use trace::{
    ClientId, Comparison, DIAGNOSTIC_TEXT_BYTES, DiskOp, Endpoint, INCOMPLETE_MARKER, Name,
    ObservationBound, Overflow, PacketFate, Record, Trace, TraceHash, compare, marks_incomplete,
};
