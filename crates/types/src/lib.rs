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
//! keyval's domain types (deterministic core: pure domain tier, PLAN.md §7.1 and
//! Appendix A).
//!
//! - **Newtypes** (CD-4): [`NodeId`], [`ClusterId`], [`Term`], [`LogIndex`], [`Version`],
//!   [`Tick`], [`RequestId`], [`ByteCount`] and [`PageSize`]. Each kind of quantity has
//!   its own type, conversions are explicit, and arithmetic is checked.
//! - **Smart constructors** (CD-3): [`Key::parse`] (DM-1) and [`Value::parse`] (DM-2)
//!   enforce the configured byte limits ([`KeyValueLimits`], D-80) once, at the boundary; fields are private, so the core
//!   never re-checks.
//! - **Domain enums** (CD-2): [`Lifecycle`], [`ExpectedVersion`], [`Mutation`], the
//!   write's [`Decision`] and [`TerminalResult`], and an [`Attempt`]'s state (§7.5, §7.8).
//! - **Error codes** (CD-6): [`ErrorCode`] has one variant per §8.4 code, matching
//!   `api/openapi.yaml` exactly.
//!
//! Integers that appear in the HTTP API (node IDs, terms, log indices, versions) are
//! refused at or above 2^53 when constructed ([`MAX_SAFE_INTEGER`], D-77).
//!
//! - **Ports** (CD-1, §7.2), all in core-owned types: the inbound port [`Inbound::step`]
//!   turns an [`Input`] into ordered [`Effect`] values, and every storage effect can be
//!   reported failed ([`StorageOp`]); the in-process ports are [`Entropy`],
//!   [`JournalRead`] and the [`DataDir`] lock; and [`Consensus`] isolates the Raft
//!   library, so no library type crosses it.
//!
//! - **Config** (O-5, CD-3, §7.10): [`Config::parse`] reads the one static config file
//!   with a strict in-crate reader and refuses every §7.10 inconsistency with a specific
//!   [`ConfigError`]. Each numeric key's allowed range is declared once, in
//!   [`ConfigParam`], and [`ConsensusSettings`] carry the consensus library's settings
//!   in core-owned types. [`Config::semantic_fingerprint`] digests every value that can
//!   change a result, plus the voter set (DUR-8).
//! - **Limits** (CD-9, §7.11): [`Limit`] names every bound once, configured or fixed,
//!   and [`BoundedQueue`] and [`BoundedMap`] enforce them with typed refusals.
//!   [`Config::exposure_warnings`] reports each non-loopback address (O-9).
//! - **SHA-256** ([`Sha256`], D-75): in-crate, for the fingerprint and command digests.
//!
//! This crate also owns [`invariant!`], the project's CD-7 assertion macro and the only
//! sanctioned panic in first-party production code (CD-8).
//!
//! As a deterministic crate it obeys the determinism rules of §7.14 and §11.5 rule 4: no
//! wall clock, no unseeded or thread-local randomness, no threads or async, no randomly
//! hashed maps and no direct I/O. Its `clippy.toml` and the lint block above enforce this
//! (checks LINT-TIERS and LINT-PROBES).

mod bounded;
mod client;
mod config;
mod consensus;
mod counts;
mod data;
mod effects;
mod error_code;
mod ids;
mod in_process;
mod invariant;
mod lifecycle;
mod limits;
mod log_index;
mod mutation;
mod outcome;
mod range;
mod sha256;
mod telemetry;

pub use bounded::{BoundedMap, BoundedQueue, MapFull, QueueFull, QueueRefused};
pub use client::{
    AttemptId, ClientOp, HistoryLimit, Refusal, Response, ScanItem, ScanPage, ScanRange,
    ScanRangeError, ServedBy,
};
pub use config::{
    AddressProblem, AddressRole, COMMAND_ENVELOPE_BOUND, COMMAND_FORMAT_VERSION, Config,
    ConfigError, ConfigParam, ConfigParamClass, ConfigSyntax, ConfigTable, ConfigValueKind,
    ConsensusSettings, ConsensusViolation, ENTRY_ENVELOPE_BOUND, ExposureWarning, FRAME_FIELDS,
    FRAME_OVERHEAD_BYTES, JOURNAL_PROTOCOL_ALLOWANCE, MAX_COMMAND_BYTES, MAX_CONFIG_BYTES,
    MAX_FRAME_PAYLOAD_BYTES, MAX_REQUEST_BODY_BYTES, MAX_SCAN_PAGE_BYTES, NodeConfig,
    PER_ENTRY_OVERHEAD_BOUND, PROTOCOL_VERSION, ReadMode, SEMANTIC_ENCODING, SemanticFingerprint,
};
pub use consensus::{
    Consensus, Entry, EntryPayload, HardState, LightReady, OutboundMessage, Progress, ProposeError,
    Ready, Role, Status, StepError,
};
pub use counts::{ByteCount, PageSize, Tick};
pub use data::{Key, KeyError, KeyValueLimits, Value, ValueError};
pub use effects::{Effect, Inbound, Incarnation, Input, StorageOp, SyncSeq};
pub use error_code::{ErrorCode, Terminality};
pub use ids::{ClusterId, ClusterIdError, NodeId, RequestId, RequestIdError};
pub use in_process::{DataDir, Entropy, EntropyStream, JournalRead, ReadLen, StorageError};
pub use lifecycle::{Lifecycle, NotReadyLifecycle};
pub use limits::{Limit, LimitSource};
pub use log_index::{EntryIndex, LogIndex, Term, Version};
pub use mutation::{ExpectedVersion, Mutation};
pub use outcome::{Attempt, AttemptOutcome, Decision, LiveStateLimit, TerminalResult};
pub use range::{MAX_SAFE_INTEGER, OutOfRange, Quantity};
pub use sha256::Sha256;
pub use telemetry::{DroppedInput, LogEvent, LogLevel, Observation};
