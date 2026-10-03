# W02 Raft determinism spike

**Beads:** W02 (br-w02-raft-determinism-spike-hta); this harness is W02.4
(br-w02-4-fresh-process-proof-vgl). **Plan:** PLAN.md §7.3, §12.2 W02; P4, T-3, T-11.
**Area:** `spike`. W02 only, kept as evidence (Appendix A). It is its own Cargo workspace,
not a member of the root one.

## What it is

A seeded three-node raft-rs cluster in one process. It is driven tick by tick through the
patched vendored `RawNode` (`vendor/raft`, W02.3's mandatory `Entropy` source), using
`tick`, `step`, `propose`, `ready`, `advance` and `advance_apply`, in raft-rs 0.7's
documented Ready order. Storage and transport are spike-local: `MemStorage` is persisted on
each Ready, and the network is in memory. **This is not keyval's journal or its Ready-loop
discipline**; W11 and W13 own those.

Everything that varies is derived from the run's seed:
- **Election-timeout entropy:** `SplitMix64`, seeded per node and per incarnation.
- **The network:** 1–3 ticks of latency and a 2% drop rate. A message to a crashed node is
  lost. One already sent by a node that has since crashed is still delivered, like a packet
  already on the wire.
- **The schedule:** one crash of the current leader at tick 120–159, restarted 25–44 ticks
  later, which forces a re-election. Then one crash of a seeded live node at tick 250–289,
  restarted 20–39 ticks later.
- **Client writes:** one to the leader every 5 ticks from tick 20.

A restart keeps only the node's `MemStorage` and applied index, and draws a fresh entropy
stream for its new incarnation. Each run lasts 400 ticks.

The trace (`src/trace.rs`) is one ASCII line per event: role changes, hard-state changes,
every sent message as canonical protobuf bytes in hex, drops, losses, every applied entry,
proposals, crashes and restarts. It ends with a statistics line. It holds no host data: no
paths, wall-clock times, PIDs or addresses.

**Non-vacuity (P10).** A run fails unless it has:
- at least two leaderships;
- at least one re-election after a crash;
- two crash-restarts;
- at least three applied client writes.

## Usage

The gate-facing form is **`./dev check SPIKE-DETERMINISM`** (`xtask/src/spike.rs`, claim
class simulation-evidenced). It builds this workspace into its own per-checkout target
directory, then runs:
- the proof for seeds 0..100 in 3 fresh processes;
- the negative control for seeds 0..5 in 2.

It keeps the traces under that target directory's `spike-proof/`.

By hand, use a target directory outside the source tree, since only the root `target/` is
git-ignored:

```sh
cd spike
export CARGO_TARGET_DIR=/path/outside/the/tree
cargo run --locked --quiet -- run   --seed 7 --out "$CARGO_TARGET_DIR/seed7.trace"
cargo run --locked --quiet -- prove --seeds 0..100 --processes 3 --out "$CARGO_TARGET_DIR/proof"
cargo run --locked --quiet -- prove --seeds 0..10 --processes 2 --entropy clock --out "$CARGO_TARGET_DIR/control"
```

`prove` runs every seed in each of `--processes` fresh OS processes. Each process is this
binary's `run`, with an empty environment. It then compares the traces byte for byte. On a
mismatch it prints the first differing event of each trace and keeps both files.

`--entropy clock` is the **negative control**: it seeds each node's entropy from the wall
clock, so `prove` must detect divergence. Exit status: 0 proven, 1 failed, 2 usage.

## Result, 2026-10-03

Run on vendored tree `f67ae903` (W02.3, commit ae04391) with toolchain 1.99.0 on
x86_64-unknown-linux-gnu.

- **Seeded, `prove --seeds 0..100 --processes 3`:** all 100 seeds are **byte-identical across
  3 fresh processes**, in 300 processes and 4.8 s. That is 46,908,405 trace bytes compared;
  the fingerprint is `4d4017d4c7b89c5f`. All 100 traces are distinct from one another.

  | Per seed | Min | Max | Total over 100 seeds |
  |---|---|---|---|
  | leaderships | 2 | 3 | 235 |
  | re-elections after a crash | 1 | 2 | 135 |
  | crash-restarts | 2 | 2 | 200 |
  | applied client writes (summed over nodes) | 181 | 216 | 20,207 |
  | messages sent | 1,610 | 1,876 | 177,712 |
  | messages dropped | 55 | 97 | 7,481 |

- **Negative control, `--entropy clock --seeds 0..10 --processes 2`:** **10 of 10 seeds
  diverged**, each reported at its first differing event. Typically a node's first
  `PreCandidate` comes at a different tick, or a different node goes first.

**Claim class (§10.15): simulation-evidenced.** No divergence was found in 100 seeds × 3
fresh processes on the stated build. That is evidence, not proof, and it covers only this
harness's spike-local loop, not keyval's composition roots.
