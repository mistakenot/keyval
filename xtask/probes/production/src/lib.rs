//! Lint probes for production-tier crates (PLAN.md §5.2, W01.3).
//!
//! Each module gated on `keyval_probe` holds one deliberate violation of the workspace
//! lint policy. It is compiled only when the probe runner (`xtask` tests in
//! `lint::probes`, check LINT-PROBES) passes `--cfg keyval_probe="<name>"`, and the runner
//! asserts that clippy rejects it with the expected lint. Normal builds compile only
//! [`controls`], code the policy must accept, so every workspace lint run re-proves them.

pub mod controls;

#[cfg(keyval_probe = "unsafe_code")]
pub mod unsafe_code;

#[cfg(keyval_probe = "unwrap_used")]
pub mod unwrap_used;

#[cfg(keyval_probe = "expect_used")]
pub mod expect_used;

#[cfg(keyval_probe = "panic")]
pub mod panic;

#[cfg(keyval_probe = "todo")]
pub mod todo;

#[cfg(keyval_probe = "unimplemented")]
pub mod unimplemented;

#[cfg(keyval_probe = "unreachable")]
pub mod unreachable;

#[cfg(keyval_probe = "dbg_macro")]
pub mod dbg_macro;

#[cfg(keyval_probe = "group_all")]
pub mod group_all;

#[cfg(keyval_probe = "group_pedantic")]
pub mod group_pedantic;

#[cfg(keyval_probe = "group_nursery")]
pub mod group_nursery;
