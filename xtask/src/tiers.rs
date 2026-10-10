//! Crate tiers and the dependency rule (PLAN.md CD-1, P3, T-12; Appendix A).
//!
//! A workspace crate's tier follows from its directory, which Appendix A fixes. Package
//! names may vary, directories may not. Dependencies point inward only:
//!
//! | Tier | May depend on (first-party) | Third-party |
//! |---|---|---|
//! | domain (`types`, `statemachine`) | domain | `deterministic` ledger scope only |
//! | engine (`node`, `journal`, `raft_adapter`) | domain, engine, vendored Raft | `deterministic` only |
//! | adapter (`fs_storage`) | domain, engine | ledgered |
//! | production root (`server`) | domain, engine, adapter | ledgered |
//! | simulation root (`sim`) | domain, engine, adapter, oracle | ledgered |
//! | oracle (`model`, `checker`) | oracle only (T-12) | ledgered |
//! | conformance (`conformance/`) | oracle only (PLAN.md §10.8: the reference model and the K-10 witness); never a production crate (T-12) | ledgered |
//! | CLI (`cli`) | domain, engine, adapter | ledgered |
//! | xtask, fuzz | anything | ledgered |
//! | lint probe (`xtask/probes/*`) | domain | none |
//!
//! The rule covers every dependency kind (normal, build and dev), so tests cannot carry a
//! dependency the code itself may not have. The ledger check (DEP-LEDGER) separately
//! requires every third-party dependency to be listed in DEPENDENCIES.md.

use std::path::Path;

use crate::check::StaticOutcome;
use crate::ledger::{self, Ledger};
use crate::metadata::{self, Metadata, Package};

/// Where a crate sits in the hexagonal structure (§7.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// Pure domain crates.
    Domain,
    /// Deterministic engine crates (no ambient I/O).
    Engine,
    /// Environment adapter used by the composition roots.
    Adapter,
    /// The production composition root (the server binary).
    ProductionRoot,
    /// The simulation composition root.
    SimulationRoot,
    /// Test oracles: reference model and checkers.
    Oracle,
    /// The HTTP-only conformance suite.
    Conformance,
    /// The `keyval` CLI.
    Cli,
    /// Repository tooling: xtask and the fuzz targets.
    Tooling,
    /// Lint probe fixtures (W01.3).
    Probe,
}

impl Tier {
    /// Domain and engine crates obey the determinism rules (§5.2, §7.14).
    pub const fn is_deterministic(self) -> bool {
        match self {
            Self::Domain | Self::Engine => true,
            Self::Adapter
            | Self::ProductionRoot
            | Self::SimulationRoot
            | Self::Oracle
            | Self::Conformance
            | Self::Cli
            | Self::Tooling
            | Self::Probe => false,
        }
    }

    /// Whether a crate of this tier may depend on a first-party crate of tier `to`.
    pub const fn may_depend_on(self, to: Self) -> bool {
        match self {
            Self::Domain | Self::Probe => matches!(to, Self::Domain),
            Self::Engine => matches!(to, Self::Domain | Self::Engine),
            Self::Adapter | Self::Cli => matches!(to, Self::Domain | Self::Engine | Self::Adapter),
            Self::ProductionRoot => matches!(to, Self::Domain | Self::Engine | Self::Adapter),
            Self::SimulationRoot => {
                matches!(
                    to,
                    Self::Domain | Self::Engine | Self::Adapter | Self::Oracle
                )
            }
            // PLAN.md §10.8: the conformance suite may use the reference model and the K-10
            // witness, and no production crate (T-12).
            Self::Oracle | Self::Conformance => matches!(to, Self::Oracle),
            Self::Tooling => true,
        }
    }

    /// Display name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Domain => "domain",
            Self::Engine => "engine",
            Self::Adapter => "adapter",
            Self::ProductionRoot => "production root",
            Self::SimulationRoot => "simulation root",
            Self::Oracle => "oracle",
            Self::Conformance => "conformance",
            Self::Cli => "cli",
            Self::Tooling => "tooling",
            Self::Probe => "lint probe",
        }
    }
}

/// The tier of the crate whose directory is `dir` (relative to the workspace root), or
/// `None` if Appendix A has no place for it.
pub fn classify(dir: &Path) -> Option<Tier> {
    let parts: Vec<&str> = dir
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    match parts.as_slice() {
        ["crates", "types" | "statemachine"] => Some(Tier::Domain),
        ["crates", "node" | "journal" | "raft_adapter"] => Some(Tier::Engine),
        ["crates", "fs_storage"] => Some(Tier::Adapter),
        ["crates", "server"] => Some(Tier::ProductionRoot),
        ["crates", "sim"] => Some(Tier::SimulationRoot),
        ["crates", "model" | "checker"] => Some(Tier::Oracle),
        ["crates", "cli"] => Some(Tier::Cli),
        ["conformance"] => Some(Tier::Conformance),
        ["xtask" | "fuzz"] => Some(Tier::Tooling),
        ["xtask", "probes", _] => Some(Tier::Probe),
        _ => None,
    }
}

/// Whether `path` (absolute) is vendored third-party source under `<root>/vendor/`.
fn is_vendored(path: &Path, root: &Path) -> bool {
    path.starts_with(root.join("vendor"))
}

// check: DEP-RULE
/// Checks every dependency edge of every workspace member against the tier rules; one
/// case per member plus one per dependency edge.
pub fn check_dependency_rule(root: &Path) -> Result<StaticOutcome, String> {
    let metadata = metadata::load(root)?;
    let ledger = ledger::load(root)?;
    Ok(dependency_rule(&metadata, &ledger))
}

/// The dependency-rule verdict for already-loaded metadata and ledger.
pub fn dependency_rule(metadata: &Metadata, ledger: &Ledger) -> StaticOutcome {
    let root = &metadata.workspace_root;
    let members = metadata.members();
    let mut cases = 0;
    let mut violations = Vec::new();
    for package in &members {
        cases += 1;
        let Some(tier) = tier_of(package, root) else {
            violations.push(format!(
                "{}: crate directory {} has no tier in Appendix A; add it to xtask/src/tiers.rs \
                 only with a plan change",
                package.name,
                relative(package.dir(), root).display()
            ));
            continue;
        };
        for dep in &package.dependencies {
            cases += 1;
            if let Some(path) = &dep.path {
                if is_vendored(path, root) {
                    if !matches!(tier, Tier::Engine) {
                        violations.push(format!(
                            "{} ({}) depends on vendored `{}`; only engine crates may use the \
                             vendored Raft library, and no Raft type crosses the Consensus \
                             port (CD-1)",
                            package.name,
                            tier.name(),
                            dep.name
                        ));
                    }
                    continue;
                }
                match classify(relative(path, root)) {
                    Some(to) if tier.may_depend_on(to) => {}
                    Some(to) => violations.push(format!(
                        "{} ({}) depends on `{}` ({}); dependencies point inward only (CD-1)",
                        package.name,
                        tier.name(),
                        dep.name,
                        to.name()
                    )),
                    None => violations.push(format!(
                        "{} depends on path crate `{}` at {}, which has no tier in Appendix A",
                        package.name,
                        dep.name,
                        path.display()
                    )),
                }
            } else if tier.is_deterministic() || matches!(tier, Tier::Probe) {
                let scope = ledger.get(&dep.name).map(|row| row.scope.as_str());
                if scope != Some("deterministic") {
                    violations.push(format!(
                        "{} ({}) depends on third-party `{}`, whose DEPENDENCIES.md scope is \
                         {scope:?}; deterministic crates use only std, the vendored Raft \
                         library and crates ledgered as `deterministic` (CD-12, P4)",
                        package.name,
                        tier.name(),
                        dep.name
                    ));
                }
            }
        }
    }
    StaticOutcome { cases, violations }
}

fn tier_of(package: &Package, root: &Path) -> Option<Tier> {
    classify(relative(package.dir(), root))
}

fn relative<'a>(path: &'a Path, root: &Path) -> &'a Path {
    path.strip_prefix(root).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    // check: DEP-RULE-FIXTURES
    // The dependency rule's own fixtures: each forbidden edge (a deterministic crate on an
    // adapter, an oracle on production code, …) is rejected (CD-1, T-12; W04 acceptance).

    use super::*;
    use crate::metadata::{Dependency, Target};
    use std::path::PathBuf;

    fn package(dir: &str, deps: Vec<Dependency>) -> Package {
        Package {
            name: dir.rsplit('/').next().unwrap().to_owned(),
            id: dir.to_owned(),
            manifest_path: PathBuf::from(format!("/w/{dir}/Cargo.toml")),
            targets: vec![Target {
                kind: vec!["lib".to_owned()],
                src_path: PathBuf::from(format!("/w/{dir}/src/lib.rs")),
            }],
            dependencies: deps,
        }
    }

    fn path_dep(dir: &str) -> Dependency {
        Dependency {
            name: dir.rsplit('/').next().unwrap().to_owned(),
            path: Some(PathBuf::from(format!("/w/{dir}"))),
        }
    }

    fn registry_dep(name: &str) -> Dependency {
        Dependency {
            name: name.to_owned(),
            path: None,
        }
    }

    fn metadata(packages: Vec<Package>) -> Metadata {
        Metadata {
            workspace_members: packages.iter().map(|p| p.id.clone()).collect(),
            packages,
            workspace_root: PathBuf::from("/w"),
        }
    }

    fn ledger() -> Ledger {
        ledger::parse(
            "| Crate | Version | Scope | Used by | Purpose | Licence | Source | Determinism |\n\
             |---|---|---|---|---|---|---|---|\n\
             | `serde` | 1.0.0 | tools | `xtask` | p | MIT | crates.io | n/a |\n\
             | `pure_lib` | 1.0.0 | deterministic | `types` | p | MIT | crates.io | pure: no I/O |\n",
        )
        .unwrap()
    }

    fn violations(packages: Vec<Package>) -> Vec<String> {
        dependency_rule(&metadata(packages), &ledger()).violations
    }

    #[test]
    fn appendix_a_directories_classify() {
        let cases = [
            ("crates/types", Some(Tier::Domain)),
            ("crates/raft_adapter", Some(Tier::Engine)),
            ("crates/fs_storage", Some(Tier::Adapter)),
            ("crates/server", Some(Tier::ProductionRoot)),
            ("crates/sim", Some(Tier::SimulationRoot)),
            ("crates/checker", Some(Tier::Oracle)),
            ("conformance", Some(Tier::Conformance)),
            ("crates/cli", Some(Tier::Cli)),
            ("xtask", Some(Tier::Tooling)),
            ("xtask/probes/production", Some(Tier::Probe)),
            ("crates/utils", None),
        ];
        for (dir, expected) in cases {
            assert_eq!(classify(Path::new(dir)), expected, "dir={dir}");
        }
    }

    #[test]
    fn a_deterministic_crate_depending_on_an_adapter_is_rejected() {
        let found = violations(vec![
            package("crates/journal", vec![path_dep("crates/fs_storage")]),
            package("crates/fs_storage", vec![path_dep("crates/types")]),
        ]);
        assert_eq!(found.len(), 1, "found={found:#?}");
        assert!(
            found[0].contains("journal (engine) depends on `fs_storage` (adapter)"),
            "rule: W04 acceptance (dependency rule fails on deterministic -> adapter); \
             found={found:#?}"
        );
    }

    #[test]
    fn oracles_and_conformance_depend_on_no_production_crate() {
        let found = violations(vec![
            package("crates/model", vec![path_dep("crates/types")]),
            package("crates/checker", vec![path_dep("crates/model")]),
            package("conformance", vec![path_dep("crates/server")]),
        ]);
        assert_eq!(
            found.len(),
            2,
            "rule: T-12 oracle independence; found={found:#?}"
        );
        assert!(
            found
                .iter()
                .any(|v| v.contains("model (oracle) depends on `types`"))
        );
        assert!(
            found
                .iter()
                .any(|v| v.contains("conformance (conformance) depends on `server`")),
            "rule: the conformance suite never depends on a production crate; found={found:#?}"
        );
    }

    #[test]
    fn conformance_may_use_the_oracle_crates() {
        let found = violations(vec![
            package("crates/model", Vec::new()),
            package("crates/checker", vec![path_dep("crates/model")]),
            package(
                "conformance",
                vec![path_dep("crates/model"), path_dep("crates/checker")],
            ),
        ]);
        assert_eq!(
            found,
            Vec::<String>::new(),
            "rule: PLAN.md §10.8 lets the conformance suite use the reference model and the \
             K-10 witness from the oracle crates (br-ucsp)"
        );
    }

    #[test]
    fn the_server_may_not_carry_test_only_crates() {
        let found = violations(vec![package(
            "crates/server",
            vec![path_dep("crates/sim"), path_dep("crates/checker")],
        )]);
        assert_eq!(found.len(), 2, "found={found:#?}");
    }

    #[test]
    fn inward_dependencies_are_allowed() {
        let found = violations(vec![
            package("crates/statemachine", vec![path_dep("crates/types")]),
            package(
                "crates/node",
                vec![path_dep("crates/statemachine"), path_dep("crates/journal")],
            ),
            package(
                "crates/sim",
                vec![path_dep("crates/node"), path_dep("crates/checker")],
            ),
            package("crates/cli", vec![path_dep("crates/journal")]),
            package("xtask", vec![registry_dep("serde")]),
        ]);
        assert_eq!(found, Vec::<String>::new());
    }

    #[test]
    fn deterministic_crates_use_only_deterministic_scope_third_party_crates() {
        let found = violations(vec![package(
            "crates/types",
            vec![registry_dep("pure_lib"), registry_dep("serde")],
        )]);
        assert_eq!(found.len(), 1, "found={found:#?}");
        assert!(
            found[0].contains("`serde`") && found[0].contains("\"tools\""),
            "rule: any third-party dependency of a deterministic crate (dev ones included) \
             needs deterministic scope; found={found:#?}"
        );
    }

    #[test]
    fn only_engine_crates_may_use_the_vendored_raft_library() {
        let found = violations(vec![
            package("crates/raft_adapter", vec![path_dep("vendor/raft")]),
            package("crates/types", vec![path_dep("vendor/raft")]),
        ]);
        assert_eq!(found.len(), 1, "found={found:#?}");
        assert!(found[0].starts_with("types (domain)"), "found={found:#?}");
    }

    #[test]
    fn an_unclassified_crate_is_rejected() {
        let found = violations(vec![package("crates/utils", vec![])]);
        assert!(
            found.len() == 1 && found[0].contains("no tier in Appendix A"),
            "found={found:#?}"
        );
    }

    #[test]
    fn the_real_workspace_satisfies_the_dependency_rule() {
        let root = crate::test_support::workspace_root();
        let outcome = check_dependency_rule(&root).unwrap();
        assert!(outcome.cases >= 4, "cases={}", outcome.cases);
        assert_eq!(outcome.violations, Vec::<String>::new());
    }
}
