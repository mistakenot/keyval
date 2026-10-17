//! The keyval command-line tool (PLAN.md §8.6, O-6, O-7; Appendix A `crates/cli`).
//!
//! W23 gives it [`doctor`]: `keyval doctor <data-dir>` verifies a node's data directory
//! offline, and [`output`]: the `keyval.cli.error/v1` envelope every command prints for a
//! usage or configuration error under `--json`. W33 adds the client commands next to it. The binary (`src/main.rs`) only
//! parses arguments and prints; everything it reports is built here, so tests call it
//! directly as well as through the binary.

pub mod client;
pub mod doctor;
pub mod output;
pub mod pending;
