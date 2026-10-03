//! `./dev doctor`: verify the development environment (PLAN.md §11.1, O-8).
//!
//! Each tool is probed for its installed version and, where the project relies on one,
//! a real capability: usually its robot-mode JSON output (§11.5 rule 14). A tool's
//! documented behaviour is never assumed (RISK-16). A missing, broken or wrongly
//! versioned **required** tool fails the command. Optional tools are reported but never
//! fail it.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

use crate::output::Report;
use crate::process::{self, Captured, RunError};

/// Schema identifier of the `--json` report.
pub const SCHEMA: &str = "keyval.dev.doctor/v1";

const PROBE_LIMIT: Duration = Duration::from_secs(30);

/// Whether a missing tool fails `./dev doctor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Need {
    /// The project's workflow depends on it (O-8, §11.1).
    Required,
    /// Useful, reported, never fatal.
    Optional,
}

/// How a probe's output is judged.
#[derive(Clone, Copy, Debug)]
pub enum Expect {
    /// Exit 0; the result is the first non-empty output line.
    FirstLine,
    /// `rustc -vV`: the `release:` line must equal the pinned channel.
    PinnedRustc,
    /// `cargo --version`: the version word must equal the pinned channel.
    PinnedCargo,
    /// Exit 0 and stdout is a JSON object with this top-level key.
    JsonKey(&'static str),
    /// `ubs robot-docs exit-codes` documents the codes the gate relies on: 0 clean,
    /// 1 findings, 2 partial or refused scan, 3 nothing scanned.
    UbsExitCodes,
}

/// One command run against a tool, and how its output is judged.
#[derive(Clone, Copy, Debug)]
pub struct Probe {
    /// Program and arguments; the program is looked up on `PATH`.
    pub argv: &'static [&'static str],
    /// How the output is judged.
    pub expect: Expect,
}

/// A tool the development environment needs.
#[derive(Clone, Copy, Debug)]
pub struct ToolSpec {
    /// Display name.
    pub name: &'static str,
    /// What the project uses it for.
    pub purpose: &'static str,
    /// Whether its absence fails doctor.
    pub need: Need,
    /// Reports the installed version.
    pub version: Probe,
    /// Exercises a capability the project relies on, after the version probe passes.
    pub capability: Option<Probe>,
}

const fn probe(argv: &'static [&'static str], expect: Expect) -> Probe {
    Probe { argv, expect }
}

/// The environment `./dev doctor` verifies (O-8 lists the required set; §11.1 the rest).
pub const TOOLS: &[ToolSpec] = &[
    ToolSpec {
        name: "rustc",
        purpose: "Rust compiler, pinned by rust-toolchain.toml",
        need: Need::Required,
        version: probe(&["rustc", "-vV"], Expect::PinnedRustc),
        capability: None,
    },
    ToolSpec {
        name: "cargo",
        purpose: "Cargo, the only build tool",
        need: Need::Required,
        version: probe(&["cargo", "--version"], Expect::PinnedCargo),
        capability: None,
    },
    ToolSpec {
        name: "rustfmt",
        purpose: "format check (T0)",
        need: Need::Required,
        version: probe(&["rustfmt", "--version"], Expect::FirstLine),
        capability: None,
    },
    ToolSpec {
        name: "clippy",
        purpose: "lints with denied warnings (T0)",
        need: Need::Required,
        version: probe(&["cargo", "clippy", "--version"], Expect::FirstLine),
        capability: None,
    },
    ToolSpec {
        name: "git",
        purpose: "version control; tree IDs for receipts",
        need: Need::Required,
        version: probe(&["git", "--version"], Expect::FirstLine),
        capability: None,
    },
    ToolSpec {
        name: "br",
        purpose: "beads task tracker (robot mode)",
        need: Need::Required,
        version: probe(&["br", "--version"], Expect::FirstLine),
        capability: Some(probe(
            &["br", "version", "--json"],
            Expect::JsonKey("version"),
        )),
    },
    ToolSpec {
        name: "bv",
        purpose: "beads triage (robot mode)",
        need: Need::Required,
        version: probe(&["bv", "--version"], Expect::FirstLine),
        capability: Some(probe(&["bv", "--robot-next"], Expect::JsonKey("version"))),
    },
    ToolSpec {
        name: "ntm",
        purpose: "agent session manager",
        need: Need::Required,
        version: probe(&["ntm", "--version"], Expect::FirstLine),
        capability: Some(probe(
            &["ntm", "--robot-version"],
            Expect::JsonKey("system"),
        )),
    },
    ToolSpec {
        name: "ubs",
        purpose: "bug scan of the exact staged files (T0)",
        need: Need::Required,
        version: probe(&["ubs", "--version"], Expect::FirstLine),
        capability: Some(probe(
            &["ubs", "robot-docs", "exit-codes"],
            Expect::UbsExitCodes,
        )),
    },
    ToolSpec {
        name: "dcg",
        purpose: "destructive command guard",
        need: Need::Required,
        version: probe(&["dcg", "--version"], Expect::FirstLine),
        capability: None,
    },
    ToolSpec {
        name: "am",
        purpose: "MCP Agent Mail: identities, messages, file reservations",
        need: Need::Required,
        version: probe(&["am", "--version"], Expect::FirstLine),
        capability: Some(probe(
            &["am", "health", "--json"],
            Expect::JsonKey("overall"),
        )),
    },
    ToolSpec {
        name: "rch",
        purpose: "offloads cargo builds to remote workers",
        need: Need::Optional,
        version: probe(&["rch", "--version"], Expect::FirstLine),
        capability: None,
    },
    ToolSpec {
        name: "cass",
        purpose: "search past agent sessions",
        need: Need::Optional,
        version: probe(&["cass", "--version"], Expect::FirstLine),
        capability: None,
    },
    ToolSpec {
        name: "cm",
        purpose: "CASS memory: learned procedures",
        need: Need::Optional,
        version: probe(&["cm", "--version"], Expect::FirstLine),
        capability: None,
    },
];

/// The state of one tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Installed, the expected version, and its capability probe passed.
    Ok,
    /// Not found on `PATH`.
    Missing,
    /// Installed but a probe failed: bad exit, timeout or unexpected output.
    Broken,
    /// Installed but not the pinned version.
    Mismatch,
}

/// What doctor found for one tool.
#[derive(Debug, Serialize)]
pub struct ToolReport {
    /// Display name.
    pub name: &'static str,
    /// What the project uses it for.
    pub purpose: &'static str,
    /// Whether its absence fails doctor.
    pub need: Need,
    /// Overall state.
    pub status: ToolStatus,
    /// Where the program was found on `PATH`.
    pub path: Option<String>,
    /// First line of the version probe's output.
    pub version: Option<String>,
    /// The capability probe's command line, if the tool has one.
    pub capability: Option<String>,
    /// Why the tool is not `ok`.
    pub detail: Option<String>,
}

/// Facts about the machine, recorded for later capacity decisions (§11.12).
#[derive(Debug, Serialize)]
pub struct Machine {
    /// Available parallelism as reported by the OS.
    pub cpus: Option<usize>,
}

/// The `./dev doctor` report (`keyval.dev.doctor/v1`).
#[derive(Debug, Serialize)]
pub struct DoctorReport {
    /// Schema identifier.
    pub schema: &'static str,
    /// True when every required tool is `ok`.
    pub ok: bool,
    /// Workspace root the probes ran in.
    pub root: String,
    /// The channel pinned by `rust-toolchain.toml`.
    pub pinned_toolchain: Option<String>,
    /// Why the pin could not be read, if it could not.
    pub pin_error: Option<String>,
    /// Machine facts.
    pub machine: Machine,
    /// One entry per probed tool, in table order.
    pub tools: Vec<ToolReport>,
    /// Names of required tools that are not `ok`.
    pub failures: Vec<&'static str>,
}

/// Probes every tool in `tools` from `root`, judging the toolchain against `pin`.
pub fn run(root: &Path, pin: Result<String, String>, tools: &[ToolSpec]) -> DoctorReport {
    let (pinned_toolchain, pin_error) = match pin {
        Ok(pin) => (Some(pin), None),
        Err(why) => (None, Some(why)),
    };
    let reports: Vec<ToolReport> = tools
        .iter()
        .map(|spec| probe_tool(spec, root, pinned_toolchain.as_deref()))
        .collect();
    let failures: Vec<&'static str> = reports
        .iter()
        .filter(|r| r.need == Need::Required && r.status != ToolStatus::Ok)
        .map(|r| r.name)
        .collect();
    DoctorReport {
        schema: SCHEMA,
        ok: failures.is_empty() && pin_error.is_none(),
        root: root.display().to_string(),
        pinned_toolchain,
        pin_error,
        machine: Machine {
            cpus: std::thread::available_parallelism().ok().map(usize::from),
        },
        tools: reports,
        failures,
    }
}

/// The result of judging one probe.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Passed; carries the value worth recording (e.g. the version line).
    Pass(String),
    /// Ran, but reported a version other than the pin.
    Mismatch(String),
    /// Failed for another reason.
    Broken(String),
}

fn probe_tool(spec: &ToolSpec, root: &Path, pin: Option<&str>) -> ToolReport {
    let executable = spec.version.argv.first().copied().unwrap_or(spec.name);
    let mut report = ToolReport {
        name: spec.name,
        purpose: spec.purpose,
        need: spec.need,
        status: ToolStatus::Ok,
        path: process::find_in_path(executable).map(|p| p.display().to_string()),
        version: None,
        capability: spec.capability.map(|c| c.argv.join(" ")),
        detail: None,
    };
    match run_probe(&spec.version, root, pin) {
        Err(RunError::NotFound) => {
            report.status = ToolStatus::Missing;
            report.detail = Some(format!("`{executable}` not found on PATH"));
            return report;
        }
        Err(err) => return broken(report, &spec.version, &err.to_string()),
        Ok(Verdict::Pass(version)) => report.version = Some(version),
        Ok(Verdict::Mismatch(why)) => {
            report.status = ToolStatus::Mismatch;
            report.detail = Some(why);
            return report;
        }
        Ok(Verdict::Broken(why)) => return broken(report, &spec.version, &why),
    }
    if let Some(capability) = &spec.capability {
        match run_probe(capability, root, pin) {
            Ok(Verdict::Pass(_)) => {}
            Ok(Verdict::Mismatch(why) | Verdict::Broken(why)) => {
                return broken(report, capability, &why);
            }
            Err(err) => return broken(report, capability, &err.to_string()),
        }
    }
    report
}

fn broken(mut report: ToolReport, probe: &Probe, why: &str) -> ToolReport {
    report.status = ToolStatus::Broken;
    report.detail = Some(format!("`{}`: {why}", probe.argv.join(" ")));
    report
}

fn run_probe(probe: &Probe, root: &Path, pin: Option<&str>) -> Result<Verdict, RunError> {
    let Some(mut cmd) = static_command(probe.argv) else {
        return Ok(Verdict::Broken("empty probe command".to_owned()));
    };
    // From the root, so rustup resolves the toolchain that rust-toolchain.toml pins.
    cmd.current_dir(root);
    let captured = process::run(&mut cmd, PROBE_LIMIT)?;
    Ok(evaluate(probe.expect, &captured, pin))
}

/// A command whose executable and arguments are compile-time constants: probes come
/// only from static tables such as [`TOOLS`], never from user input, so the set of
/// executables doctor can run is a fixed allowlist.
fn static_command(argv: &'static [&'static str]) -> Option<Command> {
    let (first, rest) = argv.split_first()?;
    let mut cmd = Command::new(first);
    cmd.args(rest);
    Some(cmd)
}

fn evaluate(expect: Expect, captured: &Captured, pin: Option<&str>) -> Verdict {
    if !captured.status.success() {
        let stderr = first_line(&captured.stderr).unwrap_or("");
        return Verdict::Broken(format!("exited with {}: {stderr}", captured.status));
    }
    let first = first_line(&captured.stdout).or_else(|| first_line(&captured.stderr));
    match expect {
        Expect::FirstLine => first.map_or_else(
            || Verdict::Broken("printed nothing".to_owned()),
            |line| Verdict::Pass(line.to_owned()),
        ),
        Expect::PinnedRustc => {
            let release = captured
                .stdout
                .lines()
                .find_map(|line| line.strip_prefix("release: "));
            pinned(first, release, pin)
        }
        Expect::PinnedCargo => {
            let version = first.and_then(|line| line.split_whitespace().nth(1));
            pinned(first, version, pin)
        }
        Expect::JsonKey(key) => match serde_json::from_str::<serde_json::Value>(&captured.stdout) {
            Ok(value) if value.get(key).is_some() => Verdict::Pass(format!("json `{key}`")),
            Ok(_) => Verdict::Broken(format!("JSON output lacks the `{key}` key")),
            Err(err) => Verdict::Broken(format!("output is not JSON: {err}")),
        },
        Expect::UbsExitCodes => ubs_exit_codes(&captured.stdout),
    }
}

fn pinned(first: Option<&str>, found: Option<&str>, pin: Option<&str>) -> Verdict {
    match (first, found, pin) {
        (_, _, None) => Verdict::Mismatch("no readable toolchain pin to compare with".to_owned()),
        (_, None, Some(_)) | (None, _, _) => {
            Verdict::Broken("version not found in the output".to_owned())
        }
        (Some(line), Some(found), Some(pin)) if found == pin => Verdict::Pass(line.to_owned()),
        (Some(_), Some(found), Some(pin)) => Verdict::Mismatch(format!(
            "version {found} is active but rust-toolchain.toml pins {pin}"
        )),
    }
}

fn ubs_exit_codes(stdout: &str) -> Verdict {
    let value: serde_json::Value = match serde_json::from_str(stdout) {
        Ok(value) => value,
        Err(err) => return Verdict::Broken(format!("output is not JSON: {err}")),
    };
    let documented: Vec<u64> = value
        .get("exit_codes")
        .and_then(serde_json::Value::as_array)
        .map(|codes| {
            codes
                .iter()
                .filter_map(|c| c.get("code").and_then(serde_json::Value::as_u64))
                .collect()
        })
        .unwrap_or_default();
    let missing: Vec<u64> = [0, 1, 2, 3]
        .into_iter()
        .filter(|code| !documented.contains(code))
        .collect();
    if missing.is_empty() {
        Verdict::Pass("exit codes 0-3 documented".to_owned())
    } else {
        Verdict::Broken(format!("exit codes {missing:?} are not documented"))
    }
}

fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

impl Report for DoctorReport {
    fn ok(&self) -> bool {
        self.ok
    }

    fn render_human(&self) -> String {
        let mut out = format!(
            "keyval doctor: workspace {}, pinned toolchain {}\n",
            self.root,
            self.pinned_toolchain.as_deref().unwrap_or("UNREADABLE")
        );
        if let Some(why) = &self.pin_error {
            let _ = writeln!(out, "  pin error: {why}");
        }
        for tool in &self.tools {
            let status = match (tool.status, tool.need) {
                (ToolStatus::Ok, _) => "ok",
                (ToolStatus::Missing, Need::Required) => "MISSING",
                (ToolStatus::Missing, Need::Optional) => "missing",
                (ToolStatus::Broken, Need::Required) => "BROKEN",
                (ToolStatus::Broken, Need::Optional) => "broken",
                (ToolStatus::Mismatch, Need::Required) => "MISMATCH",
                (ToolStatus::Mismatch, Need::Optional) => "mismatch",
            };
            let need = match tool.need {
                Need::Required => "",
                Need::Optional => " (optional)",
            };
            let shown = tool
                .detail
                .as_deref()
                .or(tool.version.as_deref())
                .unwrap_or("");
            let _ = writeln!(out, "  {status:<8} {:<8} {shown}{need}", tool.name);
        }
        if self.ok {
            out.push_str("doctor: ok");
        } else {
            let _ = write!(
                out,
                "doctor: FAILED ({} required tool(s) not ok: {}{})",
                self.failures.len(),
                self.failures.join(", "),
                if self.pin_error.is_some() {
                    "; toolchain pin unreadable"
                } else {
                    ""
                }
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    // check: DEV-DOCTOR
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    fn captured(code: i32, stdout: &str, stderr: &str) -> Captured {
        Captured {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        }
    }

    const RUSTC_VV: &str = "rustc 1.99.0 (b940084d7 2026-09-28)\nbinary: rustc\n\
                            release: 1.99.0\nLLVM version: 23.1.1\n";

    #[test]
    fn pinned_rustc_passes_only_on_the_pinned_release() {
        let out = captured(0, RUSTC_VV, "");
        let cases = [
            (
                Some("1.99.0"),
                Verdict::Pass("rustc 1.99.0 (b940084d7 2026-09-28)".to_owned()),
            ),
            (
                Some("1.98.0"),
                Verdict::Mismatch(
                    "version 1.99.0 is active but rust-toolchain.toml pins 1.98.0".to_owned(),
                ),
            ),
            (
                None,
                Verdict::Mismatch("no readable toolchain pin to compare with".to_owned()),
            ),
        ];
        for (pin, expected) in cases {
            let observed = evaluate(Expect::PinnedRustc, &out, pin);
            assert_eq!(observed, expected, "pin={pin:?} output={RUSTC_VV:?}");
        }
    }

    #[test]
    fn pinned_cargo_compares_the_version_word() {
        let out = captured(0, "cargo 1.99.0 (5f94df478 2026-08-27)\n", "");
        assert!(matches!(
            evaluate(Expect::PinnedCargo, &out, Some("1.99.0")),
            Verdict::Pass(_)
        ));
        assert!(matches!(
            evaluate(Expect::PinnedCargo, &out, Some("1.100.0")),
            Verdict::Mismatch(_)
        ));
    }

    #[test]
    fn a_failing_exit_status_is_broken_whatever_the_output() {
        let out = captured(
            1,
            "rustc 1.99.0\nrelease: 1.99.0\n",
            "error: toolchain missing",
        );
        let observed = evaluate(Expect::PinnedRustc, &out, Some("1.99.0"));
        assert!(
            matches!(&observed, Verdict::Broken(why) if why.contains("toolchain missing")),
            "rule: a non-zero exit is never a pass; observed={observed:?}"
        );
    }

    #[test]
    fn json_key_requires_json_with_the_key() {
        let cases = [
            ("{\"version\":\"0.7.4\"}", true),
            ("{\"other\":1}", false),
            ("br 0.7.4", false),
        ];
        for (stdout, should_pass) in cases {
            let observed = evaluate(Expect::JsonKey("version"), &captured(0, stdout, ""), None);
            assert_eq!(
                matches!(observed, Verdict::Pass(_)),
                should_pass,
                "stdout={stdout:?} observed={observed:?}"
            );
        }
    }

    #[test]
    fn ubs_exit_codes_must_document_0_to_3() {
        let full = r#"{"exit_codes":[{"code":0},{"code":1},{"code":2},{"code":3}]}"#;
        let partial = r#"{"exit_codes":[{"code":0},{"code":1}]}"#;
        assert!(matches!(
            evaluate(Expect::UbsExitCodes, &captured(0, full, ""), None),
            Verdict::Pass(_)
        ));
        let observed = evaluate(Expect::UbsExitCodes, &captured(0, partial, ""), None);
        assert_eq!(
            observed,
            Verdict::Broken("exit codes [2, 3] are not documented".to_owned()),
            "input={partial}"
        );
    }

    #[test]
    fn a_tool_that_is_not_installed_is_missing_and_fails_doctor() {
        let spec = ToolSpec {
            name: "nosuchtool",
            purpose: "test",
            need: Need::Required,
            version: probe(
                &["keyval-definitely-not-installed", "--version"],
                Expect::FirstLine,
            ),
            capability: None,
        };
        let report = run(Path::new("."), Ok("1.99.0".to_owned()), &[spec]);
        let tool = report.tools.first().unwrap();
        assert_eq!(tool.status, ToolStatus::Missing, "observed={tool:?}");
        assert!(!report.ok, "rule: a missing required tool fails doctor");
        assert_eq!(report.failures, ["nosuchtool"]);
    }

    #[test]
    fn a_missing_optional_tool_is_reported_but_does_not_fail_doctor() {
        let present = ToolSpec {
            name: "sh",
            purpose: "test",
            need: Need::Required,
            version: probe(&["sh", "-c", "echo sh 1.0"], Expect::FirstLine),
            capability: None,
        };
        let absent = ToolSpec {
            name: "nosuchtool",
            purpose: "test",
            need: Need::Optional,
            version: probe(&["keyval-definitely-not-installed"], Expect::FirstLine),
            capability: None,
        };
        let report = run(Path::new("."), Ok("1.99.0".to_owned()), &[present, absent]);
        assert!(report.ok, "observed={report:?}");
        assert_eq!(
            report.tools.get(1).map(|t| t.status),
            Some(ToolStatus::Missing)
        );
        assert_eq!(
            report.tools.first().and_then(|t| t.version.as_deref()),
            Some("sh 1.0")
        );
    }

    #[test]
    fn a_failing_capability_probe_breaks_an_installed_tool() {
        let spec = ToolSpec {
            name: "sh",
            purpose: "test",
            need: Need::Required,
            version: probe(&["sh", "-c", "echo sh 1.0"], Expect::FirstLine),
            capability: Some(probe(
                &["sh", "-c", "echo not-json"],
                Expect::JsonKey("version"),
            )),
        };
        let report = run(Path::new("."), Ok("1.99.0".to_owned()), &[spec]);
        let tool = report.tools.first().unwrap();
        assert_eq!(tool.status, ToolStatus::Broken, "observed={tool:?}");
        assert!(!report.ok);
    }

    #[test]
    fn an_unreadable_pin_fails_doctor() {
        let report = run(
            Path::new("."),
            Err("no rust-toolchain.toml".to_owned()),
            &[],
        );
        assert!(!report.ok, "observed={report:?}");
        assert!(report.render_human().contains("pin error"));
    }

    #[test]
    fn the_required_set_covers_every_tool_named_by_o8() {
        for name in ["rustc", "cargo", "br", "bv", "ntm", "ubs", "dcg", "am"] {
            let spec = TOOLS.iter().find(|t| t.name == name);
            assert!(
                spec.is_some_and(|t| t.need == Need::Required),
                "rule: O-8 requires doctor to check {name}; spec={spec:?}"
            );
        }
    }
}
