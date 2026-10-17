//! Black-box tests: the `./dev` binary's `--json` stdout matches the schema in
//! `schemas/dev/` that its `schema` field names (O-6; check DEV-SCHEMAS). Only commands
//! that are cheap and leave nothing behind run here; `src/schemas.rs` covers every
//! output through each command's own code.
#![cfg(test)]

mod schemas {
    use std::fs;
    use std::path::PathBuf;
    use std::process::{Command, Output};

    use jsonschema::{Draft, Registry};
    use serde_json::{Value, json};

    /// The checkout under test, located at run time: a compile-time path outlives its
    /// checkout (br-gate-shared-target-dir-dtqu).
    fn root() -> PathBuf {
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("..")
    }

    fn dev(args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(args)
            .env("KEYVAL_ROOT", root())
            .output()
            .unwrap_or_else(|err| panic!("cannot run xtask with args={args:?}: {err}"))
    }

    /// Where `instance` breaks the schema its `schema` field names, read from
    /// `schemas/dev/` with every schema registered under its `$id`.
    fn violations(instance: &Value) -> Vec<String> {
        let mut documents = Vec::new();
        for entry in fs::read_dir(root().join("schemas/dev")).unwrap() {
            let path = entry.unwrap().path();
            if path.to_string_lossy().ends_with(".schema.json") {
                let doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap())
                    .unwrap_or_else(|err| panic!("{} is not JSON: {err}", path.display()));
                documents.push(doc);
            }
        }
        let resources = documents
            .iter()
            .map(|doc| (doc["$id"].as_str().unwrap().to_owned(), doc.clone()));
        let registry = Registry::new()
            .draft(Draft::Draft202012)
            .extend(resources)
            .and_then(jsonschema::RegistryBuilder::prepare)
            .unwrap();
        let Some(schema) = documents
            .iter()
            .find(|doc| doc["title"] == instance["schema"])
        else {
            return vec![format!("no schema titled {}", instance["schema"])];
        };
        let validator = jsonschema::options()
            .with_draft(Draft::Draft202012)
            .with_registry(&registry)
            .should_validate_formats(true)
            .build(&json!({ "$ref": schema["$id"] }))
            .unwrap();
        validator
            .iter_errors(instance)
            .map(|err| format!("at {:?}: {err}", err.instance_path().as_str()))
            .collect()
    }

    /// Runs `./dev args`, expecting an exit code in `codes` and one JSON object on stdout
    /// with the identifier `id`, and asserts it matches that identifier's schema.
    fn assert_output_matches(args: &[&str], codes: &[i32], id: &str) -> Value {
        let out = dev(args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let describe = || {
            format!(
                "args={args:?} status={:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr)
            )
        };
        assert!(
            out.status.code().is_some_and(|code| codes.contains(&code)),
            "rule: exit codes are part of the ./dev contract; expected one of {codes:?}; {}",
            describe()
        );
        let instance: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|err| {
            panic!(
                "rule: --json prints one JSON object ({err}); {}",
                describe()
            )
        });
        assert_eq!(instance["schema"], json!(id), "{}", describe());
        let found = violations(&instance);
        assert!(
            found.is_empty(),
            "rule: the binary's JSON output matches its schema (O-6); violations={found:#?}; {}",
            describe()
        );
        instance
    }

    #[test]
    fn a_usage_error_prints_a_valid_error_envelope() {
        let envelope = assert_output_matches(&["--json", "nope"], &[2], "keyval.dev.error/v1");
        assert_eq!(envelope["error"]["kind"], "usage");
    }

    #[test]
    fn a_refused_tier_gate_run_prints_a_valid_error_envelope() {
        // T1 is implemented (W46.1). A tier judges only a run tree, so `--tree` from the
        // shared checkout is refused before anything runs; this checks the envelope without
        // starting a tier.
        let args = ["gate", "integration", "--tree", "not-a-tree", "--json"];
        let envelope = assert_output_matches(&args, &[1], "keyval.dev.error/v1");
        assert_eq!(envelope["error"]["kind"], "refused");
    }

    #[test]
    fn check_reports_are_valid_whether_they_pass_or_not() {
        let args = ["check", "AGENTS-RULES", "--json"];
        let passed = assert_output_matches(&args, &[0], "keyval.dev.check/v2");
        assert_eq!(passed["checks"][0]["status"], "passed");
        let args = ["check", "NOPE", "AGENTS-RULES", "--json"];
        let unknown = assert_output_matches(&args, &[1], "keyval.dev.check/v2");
        assert_eq!(unknown["checks"][0]["status"], "unknown");
    }

    #[test]
    fn the_doctor_report_is_valid() {
        // Doctor fails on a machine that lacks a tool; its report is valid either way.
        assert_output_matches(&["doctor", "--json"], &[0, 1], "keyval.dev.doctor/v2");
    }
}
