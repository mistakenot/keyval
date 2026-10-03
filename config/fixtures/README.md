# Accepted config fixtures

Every `*.toml` file here is a config that `types::Config::parse` accepts (PLAN.md §7.10,
D-78). Together they are the corpus for two proofs:

- **W08.1** (`crates/types/src/config/tests.rs`): every file parses, and every voter's
  `ConsensusSettings` pass the checks of raft-rs 0.7's `Config::validate`.
- **W13** (`raft_adapter`): a raft-rs `RawNode` is constructed from every voter of every
  file, so a config the parser accepts never fails at `RawNode` construction.

| File | What it covers |
|---|---|
| `one-voter.toml` | 1 voter, production timing and limits (D-45) |
| `three-voters.toml` | 3 voters with non-consecutive IDs out of file order, up to 2^53 − 1 (D-77), and IPv6 addresses |
| `five-voters-explicit-limits.toml` | 5 voters with every optional §7.11 local limit written out |
| `small-limits.toml` | tiny capacity limits, each exactly on its rule's boundary (journal margin, send queue, uncommitted bytes) |
| `fastest-timing.toml` | the fastest legal timing, each value on its rule's boundary |

Add a file here only if it is accepted. The tests in `crates/types/src/config/tests.rs`
list every file, so a new one is added there too. Rejected configs are tested inline,
one rule each.
