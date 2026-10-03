//! The dependency ledger, `DEPENDENCIES.md` (PLAN.md CD-12), and the DEP-LEDGER check:
//! every direct third-party dependency of every workspace crate is listed with its
//! purpose, scope, locked version, licence and source.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use crate::check::StaticOutcome;
use crate::metadata::{self, Metadata};

/// Ledger file, relative to the workspace root.
pub const LEDGER_FILE: &str = "DEPENDENCIES.md";

/// The scopes a ledger row may declare.
pub const SCOPES: &[&str] = &["deterministic", "adapter", "oracle", "tools", "dev"];

const COLUMNS: [&str; 8] = [
    "Crate",
    "Version",
    "Scope",
    "Used by",
    "Purpose",
    "Licence",
    "Source",
    "Determinism",
];

/// One row of the ledger table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Crate name.
    pub name: String,
    /// The locked version.
    pub version: String,
    /// One of [`SCOPES`].
    pub scope: String,
    /// Workspace crates that depend on it directly.
    pub used_by: BTreeSet<String>,
    /// 1-based line in the ledger, for messages.
    pub line: usize,
}

/// The parsed ledger, keyed by crate name.
#[derive(Debug, Default)]
pub struct Ledger {
    rows: BTreeMap<String, Row>,
}

impl Ledger {
    /// The row for `name`, if listed.
    pub fn get(&self, name: &str) -> Option<&Row> {
        self.rows.get(name)
    }
}

/// Reads and parses `<root>/DEPENDENCIES.md`.
pub fn load(root: &Path) -> Result<Ledger, String> {
    let text = fs::read_to_string(root.join(LEDGER_FILE))
        .map_err(|err| format!("cannot read {LEDGER_FILE}: {err}"))?;
    parse(&text)
}

/// Parses the ledger table: the first markdown table whose header is [`COLUMNS`].
pub fn parse(text: &str) -> Result<Ledger, String> {
    let mut ledger = Ledger::default();
    let mut in_table = false;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        let cells = cells(line);
        if !in_table {
            in_table = cells == COLUMNS;
            continue;
        }
        if cells.is_empty() {
            break;
        }
        if cells
            .iter()
            .all(|c| c.chars().all(|ch| ch == '-' || ch == ':'))
        {
            continue;
        }
        let [
            name,
            version,
            scope,
            used_by,
            purpose,
            licence,
            source,
            determinism,
        ] = <[&str; 8]>::try_from(cells.as_slice()).map_err(|_| {
            format!(
                "{LEDGER_FILE}:{number}: expected 8 columns, found {}",
                cells.len()
            )
        })?;
        let name = name.trim_matches('`').to_owned();
        for (column, value) in [
            ("Crate", name.as_str()),
            ("Version", version),
            ("Purpose", purpose),
            ("Licence", licence),
            ("Source", source),
            ("Determinism", determinism),
        ] {
            if value.is_empty() {
                return Err(format!(
                    "{LEDGER_FILE}:{number}: empty {column} for `{name}`"
                ));
            }
        }
        if !SCOPES.contains(&scope) {
            return Err(format!(
                "{LEDGER_FILE}:{number}: scope {scope:?} for `{name}` is not one of {SCOPES:?}"
            ));
        }
        let row = Row {
            name: name.clone(),
            version: version.to_owned(),
            scope: scope.to_owned(),
            used_by: used_by
                .split(',')
                .map(|u| u.trim().trim_matches('`').to_owned())
                .filter(|u| !u.is_empty())
                .collect(),
            line: number,
        };
        if ledger.rows.insert(name.clone(), row).is_some() {
            return Err(format!("{LEDGER_FILE}:{number}: `{name}` is listed twice"));
        }
    }
    if in_table {
        Ok(ledger)
    } else {
        Err(format!(
            "{LEDGER_FILE}: no table with the header {COLUMNS:?}"
        ))
    }
}

/// The trimmed cells of a markdown table row; empty if `line` is not a table row.
fn cells(line: &str) -> Vec<&str> {
    let trimmed = line.trim();
    let Some(inner) = trimmed.strip_prefix('|').and_then(|l| l.strip_suffix('|')) else {
        return Vec::new();
    };
    inner.split('|').map(str::trim).collect()
}

/// Locked versions per crate name, from `Cargo.lock`.
pub fn locked_versions(lockfile: &str) -> Result<BTreeMap<String, BTreeSet<String>>, String> {
    let table: toml::Table =
        toml::from_str(lockfile).map_err(|err| format!("invalid Cargo.lock: {}", err.message()))?;
    let mut versions: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for package in table
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(name), Some(version)) = (
            package.get("name").and_then(toml::Value::as_str),
            package.get("version").and_then(toml::Value::as_str),
        ) {
            versions
                .entry(name.to_owned())
                .or_default()
                .insert(version.to_owned());
        }
    }
    Ok(versions)
}

// check: DEP-LEDGER
/// Checks the ledger against the workspace's direct dependencies and the lockfile; one
/// case per (crate, third-party dependency) edge plus one per ledger row.
pub fn check_ledger(root: &Path) -> Result<StaticOutcome, String> {
    let metadata = metadata::load(root)?;
    let ledger = load(root)?;
    let lockfile = fs::read_to_string(root.join("Cargo.lock"))
        .map_err(|err| format!("cannot read Cargo.lock: {err}"))?;
    Ok(ledger_verdict(
        &metadata,
        &ledger,
        &locked_versions(&lockfile)?,
    ))
}

/// The ledger verdict for already-loaded inputs.
pub fn ledger_verdict(
    metadata: &Metadata,
    ledger: &Ledger,
    locked: &BTreeMap<String, BTreeSet<String>>,
) -> StaticOutcome {
    let members = metadata.members();
    let member_dirs: Vec<&Path> = members.iter().map(|p| p.dir()).collect();
    let mut users: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut cases = 0;
    let mut violations = Vec::new();
    for package in &members {
        for dep in &package.dependencies {
            // First-party path dependencies (workspace members) are not ledgered.
            let first_party = dep
                .path
                .as_deref()
                .is_some_and(|path| member_dirs.contains(&path));
            if first_party {
                continue;
            }
            cases += 1;
            users.entry(&dep.name).or_default().insert(&package.name);
            match ledger.get(&dep.name) {
                None => violations.push(format!(
                    "{} depends on `{}`, which is not listed in {LEDGER_FILE} (CD-12)",
                    package.name, dep.name
                )),
                Some(row) if !row.used_by.contains(&package.name) => violations.push(format!(
                    "{LEDGER_FILE}:{}: `{}` is used by {} but its Used by column does not \
                     list it",
                    row.line, dep.name, package.name
                )),
                Some(_) => {}
            }
        }
    }
    for row in ledger.rows.values() {
        cases += 1;
        let Some(actual_users) = users.get(row.name.as_str()) else {
            violations.push(format!(
                "{LEDGER_FILE}:{}: `{}` is listed but no workspace crate depends on it directly; \
                 remove the stale row",
                row.line, row.name
            ));
            continue;
        };
        for user in &row.used_by {
            if !actual_users.contains(user.as_str()) {
                violations.push(format!(
                    "{LEDGER_FILE}:{}: `{}` lists {user} in Used by, but {user} does not \
                     depend on it",
                    row.line, row.name
                ));
            }
        }
        match locked.get(&row.name) {
            Some(versions) if versions.contains(&row.version) => {}
            Some(versions) => violations.push(format!(
                "{LEDGER_FILE}:{}: `{}` is ledgered at {} but Cargo.lock has {versions:?}; \
                 update the row in the commit that changes the version",
                row.line, row.name, row.version
            )),
            None => violations.push(format!(
                "{LEDGER_FILE}:{}: `{}` is not in Cargo.lock",
                row.line, row.name
            )),
        }
    }
    StaticOutcome { cases, violations }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{Dependency, Package, Target};
    use std::path::PathBuf;

    const HEADER: &str = "| Crate | Version | Scope | Used by | Purpose | Licence | Source | Determinism |\n\
                          |---|---|---|---|---|---|---|---|\n";

    fn ledger_with(rows: &str) -> Ledger {
        parse(&format!("# Ledger\n\n{HEADER}{rows}\nTrailing prose.\n")).unwrap()
    }

    fn workspace(deps: &[(&str, &str)]) -> Metadata {
        let mut packages: Vec<Package> = Vec::new();
        for (user, dep) in deps {
            let dir = format!("/w/{user}");
            if !packages.iter().any(|p| p.name == *user) {
                packages.push(Package {
                    name: (*user).to_owned(),
                    id: (*user).to_owned(),
                    manifest_path: PathBuf::from(format!("{dir}/Cargo.toml")),
                    targets: vec![Target {
                        kind: vec!["lib".to_owned()],
                        src_path: PathBuf::from(format!("{dir}/src/lib.rs")),
                    }],
                    dependencies: Vec::new(),
                });
            }
            let package = packages.iter_mut().find(|p| p.name == *user).unwrap();
            let path = dep
                .strip_prefix("path:")
                .map(|d| PathBuf::from(format!("/w/{d}")));
            package.dependencies.push(Dependency {
                name: dep.trim_start_matches("path:").to_owned(),
                path,
            });
        }
        Metadata {
            workspace_members: packages.iter().map(|p| p.id.clone()).collect(),
            packages,
            workspace_root: PathBuf::from("/w"),
        }
    }

    fn locked(entries: &[(&str, &str)]) -> BTreeMap<String, BTreeSet<String>> {
        let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (name, version) in entries {
            map.entry((*name).to_owned())
                .or_default()
                .insert((*version).to_owned());
        }
        map
    }

    #[test]
    fn parse_reads_rows_and_ignores_surrounding_prose() {
        let ledger = ledger_with(
            "| `serde` | 1.0.229 | tools | `xtask`, `cli` | p | MIT OR Apache-2.0 | crates.io | n/a |\n",
        );
        let row = ledger.get("serde").unwrap();
        assert_eq!(row.version, "1.0.229");
        assert_eq!(row.scope, "tools");
        assert_eq!(
            row.used_by,
            ["cli", "xtask"]
                .into_iter()
                .map(str::to_owned)
                .collect::<BTreeSet<_>>()
        );
        assert_eq!(row.line, 5, "row={row:?}");
    }

    #[test]
    fn parse_rejects_malformed_rows() {
        let cases = [
            (
                "| `a` | 1 | tools | `x` | p | MIT | crates.io |\n",
                "expected 8 columns",
            ),
            (
                "| `a` | 1 | everywhere | `x` | p | MIT | crates.io | n/a |\n",
                "scope",
            ),
            (
                "| `a` | 1 | tools | `x` |  | MIT | crates.io | n/a |\n",
                "empty Purpose",
            ),
            (
                "| `a` | 1 | tools | `x` | p | MIT | crates.io | n/a |\n\
                 | `a` | 1 | tools | `x` | p | MIT | crates.io | n/a |\n",
                "listed twice",
            ),
        ];
        for (rows, expected) in cases {
            let observed = parse(&format!("{HEADER}{rows}"));
            assert!(
                observed.as_ref().is_err_and(|e| e.contains(expected)),
                "rows={rows:?} expected error containing {expected:?}; observed={observed:?}"
            );
        }
        assert!(parse("no table here\n").is_err());
    }

    #[test]
    fn an_unlisted_dependency_fails() {
        let outcome = ledger_verdict(
            &workspace(&[("xtask", "serde"), ("xtask", "regex")]),
            &ledger_with("| `serde` | 1.0.0 | tools | `xtask` | p | MIT | crates.io | n/a |\n"),
            &locked(&[("serde", "1.0.0"), ("regex", "1.2.0")]),
        );
        assert_eq!(outcome.violations.len(), 1, "observed={outcome:#?}");
        assert!(
            outcome.violations[0].contains("`regex`, which is not listed"),
            "rule: direct dependencies are a subset of the ledger (CD-12); observed={outcome:#?}"
        );
    }

    #[test]
    fn used_by_staleness_and_versions_are_checked() {
        let outcome = ledger_verdict(
            &workspace(&[("xtask", "serde"), ("cli", "serde")]),
            &ledger_with(
                "| `serde` | 1.0.0 | tools | `xtask`, `server` | p | MIT | crates.io | n/a |\n\
                 | `old` | 0.1.0 | tools | `xtask` | p | MIT | crates.io | n/a |\n",
            ),
            &locked(&[("serde", "1.0.1")]),
        );
        let expected = [
            "is used by cli but its Used by column does not list it",
            "lists server in Used by, but server does not depend on it",
            "ledgered at 1.0.0 but Cargo.lock has",
            "`old` is listed but no workspace crate depends on it",
        ];
        for fragment in expected {
            assert!(
                outcome.violations.iter().any(|v| v.contains(fragment)),
                "expected a violation containing {fragment:?}; observed={:#?}",
                outcome.violations
            );
        }
        assert_eq!(
            outcome.violations.len(),
            expected.len(),
            "observed={outcome:#?}"
        );
    }

    #[test]
    fn first_party_path_dependencies_are_not_ledgered() {
        let outcome = ledger_verdict(
            &workspace(&[
                ("xtask", "serde"),
                ("probe", "path:types"),
                ("types", "path:xtask"),
            ]),
            &ledger_with("| `serde` | 1.0.0 | tools | `xtask` | p | MIT | crates.io | n/a |\n"),
            &locked(&[("serde", "1.0.0")]),
        );
        assert_eq!(outcome.violations, Vec::<String>::new());
    }

    #[test]
    fn locked_versions_reads_cargo_lock() {
        let lock = "version = 4\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.229\"\n\n\
                    [[package]]\nname = \"serde\"\nversion = \"0.9.0\"\n";
        let versions = locked_versions(lock).unwrap();
        assert_eq!(versions["serde"].len(), 2, "observed={versions:?}");
    }

    #[test]
    fn the_real_ledger_matches_the_workspace() {
        let root = crate::test_support::workspace_root();
        let outcome = check_ledger(&root).unwrap();
        assert!(outcome.cases >= 5, "cases={}", outcome.cases);
        assert_eq!(outcome.violations, Vec::<String>::new());
    }
}
