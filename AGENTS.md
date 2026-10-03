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
  a remote. That is an explicit owner action; **no remote exists yet** (§17).
- Until W01 lands `./dev`, a single coordinator agent stages, runs the bootstrap gate on
  a frozen candidate copy, and commits (§12.4). Everyone else does not touch git.

---

## Agent Rules (PLAN.md §11.5 — normative; W01's gate checks this list is present)

1. **Owner override.** Explicit owner instructions win (§0.4).
2. **Precedence.** Follow §0.4. Report conflicts as `conflict` beads; never silently
   pick a side.
3. **Oracles are sacred (P9).** Never weaken, skip, delete or loosen a checker,
   invariant, test, fixture, bug-injection variant or corpus seed to make something
   pass. A genuine checker or model bug is fixed in a dedicated bead labelled
   `conflict` **and** `needs-owner` (one bead, both labels — §0.4 files it as a
   conflict, and §13 item 10 counts `conflict` beads), with an independent
   counterexample, and the bug-injection self-test must still pass.
4. **Determinism (P4).** In deterministic crates: no wall clock, no unseeded or
   thread-local RNG, no threads/async, no randomly hashed maps or behaviour-affecting
   unordered iteration, no direct I/O. Do not silence the determinism lint.
5. **Hexagonal core (P3, CD-1).** Domain and engine logic live only in deterministic
   crates. Adapters only translate. Deterministic crates never import adapter crates,
   HTTP, filesystem or async-runtime types, and no Raft-library type crosses the
   Consensus port. No logic that runs only in simulation, or only in production,
   inside the core.
6. **Real slices only (P6).** No placeholder crates, no fake implementations presented
   as real, no `todo!()` on a path a test exercises, no simulated slice presented as a
   real server.
7. **Plan is owner-owned.** Do not change the meaning of normative sections of
   `PLAN.md`; propose via a `plan-change` bead. `api/openapi.yaml` may be edited only
   to match `PLAN.md` §8.
8. **Stay in your area.** Edit only files within your bead's reserved area. Shared
   surfaces (§11.7) need a short explicit reservation. If you need more, reserve it,
   or file a dependency bead.
9. **Seeds.** Every failing case you fix goes into `sim/corpus/` in the same commit
   (shrunk scenario if available).
10. **Robot mode.** Use `bv --robot-*` and `br … --json`. Never launch interactive TUIs.
11. **Code design (§5.2).** Follow CD-2 to CD-12. In particular: model state so invalid
    combinations cannot be built (CD-2); parse input into domain types at the boundary
    (CD-3); use the dedicated ID/index types, never bare integers (CD-4); no wildcard
    match arms on domain enums (CD-5); assert invariants where state changes (CD-7);
    give every new queue, map or cache a registered bound (CD-9); add every new
    dependency to `DEPENDENCIES.md` with its purpose, version and licence (CD-12).
    **Do not disable or `allow` any lint that enforces a CD principle.**
12. **Registry (§10.13).** Register every new check in `registry/checks.toml` with its
    code marker, executable selector, minimum case count, covered requirements and
    claim class. **Never edit a requirement's status by hand:** activation is derived
    by `./dev commit --close` when the last listing bead closes (§10.13), and the
    registry gate rejects hand-edited statuses. Never delete registry rows, golden
    files, corpus entries or bug variants to make a gate pass.
13. **Claims (§10.15).** In bead closures, commit messages and reports, state claims in
    their class. "N seeds passed" is simulation evidence, never proof.
14. **Robot output (O-6).** Consume tool output via `--json`, never by scraping text.
    If you change a JSON output, update its schema and version.
15. **Commits (§11.10).** Commit only through `./dev commit`. Never `git add -A` in the
    shared checkout. Never create branches or worktrees (including `ntm --worktrees`).
16. **Evidence.** Close a bead only with a passing acceptance receipt for the current
    tree. Never mark a check `passed` by hand; statuses come from receipts.
17. **Test-only code.** Failpoints and bug variants live behind test-only builds and
    are never enabled in a production build.
18. **Node data.** Manage nodes only through `./dev cluster …`. Never kill processes by
    port or with broad patterns, and never delete or rewrite a node's data directory.

---

## Toolchain: Rust & Cargo

Cargo only — never any other build or package tool.

- **Toolchain:** pinned **stable** via `rust-toolchain.toml` (W01 pins the exact
  version; bumping it is a deliberate change — a bump can strengthen lint groups
  underneath us, so re-run the full lint gate and record the bump in its own bead).
- **Edition:** 2024.
- **Workspace:** virtual manifest at the root; crates live under `crates/` per
  PLAN.md Appendix A. Crates are created by the first bead that needs them (P6:
  no empty crates).
- **Determinism note (T-3):** "same build" covers toolchain, target, lockfile,
  vendored-library patch, configuration, scenario version and PRNG implementation.
  `Cargo.lock` is committed; toolchain and lockfile changes invalidate trace
  comparisons.

### Crate tiers (Appendix A — the tier decides which rules bind)

| Tier | Crates | Extra rules |
|---|---|---|
| Deterministic — domain (pure) | `types`, `statemachine` | Rules 4 & 5; std + vendored Raft + approved pure libs only; missing docs on public items denied (CD-11) |
| Deterministic — engine (no ambient I/O) | `node`, `journal`, `raft_adapter` | Same as domain |
| Adapters + composition roots | `fs_storage`, `server`, `sim` | Translate only; no domain logic (CD-1) |
| Oracles (no production deps) | `model`, `checker` | Depend on **no** production crate |
| Tools | `cli`, `xtask` (`./dev`), `conformance/` (separate package, HTTP only) | — |

One crate (or noted leaf module) = one **area** for file reservations. Shared surfaces
(workspace `Cargo.toml`, `Cargo.lock`, `AGENTS.md`, `DEPENDENCIES.md`,
`api/openapi.yaml`, `registry/*`, `.beads/`, module export facades) are edited only
under a short, explicit reservation (§11.7).

---

## Lint Policy (strict — this is a greenfield project; no legacy allowances)

Lints are configured in W01 (workspace policy + probe tests) and W04 (determinism
lint), pinned to the project toolchain, and **tested**: a probe file that violates a
denied lint must be rejected by the gate (W01 acceptance). Documentation of a lint is
never assumed to match its behaviour — the probe is the proof.

Baseline, in `[workspace.lints]` of the root `Cargo.toml`:

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
dbg_macro     = "deny"
```

With, in `clippy.toml` (tests may use these — CD-8):

```toml
allow-unwrap-in-tests = true
allow-expect-in-tests = true
allow-panic-in-tests  = true
```

Additionally, in deterministic crates:

- **Wildcard enum match arms denied** (CD-5), plus an AST check for catch-all
  identifier arms (`_x => …` spelled as a binding).
- **Missing docs on public items denied** (CD-11): every public domain type documents
  its invariants.
- **Determinism lint (W04):** `disallowed-methods` / `disallowed-types` covering at
  least `std::time::Instant::now`, `std::time::SystemTime::now`, `std::thread::spawn`,
  unseeded/thread-local RNG, default-hashed `HashMap`/`HashSet`, and direct
  filesystem/network I/O — each entry's `reason` names the port to use instead.

Escape-hatch policy:

- **Never** `allow` a lint that enforces a CD principle, and never silence the
  determinism lint (rules 4 and 11). No exceptions.
- Any other `allow` is narrow (one site, or one documented group), carries a comment
  stating what fired, why it is acceptable here, and the bead ID that decided it —
  the lint configuration doubles as a decision log. Undocumented allows are defects.
- `#[allow]` never appears in a commit just to get a gate green. Fix the code or file
  the bead.

Invariant style (CD-6/CD-7): expected failures are typed results (error enums, no
string errors). Broken invariants use `assert!` — not `debug_assert!` — because
invariants are checked **in all builds** (performance is not a v1 concern). Assertion
messages carry node, index and invariant ID.

---

## Dependencies (CD-12)

- Deterministic crates start from **the standard library plus the vendored Raft
  library** (`vendor/raft/`, pinned 0.7.0 with recorded checksums). Anything else in a
  deterministic crate needs a `DEPENDENCIES.md` entry establishing that it is pure
  (no ambient time, entropy, threads or I/O).
- Every direct dependency anywhere in the workspace is listed in `DEPENDENCIES.md`
  with purpose, scope, version, licence and source, in the same commit that adds it.
  The T0 gate checks direct dependencies ⊆ `DEPENDENCIES.md`.
- Never add: an async runtime, an HTTP stack, RNG, or clock access to a deterministic
  crate — that is what adapters and ports are for (rules 4–5). A crate that pulls in
  its own RNG or clock transitively threatens determinism (P4): check features and
  transitive deps before proposing it.
- `Cargo.lock` is committed. Version bumps are their own beads, not riders.

---

## Compiler Checks & Gates (CRITICAL)

After any substantive change, before handing off or closing a bead:

```bash
rch exec -- cargo check --all-targets
rch exec -- cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

- Offload builds through `rch exec --` so one agent does not starve the machine.
- Enumerate whole-tree breakage in one pass (collect all errors, then fix), rather
  than one error at a time.
- `ubs <changed-files>` before every commit (suite rule).
- Once W01 lands: `./dev gate fast` is the T0 gate (fmt, clippy, dependency rule,
  banned imports, registry checks); `./dev doctor` verifies the environment;
  `./dev check <ID>` runs one registered check. Gate receipts — not your say-so —
  are what close beads (rule 16).

---

## Testing

Policy (see PLAN.md §10 for the full verification architecture):

- **Unit tests** live inline (`#[cfg(test)]`) in the crate they test and cover the
  happy path, edge cases (empty, max, boundary — e.g. key length 255/256/257,
  multibyte UTF-8) and error conditions. Tests may use `unwrap`/`expect`/`panic!`.
- **Integration tests** live in the owning crate's `tests/` directory. Never add tests
  to a workspace-root `tests/` directory: the root manifest is virtual, so Cargo
  silently never compiles them.
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

---

## Multi-Agent Workflow (this repo's conventions)

- Register with Agent Mail using this repo's absolute path as `project_key`; reserve
  your bead's **area** before editing, with the bead ID in `reason`.
- Bead ID = Mail `thread_id`; subjects start `[br-…]`; announce start and completion
  in-thread. Check your inbox at natural pause points; acknowledge what you read.
- Work selection: `bv --robot-triage` → claim in `br` → reserve → work → gate →
  `./dev commit --bead <id>` → release reservations → close with the receipt.
- The tool-managed sections below (Agent Mail, Beads, bv, ee) are appended by those
  tools; edit them only via the owning tool (`am docs`, `br agents`, `bv --agents-*`,
  `ee export agentsmd`).

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
