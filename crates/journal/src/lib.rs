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
//! keyval's journal engine (deterministic core: engine, PLAN.md §7.6 and Appendix A).
//!
//! The journal is the one durable authority (CD-10): one append-only file per node,
//! written as frames, from which everything else is derived by replay.
//!
//! - [`crc32c()`]: CRC-32C, implemented in-crate and tested against the RFC 3720 vectors
//!   (CD-12).
//! - The frame codec ([`encode`], [`decode`], [`decode_header`]) implements the §7.6
//!   layout. It has a header checksum, so an intact header's length can be trusted
//!   (D-69), and a specific [`FrameError`] for every kind of damage.
//! - The records ([`Genesis`], [`RaftBatch`]) have canonical payloads, pinned
//!   byte-for-byte by the golden vectors in `golden/` (DUR-5).
//!
//! As a deterministic crate it does no I/O: bytes reach it, and leave it, through the
//! byte-storage port (§7.2).
//! - [`Genesis::verify`] refuses a start under a conflicting node, voter set or semantic
//!   configuration, modifying nothing (DUR-8, §7.7 step 2).
//! - [`LogicalLog`] gives the frames their meaning: append, logical truncation (never at
//!   or below the persisted commit), the offset index, the persisted hard state, and a
//!   bounded entry cache whose eviction never changes results.
//!
//! - [`recover`] is the fail-closed recovery scan (§7.6, §7.7): it verifies the genesis,
//!   replays the frames through [`LogicalLog::apply`], and tells a torn tail, which the
//!   core truncates, from corruption, which it refuses (D-60, D-69).

mod crc32c;
mod frame;
#[cfg(test)]
mod golden;
#[cfg(test)]
mod golden_recovery;
mod log;
#[cfg(test)]
mod log_tests;
mod record;
mod recovery;
#[cfg(test)]
mod recovery_tests;

pub use crc32c::{crc32c, update as crc32c_update};
pub use frame::{
    FORMAT_VERSION, Frame, FrameError, FrameKind, HEADER_LEN, Header, MAGIC, TRAILER_LEN, decode,
    decode_header, encode,
};
pub use log::{JournalError, LogBounds, LogicalLog, MIN_ENTRY_RECORD_BYTES};
pub use record::{Genesis, GenesisMismatch, RaftBatch, RecordError};
pub use recovery::{MAX_FRAME_LEN, Recovered, RecoveryError, recover};
