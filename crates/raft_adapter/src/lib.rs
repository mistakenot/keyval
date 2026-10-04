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

//! keyval's `raft-rs` adapter (PLAN.md §7.3, §7.4; W13), a deterministic engine crate.
//!
//! [`RaftNode`] implements the core's [`Consensus`](types::Consensus) port over the vendored,
//! entropy-patched `RawNode` (W02). No library type crosses the port (CD-1): entries, hard
//! state and status are core types, and messages travel as encoded bytes.
//!
//! - **Configuration** ([`library_config`]): the validated consensus settings map onto the
//!   library's `Config`. Pre-vote and check-quorum are on, and every §7.11 bound is set,
//!   including `max_committed_size_per_ready`, which the library leaves unlimited by default.
//!   Every config the parser accepts constructs a `RawNode` (D-78), and a test builds one for
//!   every committed fixture config.
//! - **Entropy** ([`EntropySource`]): the core's entropy port is the patched library's
//!   mandatory entropy source, so randomised election timeouts are seeded (P4).
//! - **Storage** ([`JournalStorage`]): the library reads its log through the journal's
//!   logical log, meaning its offset index and entry cache (§7.4 step 4). Its
//!   `initial_state` is the recovered hard state plus the static voter set, because
//!   membership never comes from the log in v1 (§7.7 step 6).
//! - **Logging**: an explicit logger with a discarding drain. There is no background thread
//!   and no output (§7.3).
//!
//! The adapter takes one Ready at a time. [`RaftNode::persist`] turns it into one journal
//! frame, or none when it has nothing to sync, and [`advance`](types::Consensus::advance)
//! returns the follow-up work. W13.2 builds the full Ready loop around these: sync gating,
//! release order and draining. W13.3 adds the peer-envelope codec.

mod config;
mod convert;
mod entropy;
mod node;
#[cfg(test)]
mod node_tests;
mod storage;
#[cfg(test)]
mod test_support;

pub use config::library_config;
pub use entropy::EntropySource;
pub use node::{ConstructError, RaftNode};
pub use storage::JournalStorage;
