// Deterministic-crate lint block (PLAN.md §5.2, §7.14). Source of truth:
// xtask/lint/deterministic-lints.rs. Every deterministic crate's lib.rs starts with this
// exact block (check LINT-TIERS); `forbid` means no `allow` can override it (§11.5 rule 11).
#![forbid(
    unsafe_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::unreachable,
    clippy::dbg_macro,
    clippy::wildcard_enum_match_arm,
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::disallowed_macros
)]

//! keyval's replicated state machine (PLAN.md §7.5), a pure domain crate.
//!
//! - **[`StateMachine`]**: the ordered map `key → (value, version)`, the request history
//!   and the applied index.
//!   [`StateMachine::apply`] is the only place writes change state. It is a pure,
//!   deterministic function of (state, entry), so replaying the committed log rebuilds the
//!   same state and the same results (D-10).
//! - **[`codec`]**: the canonical, versioned command encoding that log entries carry
//!   ([`Command`], at most 66 KiB).
//! - **[`StateLimits`]**: the live-state quota (DM-6), judged at apply against committed
//!   state: a write that would create a key past `max_live_keys`, or grow the key+value
//!   bytes past `max_live_bytes`, gets the terminal result `capacity_exceeded`, keys first.
//!   The state machine also keeps the committed-command-bytes counter that history
//!   admission reads (DM-7).
//! - **[`History`]**: each request ID's first execution, `request_id → (first_index,
//!   command_digest, terminal_result)`, kept forever (DM-5). The same command again is a
//!   replay of the stored result; another command under the same ID is a conflict (C-6).
//!   Digests are SHA-256 over the canonical bytes ([`CommandDigest`], D-75), and conflict
//!   detection rests on its collision resistance, a stated Assumption (§10.15).
//! - **[`StateDigest`]**: [`StateMachine::digest`], SHA-256 over a canonical encoding of the
//!   map and the request history: the applied state that convergence (K-7) and the god
//!   view (T-5) compare. The applied index is not part of it.
//!
//! Versions are execution indices (DM-3): a PUT at entry *i* gives its key version *i*,
//! so versions strictly increase per key and have gaps.
//!
//! W09 built this crate in steps: empty entries and PUT (W09.1), the request history
//! (W09.2), the live-state quota and committed-bytes counter (W09.3), and the armed
//! invariant set with its property tests (W09.4). Every invariant names its ID, the node and
//! the entry, and a corrupted-state fixture proves each one fires. DELETE and compare-and-swap apply in W27. Until
//! then no node admits them, and a committed one fails the `SM-COMMAND-ADMITTED`
//! invariant.
//!
//! As a deterministic crate it obeys §7.14 and §11.5 rule 4: no clock, randomness,
//! threads, hashed maps or I/O. Its `clippy.toml` and the lint block above enforce this.

mod apply;
mod capacity;
pub mod codec;
mod digest;
mod history;

#[cfg(test)]
mod properties;

pub use apply::{Applied, StateMachine, Stored};
pub use capacity::StateLimits;
pub use codec::Command;
pub use digest::StateDigest;
pub use history::{CommandDigest, History, Record};
