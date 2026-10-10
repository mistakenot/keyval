//! The semantic conformance corpus (PLAN.md §10.8, W31.2).
//!
//! Every test here is HTTP only: it speaks to a base URL and reads nothing of the target's
//! internals. A test runs against the in-process reference model server (§10.14) on every
//! run, and in addition against each real target the environment names and that serves every
//! operation the test exercises ([`target::active_kinds`]). A target that does not yet serve
//! an operation is not asked to (§10.8), so a gate never demands functionality that does not
//! exist; the model, which serves everything, is the proof that each test is itself right.
//!
//! The oracle crates (`model`, `checker`) are imported only under this module (T-12): the
//! model to host the reference server, the checker for the K-10 witness.

#![cfg(test)]

// check: CONF-SEMANTIC
mod cas;
mod encoding;
mod headers;
mod limits;
mod scan;
mod status;
mod support;
mod witness;
mod writes;
