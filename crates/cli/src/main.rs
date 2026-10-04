//! `keyval`: the command-line tool (PLAN.md §8.6). This binary only parses arguments and
//! prints; [`cli::doctor`] builds the report, and [`cli::output`] the error envelope.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use cli::output::{ErrorKind, ErrorReport, USAGE_EXIT, json_requested};
use types::{Config, NodeId};

/// The keyval command-line tool.
#[derive(Debug, Parser)]
#[command(name = "keyval", version, about = "The keyval command-line tool")]
struct Cli {
    /// What to do.
    #[command(subcommand)]
    command: Command,
}

/// A keyval command.
#[derive(Debug, Subcommand)]
enum Command {
    /// Verify a node's data directory offline, without modifying it (O-7).
    Doctor {
        /// The node's data directory.
        data_dir: PathBuf,
        /// The cluster config, to verify the genesis binding as well (DUR-8).
        #[arg(long, requires = "node")]
        config: Option<PathBuf>,
        /// The node's ID in that config.
        #[arg(long, requires = "config")]
        node: Option<u64>,
        /// Print the `keyval.cli.doctor/v1` JSON report.
        #[arg(long)]
        json: bool,
    },
}

/// The configuration and node to verify the binding against, or why they cannot be used:
/// an unreadable or invalid config is a configuration error, a bad `--node` a usage error.
fn binding(
    config: Option<&Path>,
    node: Option<u64>,
) -> Result<Option<(Config, NodeId)>, (ErrorKind, String)> {
    let (Some(path), Some(node)) = (config, node) else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path).map_err(|err| {
        (
            ErrorKind::Config,
            format!("cannot read {}: {err}", path.display()),
        )
    })?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let config = Config::parse(&text, dir)
        .map_err(|err| (ErrorKind::Config, format!("{}: {err}", path.display())))?;
    let node =
        NodeId::new(node).map_err(|err| (ErrorKind::Usage, format!("--node {node}: {err}")))?;
    Ok(Some((config, node)))
}

/// Reports an error that stopped `keyval doctor` before its report, and exits 2: the
/// `keyval.cli.error/v1` envelope on stdout under `--json`, the message on stderr otherwise
/// (O-6, br-tdy6).
fn fail(json: bool, kind: ErrorKind, message: &str) -> ExitCode {
    if json {
        match serde_json::to_string_pretty(&ErrorReport::new(kind, message)) {
            Ok(text) => println!("{text}"),
            Err(err) => eprintln!("keyval: cannot encode the error ({err}): {message}"),
        }
    } else {
        eprintln!("keyval doctor: {message}");
    }
    ExitCode::from(USAGE_EXIT)
}

fn main() -> ExitCode {
    // Before parsing, so that even a usage error comes in the format asked for.
    let json = json_requested(std::env::args_os().skip(1));
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // Help and version are not errors; without --json, clap reports a usage error as
        // it always did (text on stderr, exit 2).
        Err(err) if !json || !err.use_stderr() => err.exit(),
        Err(err) => return fail(true, ErrorKind::Usage, &err.to_string()),
    };
    match cli.command {
        Command::Doctor {
            data_dir,
            config,
            node,
            json,
        } => {
            let binding = match binding(config.as_deref(), node) {
                Ok(binding) => binding,
                Err((kind, message)) => return fail(json, kind, &message),
            };
            let report = cli::doctor::doctor(
                &data_dir,
                binding.as_ref().map(|(config, node)| (config, *node)),
            );
            if json {
                match serde_json::to_string_pretty(&report) {
                    Ok(text) => println!("{text}"),
                    Err(err) => {
                        eprintln!("keyval doctor: cannot encode the report: {err}");
                        return ExitCode::from(1);
                    }
                }
            } else {
                print!("{}", report.human());
            }
            ExitCode::from(report.verdict.exit_code())
        }
    }
}
