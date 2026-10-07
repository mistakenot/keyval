Milestone: M1
Auditor: fresh-context subagent (task af02a9187b835165b, "M1 drift audit (fresh auditor)"), launched by LavenderRabbit under br-w20-m1-drift-audit-q5i; read-only, no cargo builds, git writes or br writes
Independent: yes. The auditor was given only the repository, the authoritative rules and the exact committed tree. It received no interpretation or conclusions from the W19 author (LavenderRabbit), who did not audit.
Tree: b133ff95f89e7af59545c33dc90982c13d402ad4
Commit: df75a44805d26704c5d497592822ab8fac25f394

# M1 drift audit (W20)

The findings below are the auditor's, transcribed without change of substance. Line numbers marked "~" are approximate and need re-confirming before anyone edits. No P0 or P1 findings. Claim class: static reading and receipts (audit evidence, not proof); the auditor ran no tests.

## F-01 W19's acceptance never runs the demo end to end
Bead: br-fhk3
- **Severity:** P2. **Kind:** evidence / demo-honesty.
- **Where (code):** `xtask/src/demo.rs` module doc (~l.21) and `Options` doc (~l.255) say "check DEMO-M1 runs it so [--assert]"; its tests module has 11 unit-level tests under `// check: DEMO-M1`, none calls `run()`; `xtask/src/check.rs` (~l.2010) DEMO-M1 summary says "the end-to-end run is test evidence".
- **Where (plan):** `registry/workgraph.toml` W19 `accept = [DEMO-M1]`; PLAN §11.6 requires one real node with PUT/GET, kill and restart, then two seeded sim runs.
- **What is wrong:** the accept receipt (`accept-br-w19-m1-owner-demo-ddm`, tree 3ca468bc, DEMO-M1 11 cases, ok) proves only unit-level pieces. The only end-to-end evidence is a manual run, `.local/demo/m1/20261006T170206Z-2804100` (all steps passed; an earlier 17:01:20 run failed at init and was later fixed).
- **Why it matters:** a regression in `run()` would turn no gate red.
- **Decision needed:** ordinary work.
- **Fix:** add a gated check that runs `./dev demo m1 --assert` in its own target dir, or correct the doc comments, summary and claim to say the end-to-end run is manual. Do not lower `min_cases`.

## F-02 Claim classes in registry/checks.toml overstate example-based evidence
Bead: br-5kl8
- **Severity:** P2. **Kind:** registry.
- **Where:** `registry/checks.toml`: 86 of 98 checks are `class = "invariant"`, 12 `simulation_evidenced`. Examples: SERVER-HTTP, SERVER-RESTART, SERVER-INIT, DEMO-M1; the DEMO-M1 and SERVER-RESTART summaries themselves say "test" or "process-crash evidence". PLAN §10.15 class table: invariant means enforced by an assertion or checker.
- **What is wrong:** example tests and process-kill tests are classed as invariants; §10.15 has no class for example-test evidence, so the registry cannot be both honest and conforming.
- **Why it matters:** the traceability report shows these requirements as invariant-backed.
- **Decision needed:** owner (plan-change or conflict).
- **Fix:** file a `conflict` bead citing both places. Do not edit rows silently; check rows are forward-only.

## F-03 Demo limitations omit two caveats
Bead: br-qliu
- **Severity:** P3. **Kind:** demo-honesty.
- **Where:** `xtask/src/demo.rs` hand-written `LIMITATIONS`; `crates/sim/src/trace.rs:21` and `:725` (the trace hash is 64-bit FNV-1a, "a fast mismatch test, never proof of equality"); D-46, §10.17; §11.6 asks for "identical trace hashes".
- **What is wrong:** (a) SIGKILL durability: the demo does not say this is process-crash evidence only (the page cache survives a SIGKILL). (b) "Identical": the demo does not say this is a hash comparison only; both runs write the same trace path, so bytes are never compared (SIM-RUN does compare bytes).
- **Decision needed:** ordinary work.
- **Fix:** add two lines to `LIMITATIONS` and a test that asserts they are present.

## F-04 Stale or imprecise demo and registry text
Bead: br-nwcm
- **Severity:** P3. **Kind:** docs / demo-honesty.
- **Where:** `xtask/src/demo.rs` `LIMITATIONS`: "a real multi-node cluster needs peer transport (W21)" is partly stale (`crates/server/src/peer.rs`, `PATH = "/raft"`, `MAX_PEER_CONNECTIONS`; commit fcf9fb0 landed W21.1). `item_status` reads the working-tree `.beads/issues.jsonl` and treats "implemented" as "bead closed", while the docs say "committed tracker export". The SERVER-INIT summary (`xtask/src/check.rs` ~l.1668, mirrored in the registry) says `run` "does not serve yet (W18.2)", but `crates/server/tests/binary.rs` (`run_serves_an_initialised_directory_over_http_until_killed`) shows it serves.
- **Decision needed:** ordinary work.
- **Fix:** narrow the W21 limitation to what is still missing (the multi-node cluster); read the tracker export from the judged tree or reword the doc; correct the SERVER-INIT summary in the catalogue and the registry together.

## F-05 PLAN §7.12 describes an async runtime and a controller task; the server uses std threads
Bead: br-dqmj
- **Severity:** P3. **Kind:** plan-vs-code.
- **Where (plan):** PLAN §7.12 (~l.776), "an async runtime confined to this crate" and the "serial controller task". **Where (code):** `crates/server/src/{serve,http,peer,runtime}.rs` use std threads and std HTTP, with no async runtime.
- **What is wrong:** the code diverges from the plan. It is consistent with the AGENTS.md ban on async in deterministic crates, and no decision-log entry for the choice was found.
- **Decision needed:** owner (`plan-change` or a documented decision), not a silent resolution.
- **Fix:** record the decision, or amend §7.12.

## F-06 DEPENDENCIES.md misstates the scope of serde and serde_json
Bead: br-fodo
- **Severity:** P3. **Kind:** rule-violation (CD-12 accuracy).
- **Where:** `DEPENDENCIES.md` lists serde and serde_json with scope "tools" and purpose text about `./dev` and the CLI; they are also used by the production `server` crate and by `sim` (serde_json).
- **What is wrong:** the scope and "Used by" cells are inaccurate. DEP-LEDGER passes, so it does not check scope.
- **Decision needed:** ordinary work.
- **Fix:** set the scope to `adapter` (or split the rows) and correct the purpose text.

## F-07 A dead assertion in a DEMO-M1 test
Bead: br-8dze
- **Severity:** P3. **Kind:** evidence.
- **Where:** `xtask/src/demo.rs`, test `the_human_report_says_what_is_pending_and_what_is_not_proved`: "M2: 0 of 0" is in `wanted` and then skipped by `continue`.
- **What is wrong:** the line is never checked, so the test can pass whatever the report prints for M2.
- **Decision needed:** ordinary work.
- **Fix:** assert the line, or remove it from `wanted` with a comment saying why.

## F-08 W18 closure provenance is irregular and could not be verified
Bead: br-nsms
- **Severity:** P3. **Kind:** evidence.
- **Where:** accept receipt `accept-br-w18-server-single-node-x8b` is on tree 53071f1e, dated 2026-10-04; closing commit f34c6bf is dated 2026-10-06 and its message format differs from a `./dev commit --close` commit. A gate receipt exists: `gate-fast-20261006T163027Z-4513c9a17822-2645220.json`.
- **What is wrong:** the auditor could not confirm that tree 53071f1e carries a `Bead: br-w18-server-single-node-x8b` line, which AGENTS.md says `--close` requires.
- **Decision needed:** ordinary work (a check by someone with git access).
- **Fix:** run `git log --format=%H%n%B` for the commit with that tree. If the line is missing, an owner decision is required (AGENTS.md: "closes only by an owner decision").

## Areas examined with no findings

- **Deterministic crates (types, statemachine, node, journal, raft_adapter):** `lib.rs` carries the forbid block; each crate's `clippy.toml` is identical to `xtask/lint/deterministic.clippy.toml`; a grep for HashMap, Instant, SystemTime, thread, fs, env, async and println! found only `std::net::SocketAddr` in `types/config`, a pure type.
- **Lint allows:** each carries a bead ID (`crates/server/tests/peer.rs` 3 dead_code allows citing br-w21-1-http-peer-transport-g97; `crates/types/src/sha256.rs:183` and `crates/types/src/config/error.rs:318` in `reason=`).
- **TODO, `todo!`, `unimplemented!`:** none in `crates/` or `xtask/src`.
- **Ready loop:** the `crates/raft_adapter/src/ready_loop.rs` header matches PLAN §7.4 steps 1-7.
- **Server requirements:** O-1 `/v1/cluster` fields checked; O-3 log file defaults to `data_dir/node.log`; O-9 `exposure_warnings` implemented.
- **Bounds (CD-9):** `serve.rs` derives `max_connections` from `Limit::PendingAttempts`; `peer.rs` has `MAX_PEER_CONNECTIONS = 16`; the entropy source reads `/dev/urandom` in an adapter, which is acceptable.
- **Sim slice:** `crates/sim/src/slice.rs` carries a LABEL saying it is a simulated slice (P6); `./dev sim run` is used with the `slice` profile only.
- **Demo process safety:** kills by PID receipt only, respecting rule 18.
- **Receipts for W17 and W18:** selector lists match `registry/workgraph.toml`. W17: SIM-CLUSTER 14, SIM-RUN 10, DEV-SIM 9, all ok, tree ffd6a4cc. W18: SERVER-INIT 15, SERVER-RUNTIME 22, SERVER-HTTP 18, SERVER-RESTART 4, all ok, tree 53071f1e.
- **Schemas:** `schemas/dev/README` lists `demo.v1`.

## Provenance

- **Read:** AGENTS.md; PLAN.md §0.4, §6.6-6.8, §7.4, §7.10-7.12, §7.14, §10.13, §10.15, §11.6, §11.9, §12.2 W17-W19 and Appendix A; `registry/workgraph.toml`, `registry/checks.toml` (98 rows), `DEPENDENCIES.md`; `xtask/src/demo.rs` and the DEMO-M1, SERVER-INIT and related entries in `xtask/src/check.rs`; `crates/server/src/{api,main,serve,logs,peer,http,runtime,entropy}.rs` and `crates/server/tests/binary.rs`; `crates/sim/src/{trace,slice}.rs`; the lint blocks and `clippy.toml` copies in the deterministic crates; receipts in `artifacts/receipts/` and demo run directories under `.local/demo/m1/` (read-only); commit messages of W17, W18 and W19.
- **Commands:** `git archive` of df75a44 into a scratch directory, then grep, sed and read-only inspection there.
- **Not examined:** no cargo build, gate or tests were run, so `min_cases` and test counts come from receipts and static reading only; the demo was not run, so its behaviour is judged from code and the existing manual run directory; git history was not examined for the `Bead:` lines (F-08); the registry's covers lists and `min_cases` were sampled against `#[test]` counts, not checked for all 98 rows; working-copy edits (`crates/server/tests/cluster.rs`, `xtask/src/audit_reports.rs` and others) were not audited, because the audit covers only the committed tree.
