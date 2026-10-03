//! Black-box tests of `./dev doctor` against fake tool installations: a `PATH` holding
//! only scripted stand-ins for each tool (PLAN.md O-8, W01 acceptance: "reports every
//! required tool with its version and fails when one is missing").
#![cfg(test)]

mod doctor {
    use std::fmt::Write as _;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::sync::OnceLock;

    use serde_json::Value;

    /// (tool, [(arguments, stdout)]) — each fake prints `stdout` for exactly these
    /// arguments and fails on anything else.
    type FakeTool = (&'static str, Vec<(&'static str, String)>);

    /// The arguments of doctor's real ast-grep query, and one match it accepts.
    const AST_GREP_QUERY: &str =
        "run --lang rust --pattern mod $NAME; --json=compact xtask/src/main.rs";
    const AST_GREP_MATCH: &str = r#"[{"text":"mod a;","range":{},"file":"xtask/src/main.rs"}]"#;

    fn complete_install() -> Vec<FakeTool> {
        let ubs_codes = r#"{"exit_codes":[{"code":0},{"code":1},{"code":2},{"code":3}]}"#;
        vec![
            (
                "rustc",
                vec![(
                    "-vV",
                    "rustc 1.99.0 (fake)\nbinary: rustc\nrelease: 1.99.0".to_owned(),
                )],
            ),
            (
                "cargo",
                vec![
                    ("--version", "cargo 1.99.0 (fake)".to_owned()),
                    ("clippy --version", "clippy 0.1.99 (fake)".to_owned()),
                ],
            ),
            (
                "rustfmt",
                vec![("--version", "rustfmt 1.10.0 (fake)".to_owned())],
            ),
            ("git", vec![("--version", "git version 2.53.0".to_owned())]),
            (
                "br",
                vec![
                    ("--version", "br 0.7.4".to_owned()),
                    ("version --json", r#"{"version":"0.7.4"}"#.to_owned()),
                ],
            ),
            (
                "bv",
                vec![
                    ("--version", "bv v0.25.2".to_owned()),
                    ("--robot-next", r#"{"version":"v0.25.2"}"#.to_owned()),
                ],
            ),
            (
                "ntm",
                vec![
                    ("--version", "ntm version 1.36.1".to_owned()),
                    (
                        "--robot-version",
                        r#"{"system":{"version":"1.36.1"}}"#.to_owned(),
                    ),
                ],
            ),
            (
                "ubs",
                vec![
                    ("--version", "UBS Meta-Runner v5.4.17".to_owned()),
                    ("robot-docs exit-codes", ubs_codes.to_owned()),
                ],
            ),
            (
                "ast-grep",
                vec![
                    ("--version", "ast-grep 0.45.3".to_owned()),
                    (AST_GREP_QUERY, AST_GREP_MATCH.to_owned()),
                ],
            ),
            ("dcg", vec![("--version", "dcg 0.4.0".to_owned())]),
            (
                "am",
                vec![
                    ("--version", "am 0.3.36".to_owned()),
                    ("health --json", r#"{"overall":"healthy"}"#.to_owned()),
                ],
            ),
        ]
    }

    /// A POSIX sh script using only builtins (`PATH` holds nothing but the fakes).
    fn script(responses: &[(&str, String)]) -> String {
        let mut text = String::from("#!/bin/sh\ncase \"$*\" in\n");
        for (args, stdout) in responses {
            assert!(
                !stdout.contains('\''),
                "fake output must not contain single quotes: {stdout:?}"
            );
            let lines: Vec<String> = stdout.lines().map(|line| format!("'{line}'")).collect();
            writeln!(text, "  '{args}') printf '%s\\n' {} ;;", lines.join(" ")).unwrap();
        }
        text.push_str("  *) echo \"fake tool: unexpected arguments: $*\" >&2; exit 64 ;;\nesac\n");
        text
    }

    fn write_install(dir: &Path, tools: &[FakeTool]) {
        fs::create_dir_all(dir).unwrap();
        for (name, responses) in tools {
            let path = dir.join(name);
            fs::write(&path, script(responses)).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn scenario_dir(name: &str) -> PathBuf {
        Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("doctor-fakes")
            .join(name)
    }

    /// Writes every scenario before any test spawns a process, so no fake script is
    /// still open for writing when another thread executes it (ETXTBSY).
    fn scenarios() -> &'static () {
        static READY: OnceLock<()> = OnceLock::new();
        READY.get_or_init(|| {
            write_install(&scenario_dir("complete"), &complete_install());

            let mut no_dcg = complete_install();
            no_dcg.retain(|(name, _)| *name != "dcg");
            write_install(&scenario_dir("no-dcg"), &no_dcg);

            let mut old_rustc = complete_install();
            for (name, responses) in &mut old_rustc {
                if *name == "rustc" {
                    *responses = vec![(
                        "-vV",
                        "rustc 1.98.0 (fake)\nbinary: rustc\nrelease: 1.98.0".to_owned(),
                    )];
                }
            }
            write_install(&scenario_dir("old-rustc"), &old_rustc);

            let mut broken_am = complete_install();
            for (name, responses) in &mut broken_am {
                if *name == "am" {
                    *responses = vec![
                        ("--version", "am 0.3.36".to_owned()),
                        ("health --json", "mail server unreachable".to_owned()),
                    ];
                }
            }
            write_install(&scenario_dir("broken-am"), &broken_am);

            // An ast-grep whose --version works but which does nothing: it exits 0 and
            // prints nothing (br-gate-ubs-partial-scan-o59g).
            let mut hollow_ast_grep = complete_install();
            for (name, responses) in &mut hollow_ast_grep {
                if *name == "ast-grep" {
                    *responses = vec![
                        ("--version", "ast-grep 0.45.3".to_owned()),
                        (AST_GREP_QUERY, String::new()),
                    ];
                }
            }
            write_install(&scenario_dir("hollow-ast-grep"), &hollow_ast_grep);
        })
    }

    fn doctor(scenario: &str, json: bool) -> Output {
        scenarios();
        // Located at run time: a compile-time path outlives its checkout
        // (br-gate-shared-target-dir-dtqu).
        let root = Path::new(&std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("..");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_xtask"));
        if json {
            cmd.arg("--json");
        }
        cmd.arg("doctor")
            .env("PATH", scenario_dir(scenario))
            .env("KEYVAL_ROOT", root)
            .output()
            .unwrap()
    }

    fn describe(scenario: &str, out: &Output) -> String {
        format!(
            "scenario={scenario} status={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    }

    fn json_report(scenario: &str) -> (Output, Value) {
        let out = doctor(scenario, true);
        let value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|err| panic!("not JSON ({err}); {}", describe(scenario, &out)));
        (out, value)
    }

    fn tool<'a>(report: &'a Value, name: &str) -> &'a Value {
        report["tools"]
            .as_array()
            .and_then(|tools| tools.iter().find(|t| t["name"] == name))
            .unwrap_or_else(|| panic!("no entry for {name} in {report}"))
    }

    #[test]
    fn complete_install_passes_and_reports_every_version() {
        let (out, report) = json_report("complete");
        let context = describe("complete", &out);
        assert_eq!(out.status.code(), Some(0), "{context}");
        assert_eq!(report["schema"], "keyval.dev.doctor/v1", "{context}");
        assert_eq!(report["ok"], true, "{context}");
        assert_eq!(report["pinned_toolchain"], "1.99.0", "{context}");
        for (name, version) in [
            ("rustc", "rustc 1.99.0 (fake)"),
            ("cargo", "cargo 1.99.0 (fake)"),
            ("br", "br 0.7.4"),
            ("ubs", "UBS Meta-Runner v5.4.17"),
            ("ast-grep", "ast-grep 0.45.3"),
            ("dcg", "dcg 0.4.0"),
            ("am", "am 0.3.36"),
        ] {
            let entry = tool(&report, name);
            assert_eq!(entry["status"], "ok", "tool={name}; {context}");
            assert_eq!(entry["version"], version, "tool={name}; {context}");
        }
        let rch = tool(&report, "rch");
        assert_eq!(
            (&rch["status"], &rch["need"]),
            (&Value::from("missing"), &Value::from("optional")),
            "rule: a missing optional tool is reported but never fails doctor; {context}"
        );
    }

    #[test]
    fn a_missing_required_tool_fails_doctor() {
        let (out, report) = json_report("no-dcg");
        let context = describe("no-dcg", &out);
        assert_eq!(out.status.code(), Some(1), "{context}");
        assert_eq!(report["ok"], false, "{context}");
        assert_eq!(tool(&report, "dcg")["status"], "missing", "{context}");
        assert_eq!(report["failures"], serde_json::json!(["dcg"]), "{context}");
    }

    #[test]
    fn a_missing_required_tool_is_named_in_human_output() {
        let out = doctor("no-dcg", false);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(out.status.code(), Some(1), "{}", describe("no-dcg", &out));
        assert!(
            stdout.contains("MISSING") && stdout.contains("dcg") && stdout.contains("FAILED"),
            "{}",
            describe("no-dcg", &out)
        );
    }

    #[test]
    fn a_toolchain_other_than_the_pin_fails_doctor() {
        let (out, report) = json_report("old-rustc");
        let context = describe("old-rustc", &out);
        assert_eq!(out.status.code(), Some(1), "{context}");
        assert_eq!(tool(&report, "rustc")["status"], "mismatch", "{context}");
    }

    #[test]
    fn an_ast_grep_that_matches_nothing_fails_doctor() {
        let (out, report) = json_report("hollow-ast-grep");
        let context = describe("hollow-ast-grep", &out);
        assert_eq!(out.status.code(), Some(1), "{context}");
        let ast_grep = tool(&report, "ast-grep");
        assert_eq!(
            ast_grep["status"], "broken",
            "rule: --version alone never vouches for ast-grep (br-gate-ubs-partial-scan-o59g); \
             {context}"
        );
        assert_eq!(
            report["failures"],
            serde_json::json!(["ast-grep"]),
            "{context}"
        );
    }

    #[test]
    fn a_failing_capability_probe_fails_doctor() {
        let (out, report) = json_report("broken-am");
        let context = describe("broken-am", &out);
        assert_eq!(out.status.code(), Some(1), "{context}");
        let am = tool(&report, "am");
        assert_eq!(am["status"], "broken", "{context}");
        assert_eq!(
            am["version"], "am 0.3.36",
            "rule: the version is recorded even when the capability probe fails; {context}"
        );
    }
}
