# W02 go/no-go: raft-rs 0.7.0 as keyval's Raft library (D-05)

**Beads:** W02.5 (br-w02-5-go-no-go-8zy), the last subtask of W02
(br-w02-raft-determinism-spike-hta). **Plan:** §7.3, §12.2 W02, §16 D-05, §17 step 5;
RISK-1, D-57. **Author and date:** VioletOwl, 2026-10-03.
**Tree judged:** W02.4's work tree `f74795a3` (commit 1949e47), toolchain 1.99.0,
x86_64-unknown-linux-gnu.

## Outcome: GO (owner decision, 2026-10-03)

Keep `raft-rs` 0.7.0 as the primary library behind the Consensus port. It is vendored,
pinned and entropy-patched. W13 may start once W02 closes, and the OpenRaft 0.9.x fallback
is not needed.

VioletOwl recommended GO, and the owner decided GO on 2026-10-03 (comment on W02.5). §16
records it in D-05; the edit is at the end of this report.

## Evidence against W02's acceptance (§12.2)

| Acceptance item | Evidence | Landed in | Re-checked by | Claim class (§10.15) |
|---|---|---|---|---|
| `raft`/`raft-proto` 0.7.0 vendored with checksums | `vendor/raft/VENDOR.toml`: sha256 per file, upstream commit `10c6e9db` | 07e9ec7 (W02.1) | `./dev check RAFT-VENDOR` (84 cases) | Invariant, per tree |
| Audit lists every entropy, time, thread, hashing and iteration path, plus unbounded fetches on election/commit paths | `spike/raft-audit.md`: one entropy site (E-1); two unbounded fetches (U-1 `hup`, U-2 `maybe_commit_by_vote`), each with a bounded-handling policy and a named owner for its test | bc127d6 (W02.2) | `./dev check RAFT-AUDIT` (32 cases; 9c26a62, br-9dug) keeps the "none on any path" verdicts true in the vendored source | Invariant for the scanned verdicts; the rest are Assumptions (source reading) until each named test passes |
| A minimal patch defines and injects the vendored entropy trait | `third_party/raft.patch`; `vendor/raft/raft/src/entropy.rs`. `Entropy` is mandatory in `Raft::new`/`RawNode::new` and has no ambient default; `rand` is removed. No vote, log-matching, quorum or commit rule is touched (D-57). | ae04391 (W02.3) | RAFT-VENDOR (the patched files are in the manifest) | Invariant, per tree |
| Upstream tests pass with the patch | `spike/raft-patch-tests.md`. Harness: 198 passed, unpatched and patched. Unit tests: 46 unpatched, 48 patched (the 2 new tests). Doc tests: 14 and 14. Failed / ignored: 0 / 0. | ae04391 (W02.3) | Recorded procedure; not in a gate | Test evidence for the stated trees |
| Codegen pinned, recorded in `build_id`, checked by `./dev doctor` | Generated code vendored and `build = false`; `raft_vendor_sha256` is a `build_id` input; doctor has a `raft_codegen` probe | 07e9ec7, 79de0e8 (br-gqxq) | RAFT-VENDOR, DEV-DOCTOR, DEV-GATE | Invariant, per tree |
| A seeded 3-node in-memory loop gives identical spike-local traces for 100 seeds × 3 fresh processes, including crash-restart and re-election | `spike/` and its README result: 100 of 100 seeds byte-identical across 3 fresh processes. Every seed had 2 to 3 leaderships, 1 to 2 re-elections and 2 crash-restarts. 46,908,405 trace bytes, fingerprint `4d4017d4c7b89c5f`. The wall-clock control diverged on 10 of 10 seeds. | 1949e47 (W02.4) | `./dev check SPIKE-DETERMINISM` (301 cases: 300 seed-process runs plus the control); accept receipt on `f74795a3` passed | **Simulation-evidenced** |
| Go/no-go recorded in §16 | This report, and the D-05 row (below) | W02.5 | — | Owner decision |

## What GO claims, and what it does not

- **Determinism** is simulation-evidenced, not proven. No divergence was found in 100 seeds
  × 3 fresh processes on the stated build. It covers the spike's own loop:
  - storage is `MemStorage`;
  - the network is in memory;
  - the Ready order is raft-rs's documented one.

  It does not cover keyval's journal, Ready loop or composition roots. W11 and W13 own
  those, and the simulator's fresh-process replay (T-11) has to re-establish the claim at
  full scale.
- **Upstream test parity** shows that the patch broke no upstream test. It doesn't show
  behavioural equivalence.
- **The audit's "none on any path" verdicts** came from reading the source.
  - RAFT-AUDIT (9c26a62) now keeps them true executably on every tree: no unexpected
    entropy, clock, thread, randomly seeded std hash collection or pointer formatting
    outside test code.
  - The behavioural findings stay Assumptions until the tests owed in
    `spike/raft-audit.md` pass, in W08 and W13.

## Risks carried forward

| Risk | Handling | Owner |
|---|---|---|
| RISK-1: no upstream release since 0.7.0 (March 2023) | We own the vendored copy. RAFT-VENDOR pins it file for file, so any change is a recorded vendor change with its own bead. | W02 (done); future vendor beads |
| U-1, U-2: unbounded entry fetches on the election and commit paths | A large-log election/commit test with a memory bound | W13 (§7.3) |
| H-3: protobuf unknown fields | The peer decoder rejects a `Message` with unknown fields | W13 |
| U-3, U-4: per-Ready and per-message size limits | The produced `raft::Config` sets `max_committed_size_per_ready` and `max_size_per_msg` from §7.11 | W08 |
| The election-timeout range is fixed at `[election_tick, 2×election_tick)` | Config validation encodes it (the patch only replaces the entropy) | W08 |

## What would reopen D-05

- **SPIKE-DETERMINISM fails on a later tree.** It stays in the catalogue, and a failure is
  a P0 (nondeterminism).
- **W13 cannot bound U-1 or U-2** without changing vote, log-matching, quorum or commit
  rules, which D-57 forbids.
- **A fresh-process divergence in the simulator** (T-11) is traced into the vendored
  library.

Any of these reopens the OpenRaft 0.9.x evaluation as a recorded plan revision (§7.3). It
is never a quiet switch.

## The §16 change

- **D-05 row, Status:** `Proposed (pending W02)` → `Decided (W02 GO, 2026-10-03)`.
- **D-05 detail:** a closing paragraph, *W02 outcome (2026-10-03): GO.* It says that
  raft-rs 0.7.0 (vendored, pinned, with the mandatory vendored entropy trait) gave
  byte-identical spike-local traces for 100 seeds × 3 fresh processes, including
  crash-restart and re-election, and that this is simulation-evidenced. It points to this
  report and notes that the OpenRaft fallback stays recorded but unused.
