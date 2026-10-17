//! The keyval command-line tool (PLAN.md §8.6, §8.7, O-6, O-7; Appendix A `crates/cli`).
//!
//! [`doctor`] verifies a node's data directory offline. [`output`] is the
//! `keyval.cli.error/v1` envelope every command prints for a usage or configuration error
//! under `--json`. [`client`] talks to the nodes (bounded same-ID retry), [`pending`] keeps
//! unresolved writes, and [`command`] renders each client command's versioned `--json`
//! document and text. The binary (`src/main.rs`) only parses arguments and prints;
//! everything it reports is built here, so tests call it directly as well as through the
//! binary.

pub mod client;
pub mod command;
pub mod doctor;
pub mod output;
pub mod pending;
