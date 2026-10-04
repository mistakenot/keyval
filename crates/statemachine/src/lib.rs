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
//! - **[`StateMachine`]**: the ordered map `key → (value, version)` and the applied index.
//!   [`StateMachine::apply`] is the only place writes change state. It is a pure,
//!   deterministic function of (state, entry), so replaying the committed log rebuilds the
//!   same state and the same results (D-10).
//! - **[`codec`]**: the canonical, versioned command encoding that log entries carry
//!   ([`Command`], at most 66 KiB).
//!
//! Versions are execution indices (DM-3): a PUT at entry *i* gives its key version *i*,
//! so versions strictly increase per key and have gaps.
//!
//! W09 grows this crate in steps. This step (W09.1) covers empty entries and PUT. The
//! request history (W09.2), the live-state quota and committed-bytes counter (W09.3), and
//! the armed invariant set (W09.4) follow. DELETE and compare-and-swap apply in W27. Until
//! then no node admits them, and a committed one fails the `SM-COMMAND-ADMITTED`
//! invariant.
//!
//! As a deterministic crate it obeys §7.14 and §11.5 rule 4: no clock, randomness,
//! threads, hashed maps or I/O. Its `clippy.toml` and the lint block above enforce this.

mod apply;
pub mod codec;

pub use apply::{Applied, StateMachine, Stored};
pub use codec::Command;
