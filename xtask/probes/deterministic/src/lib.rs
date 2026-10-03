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
//! Lint probes for deterministic crates (PLAN.md §5.2, §7.14, W01.3).
//!
//! This crate carries exactly the deterministic-crate configuration: the lint block above
//! and a copy of `xtask/lint/deterministic.clippy.toml` (check LINT-TIERS). Each module
//! gated on `keyval_probe` holds a deliberate violation. The probe runner (`xtask` tests in
//! `lint::probes`, check LINT-PROBES) compiles it with `--cfg keyval_probe="<name>"` and
//! asserts that clippy rejects it. The `disallowed` probe uses every entry of the
//! canonical determinism configuration, and the runner asserts that each one fires.
//! Normal builds compile only [`controls`], code the policy must accept.

pub mod controls;

#[cfg(keyval_probe = "wildcard_arm")]
pub mod wildcard_arm;

#[cfg(keyval_probe = "binding_catch_all_arm")]
pub mod binding_catch_all_arm;

#[cfg(keyval_probe = "missing_docs")]
pub mod missing_docs;

#[cfg(keyval_probe = "allow_under_forbid")]
pub mod allow_under_forbid;

#[cfg(keyval_probe = "disallowed")]
pub mod disallowed;
