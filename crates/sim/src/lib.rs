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
//! - [`replay`]: checker K-14, replay identity: the same seed in fresh OS processes gives
//!   byte-identical canonical traces, and a mismatch reports its first differing event.
//!
//! The composition of the cores (W17) builds on these.

pub mod disk;
pub mod entropy;
pub mod god_view;
pub mod net;
#[cfg(test)]
mod net_tests;
pub mod replay;
pub mod scheduler;
pub mod trace;

pub use disk::{Crash, DiskHandle, Fill, SimDisk};
pub use entropy::{MasterSeed, NodeStreams, SplitMix64, Subsystem};
pub use god_view::{Changes, GodView, JournalView, NodeView};
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
