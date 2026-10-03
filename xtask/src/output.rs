//! Output conventions shared by every `./dev` command (PLAN.md §11.13, O-6).
//!
//! Each command prints either human-readable text or, with `--json`, exactly one JSON
//! object on stdout. Every JSON object carries a versioned `schema` identifier
//! (`keyval.dev.<command>/v<n>`) and an `ok` flag. Diagnostics go to stderr.
//!
//! Exit statuses are uniform across commands:
//!
//! | Status | Code | Meaning |
//! |---|---|---|
//! | [`Status::Ok`] | 0 | The command ran and everything it checked passed |
//! | [`Status::Failed`] | 1 | The command ran and found a failure, or could not complete |
//! | [`Status::Usage`] | 2 | The arguments were invalid; nothing was run |

use std::ffi::OsStr;
use std::io::{self, Write};

use serde::Serialize;

/// Schema identifier of the error envelope written when a command cannot run.
pub const ERROR_SCHEMA: &str = "keyval.dev.error/v1";

/// How a command's result is rendered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Plain text for people.
    Human,
    /// One JSON object with a versioned schema identifier (robot mode, O-6).
    Json,
}

impl Format {
    /// Selects robot mode when `--json` appears before any `--` separator.
    ///
    /// This runs before argument parsing, so that even a usage error is reported in the
    /// format the caller asked for.
    pub fn detect<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for arg in args {
            let arg = arg.as_ref();
            if arg == "--" {
                break;
            }
            if arg == "--json" {
                return Self::Json;
            }
        }
        Self::Human
    }
}

/// Process exit status shared by every `./dev` command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Ran, and everything it checked passed.
    Ok,
    /// Ran and found a failure, or could not complete. Never a pass (P10).
    Failed,
    /// Invalid arguments; nothing was run.
    Usage,
}

impl Status {
    /// The process exit code for this status.
    pub const fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Failed => 1,
            Self::Usage => 2,
        }
    }
}

/// Why a command could not produce its normal report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The arguments were invalid.
    Usage,
    /// The command could not complete, e.g. the workspace root was not found.
    Internal,
    /// Preconditions were not met, e.g. a gate asked to judge a dirty checkout.
    Refused,
    /// Part of the plan's command surface that is not implemented yet. Never a pass (P10).
    NotImplemented,
}

impl ErrorKind {
    /// The exit status that this kind of error produces.
    pub const fn status(self) -> Status {
        match self {
            Self::Usage => Status::Usage,
            Self::Internal | Self::Refused | Self::NotImplemented => Status::Failed,
        }
    }
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope<'a> {
    schema: &'static str,
    ok: bool,
    error: ErrorBody<'a>,
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    kind: ErrorKind,
    message: &'a str,
}

/// Renders the JSON error envelope (`keyval.dev.error/v1`).
pub fn error_json(kind: ErrorKind, message: &str) -> Result<String, serde_json::Error> {
    serde_json::to_string(&ErrorEnvelope {
        schema: ERROR_SCHEMA,
        ok: false,
        error: ErrorBody { kind, message },
    })
}

/// Reports an error in the requested format and returns the matching exit status.
///
/// In human mode, `detail` (for example clap's rendered usage text) is printed to
/// stderr. In JSON mode, the envelope goes to stdout and carries `message` only.
pub fn report_error(format: Format, kind: ErrorKind, message: &str, detail: &str) -> Status {
    let written = match format {
        Format::Human => write_line(&mut io::stderr().lock(), detail.trim_end()),
        Format::Json => match error_json(kind, message) {
            Ok(json) => write_line(&mut io::stdout().lock(), &json),
            Err(err) => write_line(
                &mut io::stderr().lock(),
                &format!("error: cannot encode the error envelope: {err}"),
            ),
        },
    };
    match written {
        Ok(()) => kind.status(),
        // Nowhere left to report to (e.g. a closed pipe); the exit status still says it.
        Err(_) => Status::Failed,
    }
}

/// A command's result: rendered for people, or serialised as its JSON report.
pub trait Report: Serialize {
    /// Whether everything the command checked passed.
    fn ok(&self) -> bool;
    /// The human-readable rendering (no trailing newline needed).
    fn render_human(&self) -> String;
}

/// Prints `report` to stdout in `format` and returns the matching exit status.
pub fn emit(format: Format, report: &impl Report) -> Status {
    let text = match format {
        Format::Human => report.render_human(),
        Format::Json => match serde_json::to_string(report) {
            Ok(json) => json,
            Err(err) => {
                let message = format!("cannot encode the report as JSON: {err}");
                return report_error(format, ErrorKind::Internal, &message, &message);
            }
        },
    };
    match write_line(&mut io::stdout().lock(), text.trim_end()) {
        Ok(()) if report.ok() => Status::Ok,
        Ok(()) | Err(_) => Status::Failed,
    }
}

/// Writes `text` and a newline, returning I/O errors instead of panicking the way
/// `println!` does on a closed pipe (CD-8).
pub fn write_line(out: &mut impl Write, text: &str) -> io::Result<()> {
    out.write_all(text.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_selects_json_only_before_the_separator() {
        let cases: [(&[&str], Format); 5] = [
            (&[], Format::Human),
            (&["doctor"], Format::Human),
            (&["--json", "doctor"], Format::Json),
            (&["doctor", "--json"], Format::Json),
            (&["sim", "--", "--json"], Format::Human),
        ];
        for (args, expected) in cases {
            let observed = Format::detect(args);
            assert_eq!(
                observed, expected,
                "rule: --json before any `--` selects robot mode; args={args:?}"
            );
        }
    }

    #[test]
    fn exit_codes_are_distinct_and_stable() {
        let cases = [(Status::Ok, 0), (Status::Failed, 1), (Status::Usage, 2)];
        for (status, expected) in cases {
            assert_eq!(
                status.code(),
                expected,
                "rule: exit codes are part of the ./dev contract; status={status:?}"
            );
        }
    }

    #[test]
    fn usage_errors_exit_2_and_internal_errors_exit_1() {
        assert_eq!(ErrorKind::Usage.status(), Status::Usage);
        assert_eq!(ErrorKind::Internal.status(), Status::Failed);
        assert_eq!(ErrorKind::Refused.status(), Status::Failed);
        assert_eq!(ErrorKind::NotImplemented.status(), Status::Failed);
    }

    #[test]
    fn error_envelope_carries_schema_ok_and_kind() {
        let json = error_json(ErrorKind::Usage, "unrecognized subcommand 'nope'").unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let expected = serde_json::json!({
            "schema": "keyval.dev.error/v1",
            "ok": false,
            "error": { "kind": "usage", "message": "unrecognized subcommand 'nope'" },
        });
        assert_eq!(
            value, expected,
            "rule: the error envelope matches keyval.dev.error/v1; raw={json}"
        );
    }

    #[test]
    fn write_line_appends_exactly_one_newline() {
        let mut buffer = Vec::new();
        write_line(&mut buffer, "hello").unwrap();
        assert_eq!(
            buffer,
            b"hello\n",
            "observed={:?}",
            String::from_utf8_lossy(&buffer)
        );
    }
}
