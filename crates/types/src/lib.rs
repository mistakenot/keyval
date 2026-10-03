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
//! The domain types, newtypes, ports and error type arrive with W04. This crate also owns
//! [`invariant!`], the project's CD-7 assertion macro and the only sanctioned panic in
//! first-party production code (CD-8).
//!
//! As a deterministic crate it obeys the determinism rules of §7.14 and §11.5 rule 4: no
//! wall clock, no unseeded or thread-local randomness, no threads or async, no randomly
//! hashed maps and no direct I/O. Its `clippy.toml` and the lint block above enforce this
//! (checks LINT-TIERS and LINT-PROBES).

mod invariant;
