# Upstream raft-rs tests against the entropy patch (W02.3)

**Bead:** br-w02-3-entropy-patch-ehv (part of W02, PLAN.md §7.3, D-57).
**Patched tree:** `vendor/raft` subtree `f67ae9037fe14fdd7886dfb4648837c64aa44b5e`.
`third_party/raft.patch` has sha256 `4b8572595a3a2ceb237207e3be6f3644764b5a78026a5e9ec958fabcd31412ad`;
`vendor/raft/VENDOR.toml` has `7209c82d24dd4d76ce42d4fce2ceafe4bd9e95aec320913f4d8aefa2415368be`.
**Baseline:** the unpatched `vendor/raft` as committed in 07e9ec7 (subtree
`4ab81467a57ce9c003e4bdfc244681dd5b5ba861`).
**Toolchain:** 1.99.0 (the project pin), x86_64-unknown-linux-gnu. **Date:** 2026-10-03.
**Claim class (§10.15):** test evidence for the stated trees and procedure. It shows that
every upstream test passing on the unpatched crate also passes on the patched one. It is
not a proof of behavioural equivalence. The determinism proof is W02.4.

## Result

Both runs used the same workspace, lockfile and flags; only the `raft` crate and the six
harness construction calls differ.

| Suite | Baseline (unpatched) | Patched |
|---|---|---|
| upstream harness integration tests (`harness/tests`) | 198 passed | 198 passed |
| `raft` unit tests | 46 passed | 48 passed (the 2 new `entropy::tests`) |
| `raft` doc tests | 14 passed | 14 passed |
| `datadriven` unit and doc tests | 7 + 1 passed | 7 + 1 passed |
| examples (`five_mem_node`, `single_mem_node`) | build | build |
| **failed / ignored** | **0 / 0** | **0 / 0** |

## What was run

The workspace is assembled outside the repository from:
- `raft/` and `raft-proto/`: copies of this repository's `vendor/raft/` (patched, or the
  baseline subtree);
- `harness/` and `datadriven/`: upstream raft-rs at the vendored commit
  `10c6e9db6792b85c81784e44fc278f895d5f0ab0`. The source is
  `https://codeload.github.com/tikv/raft-rs/tar.gz/10c6e9db6792b85c81784e44fc278f895d5f0ab0`,
  sha256 `9feb02ea885ed26225bf30d41132e190542b880f86fbfa37a65a6658df407f32`. Upstream's
  `src/` at that commit is byte-identical to the vendored `raft/src/` before the patch,
  which independently confirms W02.1's provenance.

The workspace manifest:

```toml
[workspace]
members = ["raft", "raft-proto", "harness", "datadriven"]
resolver = "2"

# Upstream builds raft's datadriven tests against its own in-repo crate.
[patch.crates-io]
datadriven = { path = "datadriven" }
```

The command was `RUSTFLAGS=--cap-lints=warn cargo +1.99.0 test --workspace --no-fail-fast`.

### Environment adjustments, applied identically to both runs

None of these touch the vendored crates:

- **Lockfile:** upstream's own `Cargo.lock` (shipped in the crate as
  `vendor/raft/raft/Cargo.lock`) with two updates for the current toolchain.
  - `time` 0.3.20 → 0.3.44, with `time-core` 0.1.6 and `time-macros` 0.2.24. The old
    `time` fails to compile on new rustc (`E0282` type inference). It is pulled in only by
    upstream's test logging (slog-term).
  - `serde` 1.0.152 → 1.0.229, aligned with `serde_derive` 1.0.229.
- **`[patch.crates-io] datadriven`:** the published manifest names crates.io `datadriven`
  0.1.0, whose API differs from the in-repo crate upstream's tests are written against.
- **`--cap-lints=warn`:** upstream's `lib.rs` has `#[cfg(test)] pub mod raft;` without a
  module doc under `#![deny(missing_docs)]`. Our rustc enforces that in test builds and
  upstream's 2023 rustc did not.

### Harness adaptation (patched run only)

Six construction calls pass a seeded source, and the manifest points at the workspace's
crates. `prost-codec` is dropped because the vendored raft-proto has no prost code
generation (W02.1). Nothing else in the harness changed. Each node's seed is its ID, so
nodes draw different timeout sequences, as with independent random sources.

```diff
--- a/harness/Cargo.toml
+++ b/harness/Cargo.toml
@@ -16,3 +16,2 @@
 protobuf-codec = ["raft/protobuf-codec"]
-prost-codec = ["raft/prost-codec"]
 
@@ -20,4 +19,4 @@
 [dependencies]
-raft = { path = "..", default-features = false }
-raft-proto = { path = "../proto", default-features = false }
+raft = { path = "../raft", default-features = false }
+raft-proto = { path = "../raft-proto", default-features = false }
 rand = "0.8"
--- a/harness/src/network.rs
+++ b/harness/src/network.rs
@@ -94,3 +94,3 @@
                     config.id = *id;
-                    let r = Raft::new(&config, store, l).unwrap().into();
+                    let r = Raft::new(&config, store, raft::SplitMix64::new(*id), l).unwrap().into();
                     npeers.insert(*id, r);
--- a/harness/tests/test_util/mod.rs
+++ b/harness/tests/test_util/mod.rs
@@ -109,3 +109,3 @@
 pub fn new_test_raft_with_config(config: &Config, storage: MemStorage, l: &Logger) -> Interface {
-    Interface::new(Raft::new(config, storage, l).unwrap())
+    Interface::new(Raft::new(config, storage, raft::SplitMix64::new(config.id), l).unwrap())
 }
--- a/harness/tests/integration_cases/test_raft.rs
+++ b/harness/tests/integration_cases/test_raft.rs
@@ -4409,3 +4409,3 @@
     let s = MemStorage::new_with_conf_state((vec![1, 2], vec![]));
-    let raft = Raft::new(&invalid_config, s, &default_logger());
+    let raft = Raft::new(&invalid_config, s, raft::SplitMix64::new(1), &default_logger());
     assert!(raft.is_err())
--- a/harness/tests/integration_cases/test_raw_node.rs
+++ b/harness/tests/integration_cases/test_raw_node.rs
@@ -86,3 +86,3 @@
     }
-    RawNode::new(config, storage, logger).unwrap()
+    RawNode::new(config, storage, raft::SplitMix64::new(config.id), logger).unwrap()
 }
@@ -725,3 +725,3 @@
         store.wl().set_hardstate(hard_state(1, 3, 0));
-        RawNode::new(&new_test_config(1, 10, 1), store, &l).unwrap()
+        RawNode::new(&new_test_config(1, 10, 1), store, raft::SplitMix64::new(1), &l).unwrap()
     };
@@ -1860,3 +1860,3 @@
     let config = new_test_config(1, 10, 1);
-    let mut raw_node = RawNode::new(&config, s, &l).unwrap();
+    let mut raw_node = RawNode::new(&config, s, raft::SplitMix64::new(1), &l).unwrap();
 
```

The harness's own `rand` use (`network.rs` random message drops; one `test_raw_node` case)
is upstream test scaffolding. It is not compiled into keyval and does not feed the
patched crate.

## Limits

- This run is not yet a registered check; it runs outside the repository. The audit's
  follow-up br-9dug (RAFT-AUDIT) covers the static half. A re-runnable `./dev` check of this
  suite (T1-sized: it builds criterion and upstream's dev-dependencies) would need the
  harness and datadriven vendored. That is proposed as a follow-up rather than done here.
- Upstream's `failpoints` tests (`harness/tests/failpoints_cases`, feature `failpoints`)
  were not run. keyval never enables the feature (spike/raft-audit.md, F-1).
