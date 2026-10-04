//! Running the binary, and checking its output against the schemas.
#![cfg(test)]

use std::path::Path;
use std::process::Command;

use serde_json::Value;

const INIT_SCHEMA: &str = include_str!("../../../../schemas/server/init.v1.schema.json");
const ERROR_SCHEMA: &str = include_str!("../../../../schemas/server/error.v1.schema.json");

/// What a run of the binary printed and how it exited.
pub struct Ran {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Ran {
    /// Stdout, parsed as JSON.
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|err| {
            panic!(
                "fixture: --json prints JSON ({err}); stdout={:?} stderr={:?}",
                self.stdout, self.stderr
            )
        })
    }

    /// The error envelope's `code`, after checking the envelope against its schema.
    pub fn error_code(&self) -> (Option<i32>, String) {
        let envelope = self.json();
        assert_error_schema(&envelope);
        (self.code, envelope["code"].as_str().unwrap().to_owned())
    }
}

/// Runs `keyval-server` with `args`.
pub fn keyval_server(args: &[&str]) -> Ran {
    let output = Command::new(env!("CARGO_BIN_EXE_keyval-server"))
        .args(args)
        .output()
        .unwrap();
    Ran {
        code: output.status.code(),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

/// `keyval-server init` for node `node` of the config at `config`, with `extra` arguments.
pub fn init(config: &Path, node: &str, instance: &str, extra: &[&str]) -> Ran {
    let mut args = vec![
        "init",
        "--config",
        config.to_str().unwrap(),
        "--node",
        node,
        "--instance",
        instance,
    ];
    args.extend_from_slice(extra);
    keyval_server(&args)
}

/// `keyval-server run --json` for node `node` of the config at `config`.
pub fn run(config: &Path, node: &str) -> Ran {
    keyval_server(&[
        "run",
        "--config",
        config.to_str().unwrap(),
        "--node",
        node,
        "--json",
    ])
}

/// Asserts that `report` conforms to the init report's schema.
pub fn assert_init_schema(report: &Value) {
    assert_schema(INIT_SCHEMA, report);
}

/// Asserts that `report` conforms to the error envelope's schema.
pub fn assert_error_schema(report: &Value) {
    assert_schema(ERROR_SCHEMA, report);
}

fn assert_schema(schema: &str, report: &Value) {
    let schema: Value = serde_json::from_str(schema).unwrap();
    let validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(&schema)
        .unwrap();
    let errors: Vec<String> = validator
        .iter_errors(report)
        .map(|error| error.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "rule: the output conforms to {} (O-6); errors: {errors:?}; output: {report}",
        schema["title"]
    );
}
