//! History checkers (PLAN.md §10.5; W38): independent oracles that judge one run's
//! observations, `keyval.observation/v1` (`schemas/observation/`), and nothing else (T-12).
//!
//! The checkers depend on no production crate. They parse the document into their own view
//! ([`History`]) and take first executions, versions and decisions from the reference model's
//! replay of the committed commands in index order, never from a node's claim (§10.14).
//!
//! Implemented: [`k1`] (durability), [`k10`] (black-box linearizability witness), [`k2`] (exactly-once), [`k3`] (real-time order), [`k4`]
//! (model-replay results), [`k8`] (Raft safety, judged on
//! the [`GodView`] with commit certificates), [`k11`] (persistence ordering, on the same view)
//! and [`k12`] (unknown-outcome honesty, on the client-side [`Trace`]). Each returns a [`Report`]:
//! how many cases it examined and every [`Violation`] it found. A document the checkers
//! cannot read is a [`CheckError`], never a silent pass (P10).

mod godview;
mod history;
mod k1;
mod k10;
mod k11;
mod k12;
mod k2;
mod k3;
mod k4;
mod k8;
mod report;
mod trace;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod raft_fixtures;

pub use godview::{
    Durable, Event, GodView, NodeState, Obligation, Process, Released, Role, Stamped, SyncState,
};
pub use history::{Claim, Committed, History, LogEntry};
pub use k1::k1;
pub use k2::k2;
pub use k3::k3;
pub use k4::k4;
pub use k8::k8;
pub use k10::k10;
pub use k11::k11;
pub use k12::k12;
pub use report::{CheckError, Kind, Report, Violation};
pub use trace::{Answer, AttemptState, Item, Located, Resolved, Trace};
