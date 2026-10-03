# Nondeterminism and unbounded-fetch audit of vendored raft-rs 0.7.0 (W02.2)

**Bead:** br-w02-2-nondeterminism-audit-8sj (part of W02, PLAN.md §7.3, §12.2 W02; P4, T-3, T-11).
**Audited tree:** `vendor/raft/` as committed in 07e9ec7 (`vendor/raft` subtree
`4ab81467a57ce9c003e4bdfc244681dd5b5ba861`). That is crates.io `raft` and `raft-proto` 0.7.0,
upstream commit `10c6e9db6792b85c81784e44fc278f895d5f0ab0`, with codegen pinned
(`vendor/raft/VENDOR.toml`).
**Build audited:** `raft` with `default-features = false, features = ["protobuf-codec"]`, the
only configuration keyval builds. The protobuf runtime is 2.28.0. Toolchain 1.99.0.
**Author and date:** VioletOwl, 2026-10-03.
**Claim class (§10.15):** source-reading analysis, so every verdict here is an
**Assumption** until the test named against it passes. Nothing here is simulation evidence.

## Method

I read every function reachable from the five entry points §7.3 names, plus the
`RawNode` wrappers keyval calls:

- **Construction:** `RawNode::new` → `Raft::new` → `confchange::restore`, `post_conf_change`,
  `load_state`, `commit_apply`, `become_follower` → `reset`.
- **Tick:** `tick` → `tick_election` (→ `step(MsgHup)`) and `tick_heartbeat`
  (→ `step(MsgCheckQuorum)` and `step(MsgBeat)`).
- **Campaign:** `hup` → `campaign` → `become_pre_candidate` / `become_candidate` → `poll` →
  `become_leader` → `bcast_append`.
- **Step:** term handling in `step`; vote handling and `maybe_commit_by_vote`;
  dispatch to `step_leader`, `step_candidate` and `step_follower`; `handle_append_entries`,
  `handle_append_response` → `maybe_send_append`; `maybe_commit`.
- **Timeout reset:** `reset` → `reset_randomized_election_timeout`.

I then searched the whole vendored source, and the protobuf runtime it links, for the
classes §7.3 lists: entropy, clocks, threads and synchronisation, hashed collections and
their iteration, pointer formatting, logging, and entry fetches without a size bound.
Test modules (`#[cfg(test)]`, `datadriven_test.rs`) and doc examples were checked
separately and are excluded from verdicts unless noted.

## Summary

| Class | Verdict |
|---|---|
| Entropy | **One source:** `rand::thread_rng()` in `reset_randomized_election_timeout`. Construction and every reset reach it. W02.3 patches it. |
| Clocks | None in library code. Time enters only as `tick()` calls. |
| Threads and sync | None on production paths. |
| Hashed collections | FxHash (fixed, unseeded) everywhere on production paths. Iteration order is deterministic for the same build (T-3), and decision logic doesn't depend on it. |
| Pointer formatting | None. |
| Logging | Through the caller's explicit `slog::Logger`. Core slog reads no clock or environment. |
| Unbounded fetches | **Two:** `hup` (election path) and `maybe_commit_by_vote` (commit path). Two config-level limits: Ready's committed entries, and append messages. Each has a bounded-handling policy below. |

No finding requires touching vote, log-matching, quorum or commit rules (D-57).

## Findings

Locations are relative to `vendor/raft/raft/src/` unless they say otherwise.

### E-1: the only entropy source (election timeout)

- **Where:** `raft.rs:2807-2819`, `reset_randomized_election_timeout`. It calls
  `rand::thread_rng().gen_range(min_election_timeout..max_election_timeout)`. `thread_rng` is
  OS-seeded and per-thread, so it differs on every run and in every process.
- **Reached from:**
  - Construction: `Raft::new` → `become_follower` → `reset` (`raft.rs:986-1018`, call at
    992).
  - Every `become_follower` (a higher-term message, a lost vote, a step-down),
    `become_candidate` (campaign) and `become_leader` (won vote).
  - Not `become_pre_candidate`, which keeps the current timeout (`raft.rs:1168-1193`).
- **Why setting it from outside is not enough:** `set_randomized_election_timeout`
  (`raft.rs:474`) is overwritten on the next `reset`, as §7.3 notes.
- **Range:** `[min_election_tick, max_election_tick)`, which defaults to
  `[election_tick, 2 × election_tick)`. W08 already treats this shape as a validation rule
  (D-75).
- **Search result:** `rand::` appears nowhere else in `raft/src` or `raft-proto/src`, and
  nothing else in the audited build draws entropy.
- **Policy (W02.3):**
  - The patch defines a mandatory entropy trait inside the vendored crate.
  - `Raft::new` and `RawNode::new` take an implementation, with no ambient default.
  - `reset_randomized_election_timeout` draws from it.
  - The `rand` dependency is removed from `vendor/raft/raft/Cargo.toml`, so ambient
    entropy becomes unreachable rather than merely unused. VENDOR.toml and RAFT-VENDOR
    record the change.
- **Test:**
  - W02.3: the same seeded source produces the same timeout sequence across construction
    and resets, and `thread_rng` appears nowhere in vendored source.
  - W02.4: the fresh-process trace identity covers it end to end.

### C-1: clocks

- **Where:** `Instant` and `Duration` appear only in doc examples (`lib.rs:63-148`).
  "timeout" in `send_timeout_now` (`raft.rs:2832`) is a message type, not a clock.
- **Verdict:** no clock on any path. Time is logical: `election_elapsed` and
  `heartbeat_elapsed` advance only in `tick_election` and `tick_heartbeat`
  (`raft.rs:1081-1124`).
- **Policy:** none needed.
- **Test:** proven by search. Any future regression would be caught by a static scan
  (see "Executable follow-up").

### T-1: threads and synchronisation

- **Where:**
  - `default_logger()` (`lib.rs:574-599`) uses `static mut`, `Once`, `Mutex` and
    `thread::current()`. It is compiled only under `cfg(test)` or the `default-logger`
    feature, which keyval never enables (VENDOR.toml `[features]`), and the same goes for
    `Raft::with_default_logger` and `RawNode::with_default_logger` (`raft.rs:415`,
    `raw_node.rs:326`).
  - `MemStorage` (`storage.rs:373-440`) uses `Arc<RwLock<_>>`. It is upstream's test
    storage; keyval's `Storage` is the W13 journal adapter.
  - protobuf's `CachedSize` uses a relaxed `AtomicUsize`. Single-threaded use is
    deterministic.
- **Verdict:** none on keyval's paths. `RawNode` is documented "thread-unsafe"
  (`raw_node.rs:284`) and spawns nothing.

### H-1: FxHash collections and their iteration order

- **Where:** `lib.rs:602-604` aliases `HashMap` and `HashSet` to
  `BuildHasherDefault<FxHasher>`. FxHash is fixed and unseeded, so a map's iteration order
  is a deterministic function of its insertion sequence, its capacity and the std hashbrown
  implementation. That is: the same build (T-3: toolchain, target and lockfile).
- **Order-sensitive uses** (all deterministic for a given build):
  - Message emission order:
    - `bcast_append` (`raft.rs:901-909`) and `bcast_heartbeat_with_ctx` (`raft.rs:925-935`)
      iterate the progress map;
    - `campaign` (`raft.rs:1282`) iterates the voter set;
    - the probe loop in `post_conf_change` (`raft.rs:2700-2708`), which only a leader
      reaches after a membership change, and keyval has none.
  - `ConfState` vectors from `to_conf_state` (`tracker.rs:159-168`, "we don't sort").
    `conf_state_eq` accepts either order (`raft-proto/src/confstate.rs`).
  - Log text (`Debug` of sets).
- **Order-insensitive decisions** (verified):
  - `committed_index` sorts the acked indexes before choosing the quorum index
    (`quorum/majority.rs:76-95`);
  - `vote_result`, `tally_votes` and `quorum_recently_active` count
    (`tracker.rs:300-345`).
  - Group commit, the only branch that reads `group_id` order, is off by default and
    keyval never enables it.
- **Policy:**
  - None in the library. Deterministic per build is exactly T-3's contract.
  - W13 keeps emission order in the canonical trace, because it is part of the run. A
    toolchain bump re-baselines traces anyway (T-3).
  - Optionally, W13 may sort outgoing messages by (destination, type) if it wants trace
    stability across hashbrown versions. Not required.
- **Test (W02.4):** the 100 seeds × 3 fresh processes trace identity, including
  crash-restart. After a restart the tracker is rebuilt from the same `ConfState`, so its
  insertion order repeats.

### H-2: the std `HashMap` with `RandomState` in `quorum.rs`

- **Where:** `quorum.rs:7,67`, `type AckIndexer = std::collections::HashMap<u64, Index>`,
  which is randomly seeded.
- **Verdict:** unreachable for keyval. It is used only by the quorum datadriven tests, and
  only through `get` (`quorum.rs:69-74`), never iterated. Production commits use
  `ProgressMap` (FxHash) as the `AckedIndexer` (`tracker.rs:181`).
- **Policy:** none.

### H-3: protobuf unknown fields

- **Where:** `protobuf-2.28.0/src/unknown.rs:186` stores unknown fields in
  `HashMap<u32, UnknownValues, BuildHasherDefault<DefaultHasher>>`. That is SipHash with
  fixed keys, so it is deterministic per build. It is written back when a message is
  re-encoded, and the generated `eraftpb.rs` reads and writes it (lines 204, 233, 257).
- **Reached when:** a decoded `eraftpb` message carries fields absent from
  `eraftpb.proto`. keyval peers run the same build, so their messages never do.
- **Policy (W13):** the peer-envelope decoder rejects a `Message` whose unknown fields are
  non-empty, counting it as a malformed envelope (§8.2). Rejecting it makes the case
  unreachable rather than relying on every peer running the same build.
- **Test (W13):** codec test with an unknown field present → dropped and counted.

### L-1: logging side effects

- **Where:** `info!`, `debug!`, `warn!` and `trace!` throughout. `Raft::new` derives a
  child logger (`raft.rs:329`). `fatal!` panics.
- **Verdict:** slog core reads no clock, environment or entropy. Timestamps and
  environment filtering come only from drains (`slog-term`, `slog-envlogger`), which are off
  with `default-logger`. Arguments are formatted only when a drain consumes the record.
  Logging cannot change state.
- **Policy (W13, §7.3 "explicit logger"):**
  - Pass an explicit `slog::Logger`: `slog::Discard`, or a deterministic drain feeding the
    node's debug log, never the canonical trace.
  - Never `with_default_logger`; it is not compiled in our build anyway.

### P-1: pointer formatting

- **Verdict:** none. There is no `{:p}` or `as *const` in vendored source.

### F-1: failpoints

- **Where:** `fail_point!` only under feature `failpoints` (`raft.rs:30`, `raft.rs:1457`).
- **Verdict:** never enabled (VENDOR.toml `[features]`).
- **Policy:** bug variants are injected in `raft_adapter` or the test composition root,
  never in vendor (D-57).

### U-1: unbounded fetch on the election path (`hup`)

- **Where:** `raft.rs:1516-1575`. On every `MsgHup` (each follower or candidate election
  timeout, repeated throughout a leaderless period), `hup` slices
  `[unstable first index or applied + 1, committed + 1)`.
  - It uses `max_size = None` and `GetEntriesFor::TransferLeader`.
  - It only counts pending conf changes (`num_pending_conf`, `raft.rs:1248-1255`).
- **Size:** `committed − applied` entries, every one materialised with its data.
- **keyval specifics:** membership is static (§7.3), so the count is always 0 and the fetch
  is pure cost. The gap is small while a node applies each Ready's committed entries before
  the next Ready. It is large if the node starts with `Config.applied` below its recovered
  committed index.
- **Policy (W13):**
  1. Recovery replays the journal before constructing the `RawNode` (§7.7), and the
     adapter passes `Config.applied` = the replayed applied index, so the gap starts at 0.
  2. Committed entries are applied and `advance_apply`'d within the same Ready cycle, so
     the gap never exceeds one Ready's committed batch, which U-3 bounds.
  3. Hardening, adapter-side only: for `GetEntriesFor::TransferLeader` and
     `GetEntriesFor::CommitByVote` the journal `Storage::entries` may return entries
     with `data` elided. `num_pending_conf` reads only `entry_type`, so this changes no
     decision and bounds memory to about 40 bytes per entry.
- **Test (W13, required by §7.3):** a large-log election test. A node with a large
  committed − applied gap campaigns; peak allocation stays within the stated bound.

### U-2: unbounded fetch on the commit path (`maybe_commit_by_vote`)

- **Where:** `raft.rs:2174-2217`. A pre-candidate or candidate that *rejects* a vote request
  carrying a higher `commit`/`commit_term` it can honour fast-forwards its commit index. It
  then slices `(old committed, new committed]` with `max_size = None` and
  `GetEntriesFor::CommitByVote`, again only for `num_pending_conf`.
- **Size:** up to the candidate's whole uncommitted tail. After a long partition this
  approaches DM-7's ceilings: 1,000,000 entries and 3 GiB of command bytes.
- **Policy (W13):**
  - The adapter-side data elision from U-1 item 3, which bounds memory to about 40 MB at
    1,000,000 entries.
  - The patch must **not** alter this function: it is commit logic (D-57).
- **Test (W13, required by §7.3):** a large-log election/commit test that drives
  `maybe_commit_by_vote` across a large gap and asserts the memory bound.

### U-3: committed entries per Ready (configuration)

- **Where:** `RawNode::ready` → `next_entries_since(commit_since_index,
  max_committed_size_per_ready)` (`raw_node.rs:450-454`). The default is `NO_LIMIT`
  (`config.rs:122`).
- **Policy (W08 and W13):** the config sets `max_committed_size_per_ready` from the §7.11
  limits, as §7.3 requires. It is bounded once set.
- **Test (W08):** a test that the produced `raft::Config` never has `NO_LIMIT` here.

### U-4: append messages (configuration)

- **Where:** `maybe_send_append` fetches with `max_size = max_msg_size`, under
  `GetEntriesFor::SendAppend` (`raft.rs:791-850`, call at 815).
- **Policy (W08):** `max_size_per_msg` comes from §7.11. `Config::validate` already
  requires `max_uncommitted_size ≥ max_size_per_msg` (§7.10).
- **Test:** W08's every-accepted-fixture-constructs-a-node test covers it.

### U-5: the unstable segment in `slice`

- **Where:** `raft_log.rs:614-622`. The in-memory unstable tail is copied whole, then
  truncated by `limit_size`.
- **Verdict:** bounded by the unstable log. W13's single outstanding Ready, persisted
  before `advance`, and `max_uncommitted_size` keep that small.
- **Policy:** none beyond W13's Ready discipline (§7.4).

### U-6: `RaftLog::all_entries`

- **Where:** `raft_log.rs:395-409`, unbounded.
- **Verdict:** a `#[doc(hidden)]` test helper. keyval never calls it.

### S-1: paths keyval never reaches

- **Snapshots:** `prepare_send_snapshot`, `restore`, `handle_snapshot` and
  `request_snapshot` run only when storage reports `Compacted` or
  `LogTemporarilyUnavailable`, or a `MsgSnapshot` arrives. keyval never compacts (DUR-4) and
  never sends snapshots. W13 treats a snapshot Ready as an invariant failure.
- **ReadIndex:** `read_only.rs` (FxHash maps, lookups only) is unused, because reads are
  local (D-06).
- **Leader transfer:** `CAMPAIGN_TRANSFER`, `MsgTransferLeader` and `MsgTimeoutNow` are
  unused.
- **Joint consensus and auto-leave:** unused.
- **Policy:** W13's peer decoder may reject these message types outright, so the paths
  stay unreachable even from a misbehaving peer.

### X-1: `unsafe` in vendored code (informational, not nondeterminism)

- **Where:** `quorum/majority.rs:77-85` uses a `MaybeUninit` stack array plus
  `slice::from_raw_parts_mut`. It initialises exactly `voters.len() ≤ 7` elements before
  reading them, so it is sound by inspection. `lib.rs:583` is in `default_logger`, which is
  not compiled in our build.
- **Verdict:** recorded under Q5/D-42. Third-party internals are pinned and audited, not
  claimed CD-8 compliant. The W02.3 patch adds no `unsafe` (CD-8).

### X-2: panics inside the library (informational)

- **Where:** `assert!`, `fatal!` and `unwrap` on protocol preconditions. For example,
  `become_leader` asserts `persisted == last_index` (`raft.rs:1214`); `hup` and
  `maybe_commit_by_vote` call `fatal!` on unexpected storage errors.
- **Verdict:** not nondeterminism. W13's Ready discipline must keep the preconditions true;
  reaching one is an invariant failure.

## Tests owed, and who owes them

| Finding | Test | Owner |
|---|---|---|
| E-1 | seeded entropy source, so identical timeout sequences; no `thread_rng`/`rand` left in vendored source | W02.3 |
| E-1, H-1 | 100 seeds × 3 fresh processes, byte-identical spike traces incl. crash-restart and re-election | W02.4 |
| U-1, U-2 | large-log election/commit test with a memory bound | W13 (§7.3) |
| H-3 | peer decoder rejects a `Message` with unknown fields | W13 |
| U-3, U-4 | the produced `raft::Config` sets `max_committed_size_per_ready` and `max_size_per_msg` from §7.11 | W08 |

## Executable follow-up

The "none on any path" verdicts (C-1, T-1, P-1, F-1, and E-1's single site) are proven by
reading this tree. A static scan of `vendor/raft/*/src` outside test modules would keep
them true across a future vendor change. The scan would flag:

- `thread_rng`/`rand::` (expected 1 site before W02.3, 0 after);
- `Instant`, `SystemTime` and `thread::spawn`;
- `std::collections::HashMap` other than `quorum.rs`'s `AckIndexer`;
- `{:p}`.

It belongs beside RAFT-VENDOR in xtask, as a follow-up bead, because xtask's shared files are
currently sequenced between W03.3 and W06.2.
