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

//! keyval's node controller (PLAN.md §7.2, §7.4, §7.7, §7.10–§7.12; W14), a deterministic
//! engine crate: one node's serial core.
//!
//! [`Controller`] is the core's single entry point ([`Inbound::step`](types::Inbound)): the
//! simulation and production composition roots both drive it with the same inputs and
//! execute the effects it returns, so the same policy runs in both (P3).
//!
//! - **Start** ([`Controller::start`], §7.7 steps 5 to 7): from the recovered journal, the
//!   committed entries are replayed into the state machine, the library is constructed at
//!   that applied index, a torn tail is truncated and synced (DUR-6), and the node is
//!   `Ready`.
//! - **The inbound queue** (§7.11): client attempts and peer envelopes wait in one bounded
//!   queue of `controller_queue_inputs` (1,024) inputs. When it is full, a peer envelope is
//!   dropped and counted, and a client attempt is answered `busy`. Ticks merge into a
//!   pending count, and completions are handled at once, so neither is ever dropped.
//! - **Turns** (§7.10, §7.12): each input is admitted, then turns run until nothing more
//!   can progress. A turn processes at most [`TICKS_PER_TURN`] ticks, then one input. A root
//!   admits a burst, such as the ticks overdue after a pause, with
//!   [`Controller::admit`] and steps its last input, so those ticks are interleaved with
//!   the queued inputs instead of starving them.
//! - **Ticks and deadlines** (§7.8, §7.10): the node's logical time is the ticks it has
//!   processed. Every processed tick fires the deadlines due, even while a sync is
//!   outstanding, so write deadlines always fire. The library's tick waits until the loop
//!   can take it.
//! - **Raft** (§7.4, §8.2): the [`ReadyLoop`](raft_adapter::ReadyLoop) is fed only while
//!   no sync is outstanding. Peer envelopes are opened before their message is stepped,
//!   and every message the loop sends is sealed into an envelope before it leaves the core.
//! - **Incarnations** (§7.2): a completion from another incarnation is ignored and counted.
//! - **Failure** (DM-7, §7.11): a loop error or a failed storage effect moves the node to
//!   `failed`. It stops feeding the library and drops peer envelopes, counted.
//!
//! The request lifecycle is the policy behind [`Requests`]. [`RequestLifecycle`], the one the
//! roots use, routes writes to the [`WritePath`] (the F-7 decision order and the attempt
//! state machine, W14.2) and reads to the [`ReadPath`] (W14.3). The controller hands it client
//! attempts, applied entries, expired deadlines, status changes and the shutdown, and
//! proposes the commands it asks for, one per turn.
//!
//! No queue, map or counter here grows without a registered bound (CD-9): the queue is
//! bounded by `controller_queue_inputs`, the deadline book by `pending_attempts`, and the
//! tick counts are integers.

#[cfg(test)]
mod attempt_table_tests;
mod controller;
#[cfg(test)]
mod controller_tests;
mod deadlines;
mod lifecycle;
#[cfg(test)]
mod lifecycle_tests;
mod queue;
#[cfg(test)]
mod queue_tests;
#[cfg(test)]
mod race_tests;
mod reads;
mod requests;
#[cfg(test)]
mod startup_tests;
#[cfg(test)]
mod status_tests;
#[cfg(test)]
mod test_support;
mod write_path;
#[cfg(test)]
mod write_path_tests;

pub use controller::{Boot, Controller, Counters, Failure, StartError, TICKS_PER_TURN};
pub use deadlines::DeadlinesFull;
pub use lifecycle::RequestLifecycle;
pub use reads::ReadPath;
pub use requests::{Applying, Proposal, Requests, View};
pub use write_path::WritePath;
