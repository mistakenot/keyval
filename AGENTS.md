# AGENTS.md — keyval

> Rules for AI coding agents working on **keyval**: a strongly consistent key-value store in
> Rust — one five-voter Raft group, HTTP/JSON API, deterministic simulation testing.
> Read `PLAN.md` before doing anything else.

---

## RULE 0 — PLAN.md IS THE SOURCE OF TRUTH

`PLAN.md` is owner-owned and authoritative. This file restates the plan's agent rules
(§11.5) plus the Rust baseline that enforces them; where the two disagree, **`PLAN.md`
wins** and the disagreement is a defect to report, not to resolve silently.

Precedence when any two sources disagree (PLAN.md §0.4), higher wins:

1. Explicit owner instruction (chat, or a bead comment written by the owner)
2. `PLAN.md` (and `registry/workgraph.toml`, which mirrors §12.2 exactly)
3. `api/openapi.yaml`
4. Checkers, invariants, the reference model and the verification registry
5. Tests
6. Beads (task descriptions)
7. Code

File a bead labelled `conflict` citing both locations; never quietly pick the
convenient side.

This repo inherits the suite-wide ACFS rules in `/data/projects/AGENTS.md` (no file
deletion without permission, no destructive git, `ubs` before commit) with one explicit
replacement, stated next.

---

## Git Discipline — REPLACES the ACFS "Landing the Plane" checklist

**This section overrides any tool-generated git instructions elsewhere in this file**
(e.g. raw `git add`/`git commit`/`git push` in the appended tool blurbs below).

The ACFS landing-the-plane git workflow does **not** apply in this repo:

- Commits happen **only** through `./dev commit` (the exact-candidate wrapper, §11.10).
- Never raw `git add` / `git commit`; never `git add -A` in the shared checkout.
- Never `git pull --rebase` in the shared checkout — it can disturb other agents'
  uncommitted work.
- Never create branches or worktrees (including `ntm --worktrees`).
- Any push is performed by the wrapper under the commit lock once the owner has created
  a remote. That is an explicit owner action (§17 step 2). The owner created it on
  2026-10-03 (`origin` = github.com/mistakenot/keyval, private; bead
  `br-owner-create-remote-push-trjy`). Pushes go to `origin main` and are mirrored to
  `origin main:master` (suite convention).
- **A single coordinator agent commits for everyone until the owner launches the
  self-serve swarm** (§11.2 step 2; §17 step 6: after W04, W06 and W07 land).
  Coordinator since 2026-10-03: GoldenHorizon.
  - Everyone else does not touch git. Keep your files unstaged, and ask in your bead's
    Agent Mail thread, giving the exact paths, the parent and the expected tree ID.
  - The coordinator re-gates exactly that tree on a frozen, git-isolated candidate
    and commits it. The tracker export (`.beads/`) goes in separate beads-only
    commits.
- **W07 has landed the wrapper:**
  - `./dev commit --bead <id> <path>…` (§11.10) takes the commit lock and checks:
    - HEAD is on `main`;
    - the bead is `in_progress` (full ID only);
    - every path is inside your live Agent Mail reservation and outside any other
      agent's exclusive one.

    It refuses foreign staging. It stages only your paths and refreshes the fixed
    `.local/candidate/` from the staged tree, verified against its manifest. It runs
    the candidate's own `./dev gate fast --tree` there, git-isolated, so the judge is
    built from the tree under test.
  - If HEAD moved during the gate, it rebuilds the candidate as the new HEAD plus
    exactly your staged changes (never files edited meanwhile) and re-runs. It refuses
    instead if that concurrent commit changed one of your files, or if anyone staged
    meanwhile.
  - It commits exactly the gated tree, compare-and-swapping `main` from its parent
    (trailer `Bead: <id>`), pushes, and writes a receipt.
  - When self-serve starts, set `AGENT_NAME` (or pass `--agent`) to your Agent Mail
    name.

---

## Agent Rules (PLAN.md §11.5 — normative; W01's gate checks this list is present)

Kept **verbatim** with PLAN.md §11.5: the AGENTS-RULES check (`./dev check AGENTS-RULES`,
run in every gate's xtask tests) fails on any difference in wording, numbering or order.
Change the plan first, then copy it here.

1. **Owner override.** Explicit owner instructions win (§0.4).
2. **Precedence.** Follow §0.4. Report conflicts as `conflict` beads; never silently pick a side.
3. **Oracles are sacred (P9).**
   - Never weaken, skip, delete or loosen a checker, invariant, test, fixture, bug-injection variant or corpus seed to make something pass.
   - A genuine checker or model bug is fixed in a dedicated bead labelled `conflict` **and** `needs-owner` (one bead, both labels — §0.4 files it as a conflict, and §13 item 10 counts `conflict` beads), with an independent counterexample, and the bug-injection self-test MUST still pass.
4. **Determinism (P4).**
   - In deterministic crates: no wall clock, no unseeded or thread-local RNG, no threads/async, no randomly hashed maps or behaviour-affecting unordered iteration, no direct I/O.
   - Do not silence the determinism lint.
5. **Hexagonal core (P3, CD-1).** Domain and engine logic live only in deterministic crates. Adapters only translate. Deterministic crates never import adapter crates, HTTP, filesystem or async-runtime types, and no Raft-library type crosses the Consensus port. No logic that runs only in simulation, or only in production, inside the core.
6. **Real slices only (P6).** No placeholder crates, no fake implementations presented as real, no `todo!()` on a path a test exercises, no simulated slice presented as a real server.
7. **Plan is owner-owned.** Do not change the meaning of normative sections of `PLAN.md`; propose via `plan-change`. `api/openapi.yaml` may be edited only to match `PLAN.md` §8.
8. **Stay in your area.** Edit only files within your bead's reserved area. Shared surfaces (§11.7) need a short explicit reservation. If you need more, reserve it, or file a dependency bead.
9. **Seeds.** Every failing case you fix goes into `sim/corpus/` in the same commit (shrunk scenario if available).
10. **Robot mode.** Use `bv --robot-*` and `br … --json`. Never launch interactive TUIs.
11. **Code design (§5.2).** Follow CD-2 to CD-12. In particular:
    - model state so invalid combinations cannot be built (CD-2);
    - parse input into domain types at the boundary (CD-3);
    - use the dedicated ID/index types, never bare integers (CD-4);
    - no wildcard match arms on domain enums (CD-5);
    - assert invariants where state changes (CD-7);
    - give every new queue, map or cache a registered bound (CD-9);
    - add every new dependency to `DEPENDENCIES.md` with its purpose, version and licence (CD-12).
    Do not disable or `allow` any lint that enforces a CD principle.
12. **Registry (§10.13).** Register every new check in `registry/checks.toml` with its code marker, executable selector, minimum case count, covered requirements and claim class. **Never edit a requirement's status by hand:** activation is derived by `./dev commit --close` when the last listing bead closes (§10.13), and the registry gate rejects hand-edited statuses. Never delete registry rows, golden files, corpus entries or bug variants to make a gate pass.
13. **Claims (§10.15).** In bead closures, commit messages and reports, state claims in their class. "N seeds passed" is simulation evidence, never proof.
14. **Robot output (O-6).** Consume tool output via `--json`, never by scraping text. If you change a JSON output, update its schema and version.
15. **Commits (§11.10).** Commit only through `./dev commit`. Never `git add -A` in the shared checkout. Never create branches or worktrees (including `ntm --worktrees`).
16. **Evidence.** Close a bead only with a passing acceptance receipt for the current tree. Never mark a check `passed` by hand; statuses come from receipts.
17. **Test-only code.** Failpoints and bug variants live behind test-only builds and are never enabled in a production build.
18. **Node data.** Manage nodes only through `./dev cluster …`. Never kill processes by port or with broad patterns, and never delete or rewrite a node's data directory.

---

## Toolchain: Rust & Cargo

Cargo only — never any other build or package tool.

- **Toolchain:** pinned **stable 1.99.0** via `rust-toolchain.toml` (with clippy and
  rustfmt). Bumping it is a deliberate change: a bump can strengthen lint groups
  underneath us, so re-run the full lint gate and the lint probes, and record the bump
  in its own bead. `./dev doctor` fails if the active toolchain is not the pin.
- **Edition:** 2024; resolver 3.
- **Workspace:** virtual manifest at the root; crates live under `crates/` per
  PLAN.md Appendix A. Crates are created by the first bead that needs them (P6:
  no empty crates).
- **Builds:** run cargo as `rch exec -- cargo …` so builds offload to remote workers
  once any exist (with none, rch builds locally). `./dev` does this itself; set
  `KEYVAL_NO_RCH=1` to force plain cargo.
- **Determinism note (T-3):** "same build" covers toolchain, target, lockfile,
  vendored-library patch, configuration, scenario version and PRNG implementation.
  `Cargo.lock` is committed; toolchain and lockfile changes invalidate trace
  comparisons. Gate receipts record a build ID hashing these inputs.

### Crate tiers (Appendix A — the tier decides which rules bind)

A crate's tier follows from its **directory**, which Appendix A fixes. DEP-RULE
classifies every workspace member this way, and a crate in an unlisted directory fails.

| Tier | Crates | Extra rules |
|---|---|---|
| Deterministic — domain (pure) | `crates/types`, `crates/statemachine` | Rules 4 & 5; std + approved pure libs only; deterministic lint block + clippy config (below) |
| Deterministic — engine (no ambient I/O) | `crates/node`, `crates/journal`, `crates/raft_adapter` | Same as domain; may also use the vendored Raft library |
| Adapter | `crates/fs_storage` | Translate only; no domain logic (CD-1) |
| Composition roots | `crates/server` (production), `crates/sim` (simulation) | Translate only; the server never depends on `sim` or oracles |
| Oracles (no production deps) | `crates/model`, `crates/checker` | Depend on **no** production crate (T-12) |
| Tools | `crates/cli`, `xtask` (`./dev`), `fuzz/`, `conformance/` (separate package, HTTP only) | — |
| Lint probes (fixtures) | `xtask/probes/*` | Deliberate violations behind `cfg(keyval_probe)`; never real code |

Dependencies point inward only (DEP-RULE): domain → domain; engine → domain, engine,
vendored Raft; adapter and CLI → domain, engine, adapter; server → domain, engine,
adapter; sim → domain, engine, adapter, oracle; oracle → oracle; conformance → nothing
first-party. The rule covers dev-dependencies too.

`crates/types` exists from W01 and holds `types::invariant!` (below); W04 adds the domain
types, newtypes, ports and errors.

One crate (or noted leaf module) = one **area** for file reservations. Shared surfaces
(workspace `Cargo.toml`, `Cargo.lock`, `AGENTS.md`, `DEPENDENCIES.md`,
`api/openapi.yaml`, `registry/*`, `.beads/`, module export facades) are edited only
under a short, explicit reservation (§11.7).

---

## Lint Policy (strict — this is a greenfield project; no legacy allowances)

Lints are configured in W01 (workspace policy, deterministic-crate configuration, probe
tests) and W04 (the determinism lint applied to real crates), pinned to the project
toolchain, and **tested**: every denied lint has a probe file that the gate must reject
(check LINT-PROBES). Documentation of a lint is never assumed to match its behaviour —
the probe is the proof.

Baseline, in `[workspace.lints]` of the root `Cargo.toml` (every crate has
`[lints] workspace = true` and nothing else under `[lints]`; LINT-TIERS checks it):

```toml
[workspace.lints.rust]
unsafe_code = "forbid"          # CD-8 — includes our patch to the vendored Raft library

[workspace.lints.clippy]
all      = { level = "deny", priority = -1 }
pedantic = { level = "deny", priority = -1 }
nursery  = { level = "deny", priority = -1 }
# CD-8: expected failures are values; broken invariants use CD-7 assertions.
unwrap_used   = "deny"
expect_used   = "deny"
panic         = "deny"
todo          = "deny"
unimplemented = "deny"
unreachable   = "deny"          # invariant! is the only sanctioned panic
dbg_macro     = "deny"
```

These are `deny`, not `forbid`, only because clap's derive emits
`#[allow(clippy::restriction)]`; LINT-ALLOWS closes the gap (below).

With, in `clippy.toml` (tests may use these — CD-8):

```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
allow-panic-in-tests  = true
```

Clippy counts as test code only `#[test]` functions and `#[cfg(test)]` items. **Start
every integration-test file (`tests/*.rs`) with `#![cfg(test)]`**, or its helper
functions are linted as production code (a probe proves this).

**Invariants (CD-7/CD-8).** Use `types::invariant!(condition, "INVARIANT-ID", "format",
args…)`. It expands to `assert!` (checked in all builds) and panics with
`invariant <ID> violated: <context>`; the context names the node, index and values
involved. It is the only sanctioned panic in production code, and no `allow` is needed.

**Deterministic crates** additionally carry the canonical configuration in `xtask/lint/`
(LINT-TIERS checks both copies byte for byte):

- `lib.rs` starts with the exact block in `xtask/lint/deterministic-lints.rs`: a
  `#![forbid(...)]` of `unsafe_code`, `missing_docs` (CD-11: every public item, and every
  public domain type documents its invariants), the CD-8 lints,
  `clippy::wildcard_enum_match_arm` (CD-5: catches `_ =>` **and** catch-all bindings
  such as `other =>`), and the determinism lints. `forbid` means no `allow` can override
  any of them.
- The crate's own `clippy.toml` is a copy of `xtask/lint/deterministic.clippy.toml`:
  the test allowances plus `disallowed-methods`, `disallowed-types` and
  `disallowed-macros` covering wall-clock time (`Instant`, `SystemTime`), threads,
  `RandomState`/`HashMap`/`HashSet`, ambient environment (`std::env`), filesystem,
  network and process I/O, stdio, `println!`-style macros and `thread_local!`. Each
  entry's `reason` names the port to use instead (§7.2). These bans cover test code
  too, because the test allowances exempt only `unwrap`/`expect`/`panic!`. Tests in a
  deterministic crate use the same ports with seeded fakes. A test that needs the real
  environment, clock or filesystem belongs in an adapter or harness crate.

Escape-hatch policy:

- **Never** `allow` (or `expect`) a lint that enforces a CD principle, or a group that
  contains one (`warnings`, `clippy::all`, `clippy::style`, `clippy::restriction`), and
  never silence the determinism lint (rules 4 and 11). No exceptions. LINT-ALLOWS
  rejects them anywhere in first-party code, including inside `cfg_attr`.
- Any other `allow` is narrow (one site, or one documented group), and carries a comment
  stating what fired, why it is acceptable here, and the bead ID that decided it. The
  comment must sit within the 3 lines above the attribute, or the bead ID can be in the
  attribute's `reason = "…"`. The lint configuration doubles as a decision log.
  Undocumented allows are defects, and LINT-ALLOWS rejects them.
- `#[allow]` never appears in a commit just to get a gate green. Fix the code or file
  the bead.

Invariant style (CD-6/CD-7): expected failures are typed results (error enums, no
string errors). Broken invariants use `invariant!` — never `debug_assert!` — because
invariants are checked **in all builds** (performance is not a v1 concern). Assertion
messages carry node, index and invariant ID.

---

## Dependencies (CD-12)

- Deterministic crates start from **the standard library plus the vendored Raft
  library** (`vendor/raft/`, pinned 0.7.0 with recorded checksums; engine crates only).
  Anything else in a deterministic crate needs a `DEPENDENCIES.md` row with scope
  `deterministic`, whose Determinism cell shows it is pure (no ambient time, entropy,
  threads or I/O). DEP-RULE rejects any other third-party crate there, dev-dependencies
  included.
- Every direct dependency anywhere in the workspace (normal, dev or build) is listed in
  `DEPENDENCIES.md` in the same commit that adds it, with columns Crate, Version (the
  **locked** version), Scope (`deterministic` | `adapter` | `oracle` | `tools` | `dev`),
  Used by, Purpose, Licence, Source and Determinism. DEP-LEDGER (in the T0 gate) fails
  on an unlisted dependency, a wrong Used by, a version that differs from `Cargo.lock`,
  or a stale row. Declare the version once under `[workspace.dependencies]`.
- Never add: an async runtime, an HTTP stack, RNG, or clock access to a deterministic
  crate — that is what adapters and ports are for (rules 4–5). A crate that pulls in
  its own RNG or clock transitively threatens determinism (P4): check features and
  transitive deps before proposing it.
- `Cargo.lock` is committed. Version bumps are their own beads, not riders.

---

## Compiler Checks & Gates (CRITICAL)

While iterating:

```bash
rch exec -- cargo check --all-targets
rch exec -- cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all --check
./dev check <ID>...          # one or more registered checks, e.g. LINT-PROBES DEP-RULE
```

- Offload builds through `rch exec --` so one agent does not starve the machine.
- Enumerate whole-tree breakage in one pass (collect all errors, then fix), rather
  than one error at a time.
- `ubs <changed-files>` before every commit (suite rule; the gate runs it for you).
- Tests that run cargo themselves must build into their own target directory
  (`CARGO_TARGET_DIR`): the outer `cargo test` keeps its build directory locked while
  tests run, so a nested build there waits forever.

The `./dev` surface that exists today (PLAN.md §11.13; every command takes `--json`;
exit 0 = ok, 1 = failed, 2 = usage error):

- `./dev doctor` — the pinned toolchain and every required tool (rustc, cargo, rustfmt,
  clippy, git, br, bv, ntm, ubs, ast-grep, dcg, am), with versions and a real
  capability probe each; fails on anything missing or broken.
- `./dev check <ID>... [--case NAME]` — registered checks: XTASK, DEV-DOCTOR, DEV-CHECK,
  DEV-GATE, DEV-STEPLOG, DEV-COMMIT, DEV-ACCEPT, DEV-CLOSE, LINT-PROBES, LINT-ALLOWS,
  LINT-TIERS, DEP-RULE, DEP-LEDGER, AGENTS-RULES, INVARIANT. Unknown IDs, zero-case selections, `#[ignore]`d cases and runs below the
  check's minimum case count fail (P10). A failure prints its exact re-run command.
  Until W05's `registry/checks.toml`, the catalogue is built into `xtask/src/check.rs`.
  Every check has exactly one `// check: <ID>` marker in code.
- `./dev commit --bead <id> [--agent NAME] -m MSG <PATH>…` — the exact-candidate commit
  wrapper (see Git Discipline). `./dev commit --close <id> -m MSG` closes a bead (§11.3
  step 7):
  - it needs the newest passing accept receipt to name the tree of the newest `main`
    commit carrying `Bead: <id>`;
  - then, under the commit lock and only once the wrapper's path checks pass, it runs
    `br close` and `br sync --flush-only`, and recomputes requirement activation from
    the registry as committed at HEAD (only a canonical item's §6 requirements, once
    every listing item is closed);
  - it commits `.beads/` plus any `registry/requirements.toml` flips as one closure
    commit (`Closes: <id>`). Re-running it finishes an interrupted closure, and succeeds
    with nothing to commit if HEAD already records the closure.

  `--close <id> --failed --evidence <PATH>` records a failed spike. The bead is
  labelled `failed` + `needs-owner` (once, even on a re-run) and stays open, so its
  dependants stay blocked; the evidence file is committed (`Failed: <id>`).
- `./dev accept <id> --tree <tree-id>` — runs the bead's acceptance selectors on a
  committed tree, materialised in `.local/accept/<tree-id>/`, git-isolated, with its
  own cargo target dir, and writes a `keyval.dev.accept/v1` receipt naming that tree.
  - Selectors: the `accept` list in `registry/workgraph.toml` as committed in that tree,
    for canonical `plan:W` beads; `accept:<CHECK-ID>` labels for subtasks and
    discovered work. An empty selection fails.
  - Each selector runs through the tree's own `./dev`, so the judge is that tree's
    xtask, never the shared checkout's. A check that does not exist in the tree fails.
  - Each new accept tree is a cold build: about 1 minute and about 470 MB of target dir.
- `./dev gate fast` — the T0 gate in its fixed bootstrap form (§10.10): fmt, clippy with
  denied warnings, LINT-ALLOWS, LINT-TIERS, DEP-RULE, DEP-LEDGER, the workspace tests,
  and UBS on the exact changed files (all first-party sources if no source changed).
  It writes a receipt to `artifacts/receipts/` with the tree ID, build ID, policy
  hashes and per-step case counts. **It judges exact trees only (P11):** in a checkout
  it refuses unless the working tree is clean, and judges `HEAD^{tree}`. A commit
  wrapper passes `--tree <id> --changed <path>…` from the materialised candidate, and
  then the gate never calls git. It refuses to run if git can see any repository from
  the candidate. Run it with `GIT_DIR=/nonexistent/keyval-candidate-git` and
  `GIT_CEILING_DIRECTORIES=<candidate>:<candidate's parent>`, which is what
  `candidate::git_isolation` sets. Other tiers report "not implemented" (W46).

Receipts, not your say-so, are the evidence (rule 16): gate receipts for commits, and
accept receipts for closing beads.

---

## Testing

Policy (see PLAN.md §10 for the full verification architecture):

- **Unit tests** live inline (`#[cfg(test)]`) in the crate they test and cover the
  happy path, edge cases (empty, max, boundary — e.g. key length 255/256/257,
  multibyte UTF-8) and error conditions. Tests may use `unwrap`/`expect`/`panic!`.
- **Integration tests** live in the owning crate's `tests/` directory, and each file
  starts with `#![cfg(test)]` (see Lint Policy). Never add tests to a workspace-root
  `tests/` directory: the root manifest is virtual, so Cargo silently never compiles
  them.
- **Property tests** for stated invariants (e.g. version = execution index; replay
  returns the original result; bounded collections at N−1/N/N+1).
- **Compile-fail fixtures** prove CD-2 ("invalid states unrepresentable") claims.
- **Golden vectors** (journal frames, §7.6) and **corrupt-state fixtures** that must
  fire CD-7 assertions.
- **Simulation** is the primary correctness instrument (§10.2–10.9): seeds are
  deterministic, failing seeds become crash packs, and every fixed failure lands its
  seed in `sim/corpus/` in the same commit (rule 9).
- **Conformance** (`conformance/`) talks HTTP only and is checked against
  `api/openapi.yaml` and the reference model — never against our internals.
- Failpoints and bug-injection variants are test-only builds (rule 17).
- Do not hand-run heavy suites on the shared machine when a gate tier covers them:
  T0 on commit, T1 hourly, T2 nightly (§10.10). Cite receipts, not vibes (rule 13).

### Test logging & diagnostics standard (W01.6 — binding on every suite)

A failure must be diagnosable from its log alone, and re-runnable from it.

1. **Assertion context.** Every assertion message carries enough state to diagnose
   without a debugger. The CD-7 `invariant!` style (node, index, invariant ID) extends
   to all test suites: a failing test names its inputs, the expected value, the observed
   value and the rule violated, e.g.
   `assert_eq!(observed, expected, "rule: key length 256 is rejected (DM-1); key={key:?}")`.
2. **Re-runnability.** A failure's output contains the exact command that re-runs it.
   `./dev check` prints `./dev check <ID> --case <test>` for every failed case.
   Property-test failures persist their seed and shrunk case (proptest failure
   persistence, committed alongside the test), and the failure output names the seed,
   mirroring the simulator's seed/replay discipline (T-4). A failing test you cannot
   re-run from its log is a defect.
3. **Step logging for scripted flows.** Every scripted end-to-end flow — demos in assert
   mode, the lifecycle smoke, crash probes, the parity scenario, the clean-state
   rehearsal, the CLI journey, gates, lint probes — logs each step as a structured
   `keyval.dev.step/v1` record: flow, step, action, expected, observed, outcome,
   duration, and a re-run command when it did not pass. Records go to stderr **and** to
   a log file in the run's artifact directory. Use `xtask/src/steplog.rs` (`StepLog`).
   A flow stops at its first failed step and exits non-zero. A gate instead runs every
   step, so one receipt reports every failure, and writes `steps.jsonl` next to its
   receipt and per-step logs.
4. **No silent skips.** There is no "skipped" outcome. A step or test that cannot run is
   logged as failed, and a vacuous selection (zero cases, an `#[ignore]`d required case)
   fails (P10).
5. **Bounded.** Test logs respect CD-9: oversized fields are truncated and records past
   the limit are dropped, and both are counted (receipts report the counts). Generated
   logs live under `artifacts/` or `.local/`, which only `./dev prune` (W46) deletes,
   under the Q9 retention rule.
6. **Helpers stay out of production.** Logging and test helpers are tooling (xtask) or
   test-only modules and dev-dependencies, never compiled into a production build. They
   never log from deterministic-core production paths: the canonical trace and the
   Telemetry port own those.

Practicalities:

- A test that runs cargo itself (fixtures, probes, gates) builds into its own target
  directory via `test_support::nested_cargo`, and writes files under
  `test_support::scratch_dir` (inside the target directory), never into the source
  tree or `/tmp`.
- A test that runs git against a fixture repository clears `GIT_DIR`,
  `GIT_WORK_TREE`, `GIT_INDEX_FILE` and `GIT_CEILING_DIRECTORIES`, so that it still
  works inside a git-isolated candidate (§11.10 step 4).

---

## Multi-Agent Workflow (this repo's conventions)

- Register with Agent Mail using this repo's absolute path as `project_key`; reserve
  your bead's **area** before editing, with the bead ID in `reason`.
- Bead ID = Mail `thread_id`; subjects start `[br-…]`; announce start and completion
  in-thread. Check your inbox at natural pause points; acknowledge what you read.
- Work selection: `bv --robot-triage` → claim in `br` → reserve → work → commit →
  `./dev accept <id> --tree <tree of the work commit>` → `./dev commit --close <id>` →
  release reservations. Until the owner launches self-serve, the coordinator runs the
  git steps for you (Git Discipline, above).
- The tool-managed sections below (Agent Mail, Beads, bv, ee) are appended by those
  tools; edit them only via the owning tool (`am docs`, `br agents`, `bv --agents-*`,
  `ee export agentsmd`).

### Bead conventions (PLAN.md §11.4)

- **Titles:** `W<nn>: <imperative>` for planned work (§12.2); `W<nn>.<k>: <imperative>`
  for its subtasks; `BUG: <symptom>` for defects; `AUDIT: <milestone>` for audits;
  `M<n>: …` for milestone epics; `OWNER: <action>` for owner-only actions. Discovered
  work that is none of these gets an imperative title and the `discovered` label.
- **Description template:** Context (why, 1–3 lines); Requirements (§6 IDs, checker
  IDs, bug-variant IDs); Area (§11.7); Acceptance (executable criteria, e.g. "`./dev
  check K-5 --case committed-prefix` passes; T0 green"); Out of scope.
- **Priority:** P0 correctness bug, red gate or nondeterminism; P1 critical-path work;
  P2 other planned work; P3 nice-to-have.
- **Labels:** `m0`…`m5` (milestone); `plan:W<nn>` on the one bead per §12.2 item;
  `sub:W<nn>` on its subtasks (no `plan:` label); `conflict`, `plan-change`, `hard`,
  `needs-owner`, `failed`, `audit`; plus `milestone` (epics), `process`, `discovered`.
- **Edges:** subtasks chain with `blocks` edges and the parent W bead blocks on all of
  them. `parent-child` is used only for epic membership: mixing both on one pair wedges
  readiness in br. Edges between `plan:W` beads mirror §12.3 exactly; never add or
  remove one without a plan change.
- **Closing** needs a passing acceptance receipt for the current tree (rule 16), quoted
  in a bead comment. A failed spike is never closed: it stays open with `failed` +
  `needs-owner` (§11.4).

---

<!-- am:blurb -->
## MCP Agent Mail: coordination for multi-agent workflows

What it is
- A mail-like layer that lets coding agents coordinate asynchronously via MCP tools and resources.
- Provides identities, inbox/outbox, searchable threads, and advisory file reservations, with human-auditable artifacts in Git.

Why it's useful
- Prevents agents from stepping on each other with explicit file reservations (leases) for files/globs.
- Keeps communication out of your token budget by storing messages in a per-project archive.
- Offers quick reads (`resource://inbox/...`, `resource://thread/...`) and macros that bundle common flows.

How to use effectively
1) Same repository
   - Register an identity: call `ensure_project`, then `register_agent` using this repo's absolute path as `project_key`.
   - Reserve files before you edit: `file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true)` to signal intent and avoid conflict.
   - Communicate with threads: use `send_message(..., thread_id="FEAT-123")`; check inbox with `fetch_inbox` and acknowledge with `acknowledge_message`.
   - Read fast: `resource://inbox/{Agent}?project=<abs-path>&limit=20&agent_token=<registration_token>` or `resource://thread/{id}?project=<abs-path>&agent=<Agent>&agent_token=<registration_token>&include_bodies=true` unless the current MCP session already authenticated as that agent.
   - Tip: set `AGENT_NAME` in your environment so the pre-commit guard can block commits that conflict with others' active exclusive file reservations.

2) Across different repos in one project (e.g., Next.js frontend + FastAPI backend)
   - Option A (single project bus): register both sides under the same `project_key` (shared key/path). Keep reservation patterns specific (e.g., `frontend/**` vs `backend/**`).
   - Option B (separate projects): each repo has its own `project_key`; use `macro_contact_handshake` or `request_contact`/`respond_contact` to link agents, then message directly. Keep a shared `thread_id` (e.g., ticket key) across repos for clean summaries/audits.

Macros vs granular tools
- Prefer macros when you want speed or are on a smaller model: `macro_start_session`, `macro_prepare_thread`, `macro_file_reservation_cycle`, `macro_contact_handshake`.
- Use granular tools when you need control: `register_agent`, `file_reservation_paths`, `send_message`, `fetch_inbox`, `acknowledge_message`.

Common pitfalls
- "from_agent not registered": always `register_agent` in the correct `project_key` first.
- "FILE_RESERVATION_CONFLICT": adjust patterns, wait for expiry, or use a non-exclusive reservation when appropriate.
- Auth errors: if JWT+JWKS is enabled, include a bearer token with a `kid` that matches server JWKS; static bearer is used only when JWT is disabled.

## Integrating with Beads (dependency-aware task planning)

Beads provides a lightweight, dependency-aware issue database and a CLI (`bd`) for selecting "ready work," setting priorities, and tracking status. It complements MCP Agent Mail's messaging, audit trail, and file-reservation signals. Project: [steveyegge/beads](https://github.com/steveyegge/beads)

Recommended conventions
- **Single source of truth**: Use **Beads** for task status/priority/dependencies; use **Agent Mail** for conversation, decisions, and attachments (audit).
- **Shared identifiers**: Use the Beads issue id (e.g., `bd-123`) as the Mail `thread_id` and prefix message subjects with `[bd-123]`.
- **Reservations**: When starting a `bd-###` task, call `file_reservation_paths(...)` for the affected paths; include the issue id in the `reason` and release on completion.

Typical flow (agents)
1) **Pick ready work** (Beads)
   - `bd ready --json` -> choose one item (highest priority, no blockers)
2) **Reserve edit surface** (Mail)
   - `file_reservation_paths(project_key, agent_name, ["src/**"], ttl_seconds=3600, exclusive=true, reason="bd-123")`
3) **Announce start** (Mail)
   - `send_message(..., thread_id="bd-123", subject="[bd-123] Start: <short title>", ack_required=true)`
4) **Work and update**
   - Reply in-thread with progress and attach artifacts/images; keep the discussion in one thread per issue id
5) **Complete and release**
   - `bd close bd-123 --reason "Completed"` (Beads is status authority)
   - `release_file_reservations(project_key, agent_name, paths=["src/**"])`
   - Final Mail reply: `[bd-123] Completed` with summary and links

Mapping cheat-sheet
- **Mail `thread_id`** <-> `bd-###`
- **Mail subject**: `[bd-###] ...`
- **File reservation `reason`**: `bd-###`
- **Commit messages (optional)**: include `bd-###` for traceability

Pitfalls to avoid
- Don't create or manage tasks in Mail; treat Beads as the single task queue.
- Always include `bd-###` in message `thread_id` to avoid ID drift across tools.
<!-- am:blurb:end -->

<!-- ee:agentsmd:begin generation=0 hash=blake3:4e31560a725ff7c0 -->
<!-- generated by `ee export agentsmd`; hand-edit OUTSIDE the ee markers only -->
<!-- ee:agentsmd:end -->

<!-- br-agent-instructions-v1 -->

---

## Beads Workflow Integration

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`/`bd`) for issue tracking. Issues are stored in `.beads/` and tracked in git.

### Essential Commands

```bash
# View ready issues (open, unblocked, not deferred)
br ready              # or: bd ready

# List and search
br list --status=open # All open issues
br show <id>          # Full issue details with dependencies
br search "keyword"   # Full-text search

# Create and update
br create --title="..." --description="..." --type=task --priority=2
br update <id> --status=in_progress
br close <id> --reason="Completed"
br close <id1> <id2>  # Close multiple issues at once

# Sync with git
br sync --flush-only  # Export DB to JSONL
br sync --status      # Check sync status
```

### Workflow Pattern

1. **Start**: Run `br ready` to find actionable work
2. **Claim**: Use `br update <id> --status=in_progress`
3. **Work**: Implement the task
4. **Complete**: Use `br close <id>`
5. **Sync**: Always run `br sync --flush-only` at session end

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready` shows only open, unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers 0-4, not words)
- **Types**: task, bug, feature, epic, chore, docs, question
- **Blocking**: `br dep add <issue> <depends-on>` to add dependencies

### Session Protocol

**Before ending any session, run this checklist:**

```bash
git status              # Check what changed
git add <files>         # Stage code changes
br sync --flush-only    # Export beads changes to JSONL
git commit -m "..."     # Commit everything
git push                # Push to remote
```

### Best Practices

- Check `br ready` at session start to find available work
- Update status as you work (in_progress → closed)
- Create new issues with `br create` when you discover tasks
- Use descriptive titles and set appropriate priority/type
- Always sync before ending session

<!-- end-br-agent-instructions -->

<!-- bv-agent-instructions-v7 -->

---

## Beads Workflow Integration

This project uses a Beads tracker—either the Go `bd` CLI or the Rust `br` CLI—for issue tracking, plus [beads_viewer](https://github.com/Dicklesworthstone/beads_viewer) (`bv`) for graph-aware triage. Issues are stored in `.beads/`. `bv` auto-discovers supported JSONL exports, including `.beads/issues.jsonl` and legacy `.beads/beads.jsonl`.

**Choose the tracker CLI from this repository's instructions and configuration.** Use `bd` commands in a Go Beads workspace and `br` commands in a beads_rust workspace. Do not run both trackers against the same workspace or infer the tracker solely from the JSONL filename.

### Using bv as an AI sidecar

bv is a graph-aware triage engine for Beads projects. Instead of parsing .beads/issues.jsonl / .beads/beads.jsonl directly or hallucinating graph traversal, use robot flags for deterministic, dependency-aware outputs with precomputed metrics (PageRank, betweenness, critical path, cycles, HITS, eigenvector, k-core).

**Scope boundary:** bv handles *what to work on* (triage, priority, planning). The selected tracker CLI (`bd` or `br`) handles creating, claiming, modifying, and closing beads.

**CRITICAL: Use ONLY --robot-* flags. Bare bv launches an interactive TUI that blocks your session.**

#### The Workflow: Start With Triage

**`bv --robot-triage` is your single entry point.** Its `triage` object contains:
- `quick_ref`: at-a-glance counts + top 3 picks
- `recommendations`: ranked actionable items with scores, reasons, unblock info
- `quick_wins`: low-effort high-impact items
- `blockers_to_clear`: items that unblock the most downstream work
- `project_health`: status/type/priority distributions, graph metrics
- `commands`: copy-paste shell commands for next steps

```bash
bv --robot-triage        # THE MEGA-COMMAND: start here
bv --robot-next          # Minimal: just the single top pick + claim command

# TOON output (--format toon): a compact tabular encoding. Measured on this
# repository it is 7% smaller than JSON for --robot-graph but 9-15% LARGER for
# nested payloads (--robot-triage, --robot-plan, --robot-insights,
# --robot-label-health); use --stats to see both sizes before adopting it.
# TOON encoding shells out to the tru binary. With no encoder installed,
# --format toon prints a fallback warning, emits JSON with output_format "json",
# and --stats prints no sizes at all.
bv --robot-graph --format toon
bv --robot-triage --format toon --stats
```

Recommendations can include blocked or assigned work; `triage.quick_ref.top_picks` reflects snapshot readiness. A suggested action records its original local ID, working directory, and tracker route. Use that route rather than a namespaced display ID or an unrelated current directory. Inspect current tracker state before execution: analysis does not reserve work or guarantee that a later claim succeeds.

#### Other bv Commands

| Command | Returns |
|---------|---------|
| `--robot-plan` | Parallel execution tracks with unblocks lists |
| `--robot-priority` | Priority misalignment detection with confidence |
| `--robot-insights` | Full metrics: PageRank, betweenness, HITS, eigenvector, critical path, cycles, k-core |
| `--robot-alerts` | Stale issues, blocking cascades, priority mismatches |
| `--robot-suggest` | Hygiene: duplicates, missing deps, label suggestions, cycle breaks |
| `--robot-diff --diff-since <ref>` | Changes since ref: new/closed/modified issues |
| `--robot-graph [--graph-format=json\|dot\|mermaid]` | Dependency graph export |

Robot analysis commands default to JSON; `--format toon` selects TOON, and `--robot-help` defaults to text. In JSON mode, `--graph-format=dot` or `mermaid` puts diagram text in the `graph` field (`bv --robot-graph --graph-format=dot | jq -r .graph`).

#### Scoping & Filtering

```bash
bv --robot-plan --label backend              # Scope to label's subgraph
bv --robot-insights --as-of HEAD~30          # Historical point-in-time
bv --recipe actionable --robot-plan          # Pre-filter: ready to work (no blockers)
bv --recipe high-impact --robot-triage       # Pre-filter: top PageRank scores
```

### Tracker Commands for Issue Management

Use exactly one command family, matching the tracker configured for the repository.

#### Rust beads_rust (`br`)

Use `br` 0.6.0 or newer when executing saved claim commands. It rechecks
deferred status and future `defer_until` values when the claim runs, so a
recommendation captured before a deferral cannot bypass it. This requirement
applies to executing tracker claims.

```bash
br ready --json                       # Show issues ready to work (no blockers)
br list --status=open --json          # All open issues
br show <id> --json                   # Full issue details with dependencies
br create --title="..." --type=task --priority=2 --json
br update <id> --claim --json         # Claim for the current actor and start work
br close <id> --reason="Completed" --json
br close <id1> <id2> --reason="Completed" --json
br sync --flush-only                  # Export DB to JSONL after Beads mutations
```

#### Go Beads (`bd`)

```bash
bd ready --json                       # Show issues ready to work
bd show <id> --json                   # Full issue details
bd create "..." -t task -p 2 --json
bd update <id> --claim --json         # Atomically claim work
bd close <id> --json
bd dep add <issue> <depends-on>
bd export -o .beads/issues.jsonl        # Refresh the compatibility export read by bv
```

### Workflow Pattern

1. **Triage**: Run `bv --robot-triage` to find the highest-impact actionable work
2. **Verify**: Check the selected tracker's `show`/`ready` output before claiming
3. **Claim**: Use `br update <id> --claim --json` or `bd update <id> --claim --json`
4. **Work**: Implement the task
5. **Complete**: Use the selected tracker's `close` command
6. **Refresh for bv**: Run `br sync --flush-only` or the `bd export` command above so the JSONL export is current

### Key Concepts

- **Dependencies**: Issues can block other issues. `br ready --json` and `bd ready --json` show unblocked work.
- **Priority**: P0=critical, P1=high, P2=medium, P3=low, P4=backlog (use numbers 0-4, not words)
- **Types**: task, bug, feature, epic, chore, docs, question
- **Blocking**: Use `br dep add <issue> <depends-on>` or `bd dep add <issue> <depends-on>` to add dependencies

### Git Policy

Tracker commands do not grant permission to commit or push application code. Follow this repository's own git and tracker instructions before staging, committing, syncing, or pushing. If the repository says "commit only when asked," that rule overrides any generic workflow advice.

<!-- end-bv-agent-instructions -->
