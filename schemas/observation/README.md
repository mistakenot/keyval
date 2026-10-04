# `schemas/observation/` — the observation schema (the oracles' contract)

One simulation run's observations: what the simulated clients and the harness saw,
recorded independently of anything the system claims. The oracles read **only** this,
never production types: the reference model (`crates/model`), the checkers K-1 to K-12
and non-vacuity (K-13, §10.16). That is T-12's "documented observation schema" (PLAN.md
§10.2 "Outputs", §10.5, §10.14).

| Identifier | Schema | Fixtures |
|---|---|---|
| `keyval.observation/v1` | `observation.v1.schema.json` | `fixtures/run.v1.json`, `fixtures/drain.v1.json`, `fixtures/outcomes.v1.json` |

- `fixtures/run.v1.json` is a `marker` heal: a partition, a crash whose sync completion
  reaches the restarted incarnation stale (B-17), a stall and a permanent loss.
- `fixtures/drain.v1.json` is a `drain` heal (a DM-7 history-capacity profile): a leader
  crash that leaves a suffix to truncate, an old-term entry the next leader commits by
  prefix, `history_full`, quiescence, a certified drain, and resolution by status lookup
  on every recoverable node.
- `fixtures/outcomes.v1.json` holds one valid response outcome for every (operation,
  code) pair §8.4 and `api/openapi.yaml` allow.
- `invalid/<case>.json` holds the negative fixtures: a document with one defect, the rule
  it breaks, and the JSON pointer (`error_at`) where the schema rejects it.

Both runs keep the stream rules below and the correct-run properties listed after them.
Their payloads, digests and states are computed, not typed.
`./dev check OBSERVATION-SCHEMA` runs all of this, and this README's coverage of the
record kinds.

## The document

One JSON object per run, in the repository's schema conventions (schemas/dev/README.md:
closed objects, every field present, `null` for an absent value), with every integer
below 2^53 (§8.1), the run's `seed` included, so the simulator draws seeds in that range.

- `schema`: `keyval.observation/v1`.
- `manifest`: the run manifest (§10.2) with the swarm's fault selection (§10.4), plus
  what the oracles need that the fingerprint hides:
  - `heal`: `marker`, `precreated_marker` (DM-6 profiles) or `drain` (DM-7 profiles);
  - `tick_us`: the tick length, so tick budgets compare with `time`;
  - `quiet_ticks`: the drain's quiet period;
  - `limits`: the semantic limits the reference model takes (§7.10).
- `voters`: the configured voter IDs (one, three or five).
- `complete`, `overflowed`, `event_budget` and `scenario`: a required observation that
  overflowed its bound is listed in `overflowed` (§7.11), `event_budget.exhausted` says
  whether the scheduler ran out of events, and `scenario` says whether the scenario kept
  the healthy envelope's bounds, naming the one it broke if not (§9.4). Any of the three
  makes the run **incomplete**, which fails (P10, §10.16). The schema enforces both
  directions.
- `records`: the observations, in event order.

Every record has `kind`, `seq` (the scheduler event that produced it) and `time` (virtual
microseconds). Node-side records name the node incarnation (`{node, incarnation}`).

## Record kinds

| Kind | What it records | Read by |
|---|---|---|
| `phase` | A run phase begins: setup, baseline, chaos, heal, check (§10.2) | K-9, K-13 |
| `fault` | A fault started or ended: type, nodes, a crash point per crashed node, partition groups or links (§9.1, §10.2) | K-9, K-13 |
| `invoke` | A client attempt: client, attempt ID, target incarnation, request ID (writes only), misuse flag, operation (§10.3) | K-1, K-2, K-3, K-4, K-5, K-10, K-12 |
| `attempt` | A write attempt's §7.8 lifecycle on its node: admitted, short_circuit, attached (to which attempt), submitted (at the consensus hand-off), dropped, terminal | K-12 |
| `response` | A node incarnation generated an attempt's response: its generation order, the operation, and the outcome as the client sees it (§8.2, §8.4) | K-1, K-2, K-3, K-4, K-5, K-6, K-10, K-12, K-13 |
| `delivery` | One copy of a response reached its client, or was lost | K-3, K-10, K-12 |
| `append` | An entry appended to a node's log: index, term, exact payload bytes | K-1, K-8, K-11, K-12 |
| `truncate` | A node removed its log suffix from an index on | K-1, K-8, K-11 |
| `recovered` | A node incarnation began: its recovered durable state and whole log | K-1, K-8, K-11 |
| `sync` | A durability obligation (term, vote, the log as requested up to an index) requested, completed, or delivered stale to a later incarnation (DUR-2, B-17) | K-11 |
| `accept` | A replica incarnation accepted an entry (a follower when it acknowledges it, the leader when its covering sync completes), in which term, with the payload's digest (commit certificates) | K-8, K-11 |
| `release` | A vote grant, append acknowledgement, client success or published read, with the attempt it answers (the last two) and the DUR-2 obligation it depends on | K-11 |
| `committed` | An entry committed by certificate: index, term, payload bytes and the decoded command | K-1, K-2, K-4, K-5, K-8 |
| `node_state` | A node's god-view state after a step that changed it: process, lifecycle, role, term, vote, leader, commit, applied, state digest (T-5) | K-1, K-5, K-7, K-8, K-9 |
| `marker` | A node applied the catch-up marker, with its state digest then | K-7, K-9, K-13 |
| `lost` | The permanent-loss set so far (at most two nodes) | K-1, K-7, K-9, K-13 |
| `quiescence` | All client activity stopped: nothing queued or in flight on any node | K-9, K-12 |
| `drain` | A certified drain: stable leader, last = commit = applied with identical digests on every recoverable node, over the quiet period | K-7, K-9, K-13 |
| `resolution` | How a logical write was resolved (executed with its index, not executed, refused, or unresolved), with the attempts that decide it (§10.2, §9.3) | K-9, K-10, K-12, K-13 |

K-10 is black-box (§10.5): it reads only `invoke`, `response` as delivered, `delivery`
and `resolution`, never the god-view records.

### What the oracles derive themselves

- **Logs.** A node's log after any step is its incarnation's `recovered` log, changed by
  its `append` and `truncate` records since. K-8 judges log matching on entry bytes and
  term from these, and recomputes commit certificates from `accept`; `committed` is the
  harness's own computation, a cross-check.
- **First executions and results.** The reference model replays the `committed`
  commands, in index order; first executions, versions and decisions come from that
  replay, never from a node's claim (§10.14). `payload` is the entry's exact bytes in the
  canonical command codec (empty for a no-op): K-8 compares them, and a correct run's
  `command` re-encodes to them.
- **Durable state.** A node's durable state is its hard state (term and vote) and its
  log *contents* as of the latest completed sync's request, or as recovered: a sync
  covers the log as it was when it was requested, and a `stale` sync covers nothing. An
  obligation (`release.requires`) is met when its hard state, if it has one, is the
  durable hard state, and the durable log holds the node's current log up to
  `last_index`. So after a truncation a replaced entry is uncovered until a later sync,
  however long the durable log is. A vote grant carries a hard state only (its
  `last_index` is 0); an append acknowledgement carries a hard state and the
  acknowledged log; a client success (a terminal execution decision: a write's `ok`,
  `version_mismatch` or `capacity_exceeded`) or a published read (a read answered with
  `served_by`) carries the decided or served log only, since short circuits and reads
  never wait for a term or vote sync (F-7). A pre-vote grant persists nothing and is not
  a release.
- **Node state.** A `node_state` holds until that node's next one.

### Digests

- `accept.payload_digest`: SHA-256 of the entry's payload bytes, which for a command is
  its DM-5 command digest.
- `state_digest` (`node_state`, `marker`, `drain`): SHA-256 of the node's applied map and
  request history in the `keyval.state/v1` encoding. The oracles compare state digests
  for equality; the encoding is documented in `crates/statemachine/src/digest.rs`, and
  the fixtures compute it.

## Stream rules

The schema checks each record; these rules span records. The producer guarantees them
whatever the system under test does, and the checkers rely on them:

1. `seq` and `time` never decrease along `records`. Within one step, records are in
   causal order: a `node_state` is recorded at the point of its change, before any later
   record of that step.
2. The phases appear once each, in order. The heal starts `chaos_ticks` × `tick_us`
   after the chaos phase, and the harness ends the heal at its budget, so the check
   starts within `heal_budget_ticks` × `tick_us` of the heal.
3. Every attempt ID is invoked exactly once, before any `attempt`, `response` or
   `delivery` record names it. Its `attempt` and `response` records come from the
   incarnation it was sent to, a response repeats the invoked operation, and an attached
   attempt waits on an attempt with the same request ID at the same incarnation.
4. Every `delivery` follows a response to its attempt (more than one copy when
   duplicated). A response carrying a terminal execution decision, or a read answered
   with `served_by`, follows in its step its incarnation's `client_success` or
   `published_read` release naming the attempt, whose `last_index` is at least the
   decided or served index.
5. Response generations strictly increase per node incarnation.
6. Each node's records start with a `recovered` record per incarnation. Incarnations
   count up from 0, and every node-side record and invocation target names a configured
   voter's current incarnation (a `stale` sync names the incarnation it reached). A recovered durable state describes
   the recovered log: its length and last term.
7. An `append` extends its node's log by one entry, a `truncate` removes an existing
   suffix, and an `accept` names an entry its replica's log holds. Sync IDs are unique; a
   completed sync names a request of its own incarnation, a stale one a request of an
   earlier incarnation, both with the same `covers`, which describes a prefix of the log
   as requested.
8. `committed` records name indices 1, 2, 3, … once each, in order. Each follows a
   certificate for its index or a later one: accepts from a majority of distinct voters,
   in the certified entry's own term, whose logs held this entry (K-8: a certified entry
   and the prefix before it). Its payload is empty exactly when it has no command.
9. The manifest selects each fault type once, and only selected faults are injected,
   on configured voters. A fault ends only after it started, partition groups are
   disjoint, and a crash names one crash point per crashed node.
10. `event_budget.exhausted` holds exactly when `used` reached `max_events`. After
    `quiescence`, every invocation is a status lookup. A `drain` holds over the
    manifest's `quiet_ticks` and follows `quiescence` by at least `quiet_ticks` ×
    `tick_us`.
11. Every logical write has exactly one `resolution`, after all its attempts' records,
    and its evidence is answered attempts of that write or status lookups of it.

## Correct runs

These are checker verdicts, not producer guarantees: a run of a broken system breaks
them, and its observations must still be recordable. Both fixture runs keep them, and
the tests check that they do:

- every release and acceptance is durable first, by the rule above (K-11);
- every `served_by` names the responding node, and its prefix was applied and committed
  there before it answered (K-5);
- an attempt gets at most one response (K-12);
- every committed command is one a client invoked with that request ID, and its payload
  is that command's canonical encoding; the first committed command for a request ID is
  its logical write's own, since misuse follows the original's terminal result (§10.3),
  though a misuse command may commit later as a no-effect conflict (§7.5 step 3, K-2)
  (K-4);
- every fault type the manifest selected is injected (§10.16);
- every recoverable node (the voters less the permanently lost) applies the marker with
  one state digest in a `marker` heal, or is in the certified drain in a `drain` heal
  (K-7);
- every logical write is resolved, none `unresolved` (K-9).

## Producers

The simulation's recorders (W17's harness, W36's workload recording) write this
document, and their tests validate real output against the schema (O-6). No producer
exists yet, so the fixtures stand in until then. Change the schema in the same commit as
its producers. A change that breaks a reader (a field renamed, removed or retyped, or a
value no longer allowed) bumps the version. Never edit a fixture to make a test pass:
fixtures are oracles (P9), and a change to one is reviewed like a change to a checker.
