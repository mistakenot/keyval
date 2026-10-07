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
| `clap` | 4.6.7 | tools | `cli`, `server`, `xtask` | Typed command-line parsing for `./dev` and the `keyval` CLI: subcommands, flags, usage errors (parse at the boundary, CD-3). Default features off; only `std`, `derive`, `help`, `usage`, `error-context`, `suggestions` (no colour stack). | MIT OR Apache-2.0 | crates.io | n/a: tools only |
| `jsonschema` | 0.58.5 | tools | `cli`, `server`, `sim`, `xtask` | JSON Schema 2020-12 validation for `./dev check SPEC` (W03.3): `api/openapi.yaml` against the vendored OpenAPI 3.1 schema, and fixture instances against the spec's schemas, with custom keywords for the spec's `x-maxUtf8Bytes` and `x-maxPageUtf8Bytes` byte limits. Default features off: no HTTP or file retrieval and no TLS, so validation is offline; and, as a dev-dependency of `cli`, its tests' validation of `keyval doctor --json` against `schemas/cli/doctor.v1.schema.json`; and, as a dev-dependency of `sim`, SIM-RUN's validation of the `keyval-sim` report against `schemas/dev/sim-run.v1.schema.json` (W17.2). | MIT | crates.io | n/a: tools, and dev-dependencies only |
| `protobuf` | 2.28.0 | deterministic | `raft_adapter` | The runtime of the vendored library's generated `eraftpb` code, pinned to exactly its generator's version (VENDOR.toml `[codegen]`). `raft_adapter` encodes outbound and decodes inbound Raft messages with it, so messages cross the Consensus port as bytes (CD-1). Only the `bytes` feature, which `raft-proto` enables (an entry's data is `bytes::Bytes`). | MIT | crates.io | pure: encoding and decoding only, with no clock, entropy, threads or I/O (W02.2 audit). `CachedSize` is a relaxed atomic, deterministic in single-threaded use (T-1). Unknown fields would be kept in a fixed-key SipHash map and re-encoded (H-3), so the adapter refuses any message that carries one. |
| `raft` | 0.7.0 | deterministic | `raft_adapter` | The Raft consensus library (D-05, W02 GO): `RawNode` behind the Consensus port, in `raft_adapter` only (§7.3, DEP-RULE). Default features off; only `protobuf-codec`. | Apache-2.0 | vendored: `vendor/raft/raft` (VENDOR.toml, checks RAFT-VENDOR and RAFT-AUDIT) | audited and patched (W02.2, W02.3; `spike/raft-audit.md`): election timeouts come only from the caller's `raft::Entropy`, which is the core's entropy port; FxHash maps are fixed per build (H-1); no threads, clock or I/O on keyval's paths, and `default-logger` is off. W02.4's fresh-process trace identity is simulation evidence of this. |
| `saphyr` | 0.1.0 | tools | `xtask` | YAML 1.2 parsing of `api/openapi.yaml` for `./dev check SPEC` (W03.3). Pure Rust, unlike the deprecated `serde_yaml` and its libyaml port. | MIT OR Apache-2.0 | crates.io | n/a: tools only |
| `serde` | 1.0.229 | adapter | `cli`, `model`, `server`, `xtask` | Derived serialisation for the versioned `--json` robot output of every `./dev` command (O-6); and the `keyval` CLI's `--json` reports; and the reference model server's closed request bodies (W30.1); and the production `server`'s HTTP request and response bodies (a composition root, so the scope is the widest: it links into the production binary). | MIT OR Apache-2.0 | crates.io | n/a: no deterministic crate uses it; the `server` composition root, the tools and the test-only model server (an oracle) only |
| `serde_json` | 1.0.151 | adapter | `checker`, `cli`, `model`, `server`, `sim`, `xtask` | JSON encoding of robot output, and decoding of other tools' `--json` output (`br`, `cargo metadata`, clippy diagnostics; §11.5 rule 14); and the `keyval` CLI's `--json` reports; and the reference model server's strict request and response JSON (W30.1); and the `keyval-sim` binary's run report, which `./dev sim run` relays (W17.2); and the `checker` oracle's reading of `keyval.observation/v1` documents (W38.1); and the production `server`'s HTTP JSON bodies (a composition root, so the scope is the widest: it links into the production binary). | MIT OR Apache-2.0 | crates.io | n/a: no deterministic crate uses it; the `server` composition root, the tools, the test-only model server (an oracle) and the `checker` oracle; in `sim`, a pure encoder of the run report, outside the canonical trace |
| `sha2` | 0.11.0 | tools | `xtask` | SHA-256 for gate receipts: the build ID (§10.2 run manifest) and policy-file hashes (§10.10). Default features off. Tools only: the deterministic core implements SHA-256 in-crate (§7.5, D-75). | MIT OR Apache-2.0 | crates.io | n/a: tools only |
| `slog` | 2.8.2 | deterministic | `raft_adapter` | The library's mandatory explicit logger: `RawNode::new` takes a `slog::Logger` (§7.3 "explicit logger"). Default features (`std`, `nested-values`). | MPL-2.0 OR MIT OR Apache-2.0 | crates.io | pure: core slog reads no clock, environment or entropy, and formats a record only when a drain consumes it (W02.2 audit L-1). The adapter passes `slog::Discard`, and no drain crate is a dependency. |
| `toml` | 1.1.6+spec-1.1.0 | tools | `xtask` | Parsing TOML: the toolchain pin in `rust-toolchain.toml` (`./dev doctor`), and later the `registry/*.toml` files (W05, W06). Default features off; only `std`, `serde`, `parse` (no writer). | MIT OR Apache-2.0 | crates.io | n/a: tools only |

*Scope* is one of: `deterministic` (may be used by deterministic crates; needs a purity
argument), `adapter` (adapters and composition roots), `oracle` (model, checker,
conformance; never a production crate), `tools` (`xtask`, `cli`), `dev` (test-only,
via `[dev-dependencies]`).
