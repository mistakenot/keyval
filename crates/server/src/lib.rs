//! `keyval-server`, keyval's production composition root (PLAN.md Appendix A, §8.7; W18).
//!
//! One OS process per node. Like every composition root it only translates (CD-1): the
//! journal engine decides what a data directory holds, and the file adapter does the I/O.
//!
//! - [`init`] initialises a node's data directory, and nothing else does (§7.7 step 0,
//!   D-52, D-63). It creates the directory and syncs its parent, takes the lock, creates
//!   the journal and syncs the directory, then writes and syncs the genesis frame, which
//!   binds the directory to the cluster instance ID, the node, the voters and the semantic
//!   configuration (DUR-7, DUR-8). It re-runs only over what an interrupted `init` can
//!   leave, and never re-initialises a directory that holds anything more.
//! - [`run`] is the start's preflight (§7.7 steps 1–4). It refuses a missing data
//!   directory or a journal without a valid genesis, and never creates either: a node that
//!   lost its data rejoining as a blank voter is Raft amnesia (D-52). It refuses a held
//!   lock, a conflicting genesis and a damaged journal too, modifying nothing.
//! - [`output`] is the robot-mode output (O-6): the `keyval.server.init/v1` report, the
//!   `keyval.server.error/v1` envelope, and the exit codes.
//!
//! Serving arrives with W18.2 (the serial controller task, clock, entropy and logs) and
//! W18.3 (HTTP). Until then `keyval-server run` stops after a successful preflight and
//! says so; it never presents itself as serving (P6).

pub mod init;
pub mod output;
pub mod run;
