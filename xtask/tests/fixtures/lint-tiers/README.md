Fixtures for xtask's LINT-TIERS tests (`xtask/src/lint.rs`). Each directory is a fake
package that breaks one lint-tier rule on purpose. They are not workspace members, and
cargo never builds them. `clippy.toml` here plays the workspace-root configuration.
