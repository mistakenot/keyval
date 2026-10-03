//! RAFT-VENDOR: the vendored Raft library (`vendor/raft/`) is exactly what its provenance
//! manifest records, and raft-proto's code generation stays pinned (PLAN.md §7.3, P4,
//! T-3; W02.1, br-gqxq).
//!
//! `vendor/raft/VENDOR.toml` lists the sha256 of every vendored file. Code generation is
//! pinned by vendoring its output: raft-proto declares `build = false`, so no protoc from
//! `PATH` can vary the build. The check fails on a missing, changed or unlisted file; on
//! a generated output the manifest does not list; on any route back to build-time code
//! generation (a build script, build-dependencies, a protobuf-build or prost feature, the
//! published raft-proto instead of the vendored one); and on a lockfile that holds a code
//! generator or a protobuf runtime other than the generator's.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::check::StaticOutcome;

/// The vendored library, relative to the workspace root.
pub const DIR: &str = "vendor/raft";

/// Its provenance manifest, relative to [`DIR`].
pub const MANIFEST: &str = "VENDOR.toml";

/// The manifest schema this check understands.
pub const MANIFEST_SCHEMA: &str = "keyval.vendor.raft/v1";

/// Packages that generate code at build time; none may appear in `Cargo.lock`.
const CODEGEN_PACKAGES: &[&str] = &["protobuf-build", "protobuf-codegen"];

#[derive(Debug, Deserialize)]
struct Manifest {
    schema: String,
    codegen: Codegen,
    #[serde(rename = "crate")]
    crates: Vec<VendoredCrate>,
}

#[derive(Debug, Deserialize)]
struct Codegen {
    #[serde(rename = "crate")]
    krate: String,
    outputs: Vec<String>,
    protoc: String,
    protoc_sha256: String,
    protobuf_runtime: String,
}

#[derive(Debug, Deserialize)]
struct VendoredCrate {
    name: String,
    dir: String,
    files: BTreeMap<String, String>,
}

/// What one verification of `vendor/raft/` found.
#[derive(Debug)]
struct Verification {
    /// Assertions made: one per listed file plus one per structural rule.
    cases: usize,
    /// Listed files whose content matched their recorded hash.
    files_verified: usize,
    /// The pinned protoc as the manifest records it.
    protoc: String,
    violations: Vec<String>,
}

// check: RAFT-VENDOR
/// Verifies `<root>/vendor/raft` against its manifest and the codegen pin.
pub fn check_vendor(root: &Path) -> Result<StaticOutcome, String> {
    let found = verify(root)?;
    Ok(StaticOutcome {
        cases: found.cases,
        violations: found.violations,
    })
}

/// `./dev doctor`'s verdict on the pinned Raft code generation (§12.2 W02).
#[derive(Debug, Serialize)]
pub struct CodegenPin {
    /// True when the vendored tree matches its manifest and the pin holds.
    pub ok: bool,
    /// The pinned protoc as `VENDOR.toml` records it, with its sha256.
    pub protoc: Option<String>,
    /// Vendored files whose content matched the manifest.
    pub files_verified: usize,
    /// What is wrong, one message per violation.
    pub violations: Vec<String>,
}

/// Runs the RAFT-VENDOR assertions for `./dev doctor`.
pub fn codegen_pin(root: &Path) -> CodegenPin {
    match verify(root) {
        Ok(found) => CodegenPin {
            ok: found.violations.is_empty() && found.files_verified > 0,
            protoc: Some(found.protoc),
            files_verified: found.files_verified,
            violations: found.violations,
        },
        Err(why) => CodegenPin {
            ok: false,
            protoc: None,
            files_verified: 0,
            violations: vec![why],
        },
    }
}

fn verify(root: &Path) -> Result<Verification, String> {
    let dir = root.join(DIR);
    let manifest_path = format!("{DIR}/{MANIFEST}");
    let text = fs::read_to_string(dir.join(MANIFEST))
        .map_err(|err| format!("cannot read {manifest_path}: {err}"))?;
    let manifest: Manifest = toml::from_str(&text)
        .map_err(|err| format!("invalid {manifest_path}: {}", err.message()))?;
    let mut found = Verification {
        cases: 1,
        files_verified: 0,
        protoc: format!(
            "{} (sha256 {})",
            manifest.codegen.protoc, manifest.codegen.protoc_sha256
        ),
        violations: Vec::new(),
    };
    if manifest.schema != MANIFEST_SCHEMA {
        found.violations.push(format!(
            "{manifest_path}: schema {:?}, expected {MANIFEST_SCHEMA:?}",
            manifest.schema
        ));
    }
    check_files(&dir, &manifest, &mut found)?;
    check_codegen_crate(&dir, &manifest, &mut found)?;
    check_lockfile(root, &manifest.codegen.protobuf_runtime, &mut found)?;
    Ok(found)
}

/// Every listed file exists with its recorded hash; no file is unlisted; every codegen
/// output is a listed file of the codegen crate.
fn check_files(dir: &Path, manifest: &Manifest, found: &mut Verification) -> Result<(), String> {
    let mut listed = BTreeSet::new();
    for krate in &manifest.crates {
        for (file, recorded) in &krate.files {
            let path = format!("{}/{file}", krate.dir);
            found.cases += 1;
            match fs::read(dir.join(&path)) {
                Ok(bytes) if sha256_hex(&bytes) == *recorded => found.files_verified += 1,
                Ok(_) => found.violations.push(format!(
                    "{DIR}/{path}: content differs from the sha256 recorded in {MANIFEST}"
                )),
                Err(err) => found.violations.push(format!(
                    "{DIR}/{path}: listed in {MANIFEST} but unreadable: {err}"
                )),
            }
            listed.insert(path);
        }
    }
    found.cases += 1;
    let mut on_disk = BTreeSet::new();
    walk(dir, "", &mut on_disk)?;
    on_disk.remove(MANIFEST);
    for extra in on_disk.difference(&listed) {
        found.violations.push(format!(
            "{DIR}/{extra}: not listed in {MANIFEST}; every vendored file is recorded"
        ));
    }
    let codegen_dir = manifest
        .crates
        .iter()
        .find(|krate| krate.name == manifest.codegen.krate)
        .map(|krate| krate.dir.as_str());
    for output in &manifest.codegen.outputs {
        found.cases += 1;
        let listed_output =
            codegen_dir.is_some_and(|dir| listed.contains(&format!("{dir}/{output}")));
        if !listed_output {
            found.violations.push(format!(
                "codegen output {output} of {} is not a listed vendored file",
                manifest.codegen.krate
            ));
        }
    }
    Ok(())
}

/// The codegen crate cannot generate code at build time, pins the generator's protobuf
/// runtime, and every other vendored crate that uses it resolves the vendored copy.
fn check_codegen_crate(
    dir: &Path,
    manifest: &Manifest,
    found: &mut Verification,
) -> Result<(), String> {
    let name = &manifest.codegen.krate;
    found.cases += 1;
    let Some(codegen) = manifest.crates.iter().find(|krate| &krate.name == name) else {
        found.violations.push(format!(
            "[codegen] names {name}, which no [[crate]] vendors"
        ));
        return Ok(());
    };
    let cargo = read_manifest(dir, &codegen.dir)?;
    let package = cargo.get("package").and_then(toml::Value::as_table);
    if package.and_then(|p| p.get("build")) != Some(&toml::Value::Boolean(false)) {
        found.violations.push(format!(
            "{DIR}/{}/Cargo.toml: `build = false` is required, so no build script can run \
             protoc from PATH (P4)",
            codegen.dir
        ));
    }
    found.cases += 1;
    if cargo.contains_key("build-dependencies") {
        found.violations.push(format!(
            "{DIR}/{}/Cargo.toml: build-dependencies reintroduce build-time code generation",
            codegen.dir
        ));
    }
    found.cases += 1;
    let runtime = format!("={}", manifest.codegen.protobuf_runtime);
    let protobuf = dependency(&cargo, "protobuf").and_then(|dep| match dep {
        toml::Value::String(version) => Some(version.as_str()),
        toml::Value::Table(table) => table.get("version").and_then(toml::Value::as_str),
        _ => None,
    });
    if protobuf != Some(runtime.as_str()) {
        found.violations.push(format!(
            "{DIR}/{}/Cargo.toml: protobuf is {protobuf:?}; generated code needs exactly the \
             generator's runtime {runtime:?}",
            codegen.dir
        ));
    }
    for krate in &manifest.crates {
        let cargo = read_manifest(dir, &krate.dir)?;
        found.cases += 1;
        for feature in generating_features(&cargo) {
            found.violations.push(format!(
                "{DIR}/{}/Cargo.toml: feature {feature:?} needs build-time code generation",
                krate.dir
            ));
        }
        if krate.name == *name {
            continue;
        }
        if let Some(dep) = dependency(&cargo, name) {
            found.cases += 1;
            let expected = format!("../{}", codegen.dir);
            let path = dep.get("path").and_then(toml::Value::as_str);
            if path != Some(expected.as_str()) {
                found.violations.push(format!(
                    "{DIR}/{}/Cargo.toml: {name} must resolve to the vendored copy \
                     (path = {expected:?}); the published crate runs protoc at build time",
                    krate.dir
                ));
            }
        }
    }
    Ok(())
}

/// The workspace lockfile holds no code generator, and any protobuf runtime in it is the
/// generator's.
fn check_lockfile(root: &Path, runtime: &str, found: &mut Verification) -> Result<(), String> {
    found.cases += 1;
    let text = fs::read_to_string(root.join("Cargo.lock"))
        .map_err(|err| format!("cannot read Cargo.lock: {err}"))?;
    let lock: toml::Table =
        toml::from_str(&text).map_err(|err| format!("invalid Cargo.lock: {}", err.message()))?;
    for package in lock
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = package.get("name").and_then(toml::Value::as_str);
        let version = package.get("version").and_then(toml::Value::as_str);
        match (name, version) {
            (Some(name), _) if CODEGEN_PACKAGES.contains(&name) => found.violations.push(format!(
                "Cargo.lock: {name} generates code at build time; the generated code is vendored"
            )),
            (Some("protobuf"), Some(version)) if version != runtime => found.violations.push(
                format!("Cargo.lock: protobuf {version}; the vendored code needs {runtime}"),
            ),
            _ => {}
        }
    }
    Ok(())
}

/// Features that name, or enable, prost or protobuf-build: both generate code at build time.
fn generating_features(cargo: &toml::Table) -> Vec<&str> {
    cargo
        .get("features")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flatten()
        .filter(|(feature, members)| {
            std::iter::once(feature.as_str())
                .chain(
                    members
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(toml::Value::as_str),
                )
                .any(|item| item.contains("prost") || item.contains("protobuf-build"))
        })
        .map(|(feature, _)| feature.as_str())
        .collect()
}

fn read_manifest(dir: &Path, crate_dir: &str) -> Result<toml::Table, String> {
    let path = format!("{DIR}/{crate_dir}/Cargo.toml");
    let text = fs::read_to_string(dir.join(crate_dir).join("Cargo.toml"))
        .map_err(|err| format!("cannot read {path}: {err}"))?;
    toml::from_str(&text).map_err(|err| format!("invalid {path}: {}", err.message()))
}

fn dependency<'a>(cargo: &'a toml::Table, name: &str) -> Option<&'a toml::Value> {
    cargo
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .and_then(|deps| deps.get(name))
}

/// Every file under `dir`, as `/`-separated paths relative to it.
fn walk(dir: &Path, prefix: &str, out: &mut BTreeSet<String>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|err| format!("cannot list {}: {err}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("cannot list {}: {err}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry
            .file_type()
            .map_err(|err| format!("cannot stat {relative}: {err}"))?;
        if kind.is_dir() {
            walk(&entry.path(), &relative, out)?;
        } else {
            out.insert(relative);
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const PROTO_MANIFEST: &str = "[package]\nname = \"raft-proto\"\nversion = \"0.7.0\"\n\
        build = false\n\n[dependencies.protobuf]\nversion = \"=2.28.0\"\n\n\
        [features]\nprotobuf-codec = [\"bytes\", \"protobuf/bytes\"]\n";
    const RAFT_MANIFEST: &str = "[package]\nname = \"raft\"\nversion = \"0.7.0\"\n\n\
        [dependencies.raft-proto]\nversion = \"0.7.0\"\npath = \"../raft-proto\"\n\
        default-features = false\n\n[features]\nprotobuf-codec = [\"raft-proto/protobuf-codec\"]\n";
    const LOCK: &str = "version = 4\n\n[[package]]\nname = \"xtask\"\nversion = \"0.1.0\"\n";

    /// A minimal valid workspace: two vendored crates, one generated output, a manifest
    /// whose hashes match, and a lockfile.
    fn fixture() -> PathBuf {
        let root = crate::test_support::scratch_dir("raft-vendor");
        write(&root, "Cargo.lock", LOCK);
        write(&root, "vendor/raft/raft/Cargo.toml", RAFT_MANIFEST);
        write(&root, "vendor/raft/raft/src/lib.rs", "pub fn raft() {}\n");
        write(&root, "vendor/raft/raft-proto/Cargo.toml", PROTO_MANIFEST);
        write(&root, "vendor/raft/raft-proto/build.rs", "fn main() {}\n");
        write(
            &root,
            "vendor/raft/raft-proto/src/protos/eraftpb.rs",
            "// @generated\n",
        );
        reseal(&root);
        root
    }

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Rewrites VENDOR.toml so it records the fixture's current files.
    fn reseal(root: &Path) {
        let mut text = format!(
            "schema = \"{MANIFEST_SCHEMA}\"\n\n[codegen]\ncrate = \"raft-proto\"\n\
             outputs = [\"src/protos/eraftpb.rs\"]\nprotoc = \"libprotoc 3.9.0\"\n\
             protoc_sha256 = \"30c6\"\nprotobuf_runtime = \"2.28.0\"\n"
        );
        for (name, dir) in [("raft", "raft"), ("raft-proto", "raft-proto")] {
            let mut files = BTreeSet::new();
            walk(&root.join(DIR).join(dir), "", &mut files).unwrap();
            write!(
                text,
                "\n[[crate]]\nname = \"{name}\"\ndir = \"{dir}\"\n\n[crate.files]\n"
            )
            .unwrap();
            for file in files {
                let bytes = fs::read(root.join(DIR).join(dir).join(&file)).unwrap();
                writeln!(text, "\"{file}\" = \"{}\"", sha256_hex(&bytes)).unwrap();
            }
        }
        write(root, &format!("{DIR}/{MANIFEST}"), &text);
    }

    fn outcome(root: &Path) -> StaticOutcome {
        check_vendor(root).unwrap()
    }

    fn assert_violation(root: &Path, rule: &str, expected: &str) {
        let outcome = outcome(root);
        assert!(
            outcome.violations.iter().any(|v| v.contains(expected)),
            "rule: {rule}; expected a violation containing {expected:?}; outcome={outcome:#?}"
        );
    }

    #[test]
    fn a_faithful_fixture_passes_with_every_assertion_counted() {
        let root = fixture();
        let outcome = outcome(&root);
        assert_eq!(
            outcome.violations,
            Vec::<String>::new(),
            "outcome={outcome:#?}"
        );
        // schema 1 + 5 files + listing 1 + 1 output + codegen crate and `build` 1
        // + build-dependencies 1 + runtime 1 + 2 feature scans + 1 dependent path + lockfile 1
        assert_eq!(outcome.cases, 15, "outcome={outcome:#?}");
    }

    #[test]
    fn a_changed_file_fails() {
        let root = fixture();
        write(
            &root,
            "vendor/raft/raft/src/lib.rs",
            "pub fn raft() { /* patched */ }\n",
        );
        assert_violation(
            &root,
            "vendored files match VENDOR.toml byte for byte",
            "raft/src/lib.rs: content differs",
        );
    }

    #[test]
    fn a_missing_file_fails() {
        let root = fixture();
        fs::remove_file(root.join("vendor/raft/raft/src/lib.rs")).unwrap();
        assert_violation(&root, "every listed file exists", "raft/src/lib.rs: listed");
    }

    #[test]
    fn an_unlisted_file_fails() {
        let root = fixture();
        write(
            &root,
            "vendor/raft/raft-proto/src/protos/extra.rs",
            "// stray\n",
        );
        assert_violation(
            &root,
            "every vendored file is recorded",
            "raft-proto/src/protos/extra.rs: not listed",
        );
    }

    #[test]
    fn a_wrong_schema_fails() {
        let root = fixture();
        let path = root.join(DIR).join(MANIFEST);
        let text = fs::read_to_string(&path).unwrap().replace("/v1", "/v0");
        fs::write(path, text).unwrap();
        assert_violation(&root, "the manifest schema is understood", "schema");
    }

    #[test]
    fn an_unlisted_codegen_output_fails() {
        let root = fixture();
        let path = root.join(DIR).join(MANIFEST);
        let text = fs::read_to_string(&path).unwrap().replace(
            "outputs = [\"src/protos/eraftpb.rs\"]",
            "outputs = [\"src/protos/mod.rs\"]",
        );
        fs::write(path, text).unwrap();
        assert_violation(
            &root,
            "generated code is vendored and recorded",
            "codegen output src/protos/mod.rs",
        );
    }

    #[test]
    fn a_reenabled_build_script_fails() {
        let root = fixture();
        write(
            &root,
            "vendor/raft/raft-proto/Cargo.toml",
            &PROTO_MANIFEST.replace("build = false", "build = \"build.rs\""),
        );
        reseal(&root);
        assert_violation(
            &root,
            "no build script runs protoc",
            "`build = false` is required",
        );
    }

    #[test]
    fn build_dependencies_fail() {
        let root = fixture();
        let manifest =
            format!("{PROTO_MANIFEST}\n[build-dependencies.protobuf-build]\nversion = \"0.14\"\n");
        write(&root, "vendor/raft/raft-proto/Cargo.toml", &manifest);
        reseal(&root);
        assert_violation(&root, "no build-time generator", "build-dependencies");
    }

    #[test]
    fn a_prost_feature_fails() {
        let root = fixture();
        let manifest = RAFT_MANIFEST.replace(
            "[features]\n",
            "[features]\nprost-codec = [\"raft-proto/prost-codec\"]\n",
        );
        write(&root, "vendor/raft/raft/Cargo.toml", &manifest);
        reseal(&root);
        assert_violation(
            &root,
            "no feature needs code generation",
            "feature \"prost-codec\"",
        );
    }

    #[test]
    fn the_published_raft_proto_fails() {
        let root = fixture();
        write(
            &root,
            "vendor/raft/raft/Cargo.toml",
            &RAFT_MANIFEST.replace("path = \"../raft-proto\"\n", ""),
        );
        reseal(&root);
        assert_violation(
            &root,
            "raft resolves the vendored raft-proto",
            "raft-proto must resolve to the vendored copy",
        );
    }

    #[test]
    fn an_unpinned_protobuf_runtime_fails() {
        let root = fixture();
        write(
            &root,
            "vendor/raft/raft-proto/Cargo.toml",
            &PROTO_MANIFEST.replace("=2.28.0", "2"),
        );
        reseal(&root);
        assert_violation(
            &root,
            "the generator's runtime is pinned",
            "protobuf is Some(\"2\")",
        );
    }

    #[test]
    fn a_code_generator_in_the_lockfile_fails() {
        let root = fixture();
        let lock =
            format!("{LOCK}\n[[package]]\nname = \"protobuf-codegen\"\nversion = \"2.28.0\"\n");
        write(&root, "Cargo.lock", &lock);
        assert_violation(
            &root,
            "no generator is built",
            "Cargo.lock: protobuf-codegen",
        );
    }

    #[test]
    fn another_protobuf_runtime_in_the_lockfile_fails() {
        let root = fixture();
        let lock = format!("{LOCK}\n[[package]]\nname = \"protobuf\"\nversion = \"2.27.1\"\n");
        write(&root, "Cargo.lock", &lock);
        assert_violation(
            &root,
            "the runtime matches the generator",
            "protobuf 2.27.1",
        );
    }

    #[test]
    fn a_missing_manifest_is_an_error_and_a_failed_doctor_verdict() {
        let root = fixture();
        fs::remove_file(root.join(DIR).join(MANIFEST)).unwrap();
        let err = check_vendor(&root).unwrap_err();
        assert!(
            err.contains("cannot read vendor/raft/VENDOR.toml"),
            "err={err}"
        );
        let pin = codegen_pin(&root);
        assert!(
            !pin.ok,
            "rule: doctor fails without the manifest; pin={pin:#?}"
        );
        assert_eq!(pin.violations, [err]);
    }

    #[test]
    fn the_doctor_verdict_reports_the_pinned_protoc() {
        let root = fixture();
        let pin = codegen_pin(&root);
        assert!(pin.ok, "pin={pin:#?}");
        assert_eq!(pin.files_verified, 5, "pin={pin:#?}");
        assert_eq!(pin.protoc.as_deref(), Some("libprotoc 3.9.0 (sha256 30c6)"));
        write(&root, "vendor/raft/raft/src/lib.rs", "// patched\n");
        assert!(!codegen_pin(&root).ok, "rule: a changed file fails doctor");
    }

    #[test]
    fn the_real_vendored_raft_matches_its_manifest() {
        let root = crate::test_support::workspace_root();
        let outcome = check_vendor(&root).unwrap();
        assert_eq!(
            outcome.violations,
            Vec::<String>::new(),
            "outcome={outcome:#?}"
        );
        let floor = crate::check::CATALOGUE
            .iter()
            .find(|def| def.id == "RAFT-VENDOR")
            .map(|def| def.min_cases)
            .unwrap();
        assert!(
            outcome.cases >= usize::try_from(floor).unwrap(),
            "rule: the real tree meets RAFT-VENDOR's min_cases={floor}; cases={}",
            outcome.cases
        );
    }
}
