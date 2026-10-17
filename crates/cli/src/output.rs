//! The error envelope of every `keyval` command (PLAN.md O-6, §8.7, §11.13; br-tdy6).
//!
//! A command that cannot produce its normal report prints one `keyval.cli.error/v1`
//! document on stdout under `--json`, or the message on stderr without it, and exits 2.
//!
//! `--json` is detected before the arguments are parsed, as `./dev` does it
//! (`xtask/src/output.rs`), so even a usage error comes in the format the caller asked for.

use std::ffi::OsStr;

use serde::Serialize;

/// The error envelope's schema identifier; its JSON Schema is
/// `schemas/cli/error.v1.schema.json`.
pub const ERROR_SCHEMA: &str = "keyval.cli.error/v1";

/// The exit status of a usage or configuration error (§11.13).
pub const USAGE_EXIT: u8 = 2;

/// Whether `args` (without the program name) ask for `--json`. Nothing after `--` counts.
pub fn json_requested<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    args.into_iter()
        .map_while(|arg| (arg.as_ref() != "--").then(|| arg.as_ref() == "--json"))
        .any(|json| json)
}

/// What kind of error stopped the command before its report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Invalid arguments: nothing ran.
    Usage,
    /// The `--config` file cannot be read or parsed.
    Config,
    /// The pending store cannot be used, or holds no record of the request asked for.
    Pending,
    /// The client could not start: no node to send to, or no entropy for a request ID.
    Client,
}

/// The error itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ErrorDetail {
    /// What kind of error it is.
    pub kind: ErrorKind,
    /// The error, in words.
    pub message: String,
}

/// The `keyval.cli.error/v1` document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ErrorReport {
    /// [`ERROR_SCHEMA`].
    pub schema: &'static str,
    /// Always false: the command did not produce its report.
    pub ok: bool,
    /// The error.
    pub error: ErrorDetail,
}

impl ErrorReport {
    /// The envelope for an error of `kind` saying `message`.
    #[must_use]
    pub fn new(kind: ErrorKind, message: &str) -> Self {
        let message = message.trim();
        Self {
            schema: ERROR_SCHEMA,
            ok: false,
            error: ErrorDetail {
                kind,
                message: if message.is_empty() {
                    "invalid arguments".to_owned()
                } else {
                    message.to_owned()
                },
            },
        }
    }
}
