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
//!
//! The canonical trace and god view (W15.2), replay identity (W15.3), the simulated
//! network (W16) and disk (W12), and the composition of the cores (W17) build on these.

pub mod entropy;
pub mod scheduler;

pub use entropy::{MasterSeed, NodeStreams, SplitMix64, Subsystem};
pub use scheduler::{
    Event, EventBudget, EventSeq, InThePast, NodeIncarnation, Scheduler, Step, VirtualTime,
};
