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
//! - **[`History`]**: each request ID's first execution, `request_id → (first_index,
//!   command_digest, terminal_result)`, kept forever (DM-5). The same command again is a
//!   replay of the stored result; another command under the same ID is a conflict (C-6).
//!   Digests are SHA-256 over the canonical bytes ([`CommandDigest`], D-75), and conflict
//!   detection rests on its collision resistance, a stated Assumption (§10.15).
//!
//! Versions are execution indices (DM-3): a PUT at entry *i* gives its key version *i*,
//! so versions strictly increase per key and have gaps.
//!
//! W09 grows this crate in steps: empty entries and PUT (W09.1), and the request history
//! (W09.2). The live-state quota and committed-bytes counter (W09.3) and the armed
//! invariant set (W09.4) follow. DELETE and compare-and-swap apply in W27. Until
//! then no node admits them, and a committed one fails the `SM-COMMAND-ADMITTED`
//! invariant.
//!
//! As a deterministic crate it obeys §7.14 and §11.5 rule 4: no clock, randomness,
//! threads, hashed maps or I/O. Its `clippy.toml` and the lint block above enforce this.

mod apply;
pub mod codec;
mod history;

pub use apply::{Applied, StateMachine, Stored};
pub use codec::Command;
pub use history::{CommandDigest, History, Record};
