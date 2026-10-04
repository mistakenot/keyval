//! keyval's reference model (PLAN.md §10.14, T-7): a deliberately simple, obviously
//! correct model of the API's semantics, used only for testing.
//!
//! It is written from the plan's prose (§6.2–§6.4, §7.5's apply steps, §7.11's SCAN page
//! rule, §8) and the API spec, never from the production state machine, and it depends
//! on no production crate, not even the domain types (T-12): [`data`] defines its own
//! keys, values, request IDs, indices and versions. Checkers (K-4, K-5, K-10),
//! differential tests and the reference model server judge the system against it.
//!
//! - [`Model`]: a single-threaded, in-memory ordered map plus a request history. It
//!   applies committed log entries one at a time, in log order, and says what each
//!   decided: a version, whether a key existed, a CAS mismatch with the current version,
//!   a capacity refusal, a replay or a conflict ([`apply`]).
//! - [`Model::scan`]: SCAN pages over the current state ([`scan`]).
//!
//! No consensus, no I/O, no clocks, no performance concerns and no cleverness: where a
//! simpler formulation exists, the model uses it. For instance, it detects a conflicting
//! retry by comparing whole commands, which is what the production digest stands for.
//! When the model and the state machine disagree, the plan decides which is wrong.

pub mod apply;
pub mod data;
pub mod scan;
pub mod server;

pub use apply::{
    Applied, ApplyError, Command, Decision, Entry, Expected, LiveLimit, Model, Op, TerminalResult,
};
pub use data::{
    DataError, Index, Key, Limits, MAX_INTEGER, MAX_KEY_BYTES, MAX_VALUE_BYTES, RequestId, Value,
    Version,
};
pub use scan::{
    DEFAULT_PAGE_ITEMS, Item, MAX_PAGE_BYTES, MAX_PAGE_ITEMS, Page, ScanError, ScanRequest,
};
