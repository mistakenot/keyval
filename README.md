# keyval

keyval is a strongly consistent key-value store written in Rust. One Raft group of five
voters replicates every write, clients speak HTTP/JSON, and correctness is judged mainly by a
deterministic simulator that runs the real node logic, the real Raft library and the real
journal code against a simulated network, disk and clock. It is a hobby-scale project and
also a worked example of building software with a swarm of coding agents under mechanical
quality gates.

The authoritative description is [`PLAN.md`](PLAN.md). Where this file and the plan differ,
the plan wins.

## What it does

- **Operations:** `PUT`, `GET`, `DELETE`, `SCAN`, `COMPARE_SWAP` and a read-only request
  status lookup, specified in [`api/openapi.yaml`](api/openapi.yaml).
- **Writes** are linearizable: they pass through Raft and are made durable in an
  append-only journal before the node acts on them.
- **Reads** are served locally by any ready node. They never show uncommitted data and may
  be stale.
- **Retries are safe forever.** Every write carries a client-generated request ID, and its
  first execution and result stay in replicated state, so a late retry cannot apply twice.
- **Out of scope for v1:** snapshots and log compaction, TLS and authentication, dynamic
  membership, sharding and performance tuning (`PLAN.md` §2.2).

## Status

What is built and what is not, so nothing here is read as more than it is:

- **A real single node serves `PUT` and `GET`** over HTTP, plus `/v1/health/live`,
  `/v1/health/ready` and `/v1/cluster`, through the `keyval-server` binary. It recovers
  from a SIGKILL with its data intact (process-crash evidence only; power-loss durability
  rests on stated assumptions, `PLAN.md` §10.15, §10.17).
- **`DELETE`, `COMPARE_SWAP`, `SCAN` and request status** are implemented in the state
  machine and the reference model. The HTTP server does not serve them yet.
- **The five-node cluster is simulated.** `keyval-sim` runs five production node cores in
  one process. A real five-process cluster is partly built: the peer transport, write
  routing and the `./dev cluster` launcher exist, and the work to finish and prove the real
  cluster is open. `config/local.toml` describes the local five-voter layout.
- **The `keyval` CLI** has one command, `keyval doctor <data-dir>`, which verifies a data
  directory offline. The client commands are not built.
- **Independent conformance suite:** the harness exists in `conformance/`. A single real node is
  exercised for PUT and GET only; a real cluster for nothing yet.
- The full evidence suite, the release campaign and the final audit are open work. The
  work graph is `registry/workgraph.toml`; `./dev demo m1` prints, from the tracker, which
  items are implemented and which are pending.

## Quickstart

Everything runs through the `./dev` launcher, which pins the toolchain (stable 1.99.0,
edition 2024) and builds with Cargo.

```bash
./dev doctor                       # toolchain and every required tool, with real probes
./dev sim run --seed 1             # one simulated five-node run; prints the verdict and trace hash
./dev check SPEC                   # one registered check, here the OpenAPI spec check
./dev demo m1 --assert             # real node: PUT, GET, SIGKILL, restart, GET; sim run twice
```

`./dev sim run` labels its output a *simulated slice*: it is not a server. The M1 demo
starts a real one-voter `keyval-server`, writes a key, kills the process, restarts it from
the same data directory and reads the value back. It leaves its run directory under
`.local/demo/m1/` and prints the exact `keyval-server init`, `keyval-server run` and `curl`
commands to repeat by hand.

Other `./dev` commands (all take `--json`): `check`, `commit`, `accept`, `gate`, `report`,
`prune`, `beads`, `demo`, `cluster` (`init`, `up`, `status`, `down`, `kill`, `restart`) and `sim`.
Run `./dev --help` for the list.

## Architecture

The core is hexagonal: domain and engine logic live in deterministic crates with no clock,
randomness, threads or I/O of their own. Adapters translate, and two composition roots wire
the same core to real or simulated environments.

| Crate | Role |
|---|---|
| `crates/types` | Domain types, newtypes, errors, ports, configuration and the limits registry |
| `crates/statemachine` | The replicated state machine: versions, request history, quotas |
| `crates/journal` | Journal engine: frames, genesis binding, truncation, recovery |
| `crates/raft_adapter` | The only user of the vendored Raft library, and the Ready loop |
| `crates/node` | Node controller: request lifecycle, write and read paths |
| `crates/fs_storage` | Real-file byte-storage adapter |
| `crates/server` | Production composition root: the `keyval-server` binary, HTTP, peer transport |
| `crates/sim` | Simulation composition root: the `keyval-sim` binary, simulated network, disk and clock |
| `crates/model` | Reference model of the API semantics, for testing only |
| `crates/checker` | History checkers that judge a run's observations |
| `crates/cli` | The `keyval` command-line tool |
| `conformance/` | HTTP-only conformance suite, separate from the workspace's internals |
| `xtask/` | The `./dev` tool: checks, gates, commits, reports |
| `vendor/` | The Raft library (raft-rs 0.7.0), pinned, with a determinism patch |

Dependencies point inward only, and a check enforces it. The model and the checkers depend
on no production crate, so they cannot inherit its mistakes.

## How correctness is judged

- **Deterministic simulation** is the primary instrument. A run is a function of one seed
  and one build, and the same seed gives the same trace. A seed replays with
  `./dev sim run`, and a fixed failure is meant to leave its case in `sim/corpus/`. Crash
  packs and the corpus runner are open work.
- **A reference model** written from the specification, not from the production code,
  decides what each operation should return. Differential tests compare it with the state
  machine.
- **Checkers** read recorded observations and judge them independently of the node.
- **A verification registry** (`registry/`) ties each requirement in `PLAN.md` to the
  checks that cover it. A requirement counts as active only when its work items are
  closed, and a check that runs zero cases never passes.
- **Claims carry their class** (`PLAN.md` §10.15): invariant, simulation-evidenced,
  measurement, assumption or proposal. "N seeds passed" is simulation evidence, not proof,
  and is always quoted with its fault model and build.
- **Commits are gated.** `./dev commit` runs the fast gate on the exact tree it will
  commit, and a bead closes only with an acceptance receipt for that tree.

## Where to go next

- [`PLAN.md`](PLAN.md): goals, requirements, architecture, verification and the work graph.
- [`AGENTS.md`](AGENTS.md): the rules for agents working in this repository.
- [`api/openapi.yaml`](api/openapi.yaml): the HTTP API contract.
- [`DEPENDENCIES.md`](DEPENDENCIES.md): every direct dependency, with purpose and licence.
- [`spike/go-no-go.md`](spike/go-no-go.md): the Raft determinism spike report.
