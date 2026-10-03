# DEPENDENCIES.md — keyval dependency ledger (CD-12)

Every **direct** dependency of every workspace crate is listed here with its purpose,
scope, version, licence and source, in the same commit that adds it (PLAN.md §5.2
CD-12, AGENTS.md "Dependencies"). This file is a shared surface (§11.7): edit it only
under a short, explicit Agent Mail reservation.

Rules:

- **Deterministic crates** (`types`, `statemachine`, `node`, `journal`,
  `raft_adapter`) start from the standard library plus the vendored Raft library
  (`vendor/raft/`, pinned 0.7.0 with recorded checksums, W02). Anything else used by a
  deterministic crate needs an entry here whose *Determinism* cell shows that it is
  pure: no ambient time, entropy, threads or I/O, checked transitively and per feature
  (P4).
- Never add an async runtime, HTTP stack, RNG or clock access to a deterministic crate.
  Those belong in adapters behind ports (§11.5 rules 4–5).
- `Cargo.lock` is committed. *Version* is the **locked** version. A version bump is its
  own bead, not a rider, and updates this table in the same commit.
- The T0 dependency-ledger check reads the table below. It fails if a workspace crate
  has a direct dependency (normal, dev or build) that is missing from the table, or that
  is not listed under that crate in *Used by*. Keep one row per crate, and keep the
  columns in this order.

## Direct dependencies

| Crate | Version | Scope | Used by | Purpose | Licence | Source | Determinism |
|---|---|---|---|---|---|---|---|
| `clap` | 4.6.7 | tools | `xtask` | Typed command-line parsing for `./dev`: subcommands, flags, usage errors (parse at the boundary, CD-3). Default features off; only `std`, `derive`, `help`, `usage`, `error-context`, `suggestions` (no colour stack). | MIT OR Apache-2.0 | crates.io | n/a: tools only |
| `serde` | 1.0.229 | tools | `xtask` | Derived serialisation for the versioned `--json` robot output of every `./dev` command (O-6). | MIT OR Apache-2.0 | crates.io | n/a: tools only |
| `serde_json` | 1.0.151 | tools | `xtask` | JSON encoding of robot output, and decoding of other tools' `--json` output (`br`, `cargo metadata`, clippy diagnostics; §11.5 rule 14). | MIT OR Apache-2.0 | crates.io | n/a: tools only |
| `sha2` | 0.11.0 | tools | `xtask` | SHA-256 for gate receipts: the build ID (§10.2 run manifest) and policy-file hashes (§10.10). Default features off. Tools only: the deterministic core implements SHA-256 in-crate (§7.5, D-75). | MIT OR Apache-2.0 | crates.io | n/a: tools only |
| `toml` | 1.1.6+spec-1.1.0 | tools | `xtask` | Parsing TOML: the toolchain pin in `rust-toolchain.toml` (`./dev doctor`), and later the `registry/*.toml` files (W05, W06). Default features off; only `std`, `serde`, `parse` (no writer). | MIT OR Apache-2.0 | crates.io | n/a: tools only |

*Scope* is one of: `deterministic` (may be used by deterministic crates; needs a purity
argument), `adapter` (adapters and composition roots), `oracle` (model, checker,
conformance; never a production crate), `tools` (`xtask`, `cli`), `dev` (test-only,
via `[dev-dependencies]`).
