//! History checkers (PLAN.md §10.5; W38): independent oracles that judge one run's
//! observations, `keyval.observation/v1` (`schemas/observation/`), and nothing else (T-12).
//!
//! The checkers depend on no production crate. They parse the document into their own view
//! ([`History`]) and take first executions, versions and decisions from the reference model's
//! replay of the committed commands in index order, never from a node's claim (§10.14).
//!
//! Implemented: [`k1`] (durability) and [`k2`] (exactly-once). Each returns a [`Report`]:
//! how many cases it examined and every [`Violation`] it found. A document the checkers
//! cannot read is a [`CheckError`], never a silent pass (P10).

mod history;
mod k1;
mod k2;
mod report;

#[cfg(test)]
mod fixtures;

pub use history::{Claim, Committed, History, LogEntry};
pub use k1::k1;
pub use k2::k2;
pub use report::{CheckError, Kind, Report, Violation};
