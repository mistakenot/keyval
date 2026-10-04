//! `keyval-server`: one keyval node (PLAN.md §8.7). This binary only parses arguments,
//! loads the configuration and prints; [`server::init`] and [`server::run`] do the work,
//! and [`server::output`] shapes what is printed.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde::Serialize;
use server::output::{Code, Command as Which, ErrorReport, InitReport};
use types::{Config, InstanceId, NodeId};

/// One keyval node.
#[derive(Debug, Parser)]
#[command(name = "keyval-server", version, about = "One keyval node")]
struct Cli {
    /// What to do.
    #[command(subcommand)]
    command: Command,
}

/// A keyval-server command.
#[derive(Debug, Subcommand)]
enum Command {
    /// Initialise a node's data directory (§7.7 step 0): the only way one is created.
    Init {
        /// The cluster config file.
        #[arg(long)]
        config: PathBuf,
        /// The node's ID in that config.
        #[arg(long)]
        node: u64,
        /// The cluster instance ID: 32 hex digits, the same for every node of the cluster.
        #[arg(long)]
        instance: String,
        /// Print the `keyval.server.init/v1` JSON report, or the error envelope.
        #[arg(long)]
        json: bool,
    },
    /// Start the node from its initialised data directory; a missing or uninitialised one
    /// is refused (D-52).
    Run {
        /// The cluster config file.
        #[arg(long)]
        config: PathBuf,
        /// The node's ID in that config.
        #[arg(long)]
        node: u64,
        /// Print errors as the `keyval.server.error/v1` JSON envelope.
        #[arg(long)]
        json: bool,
    },
}

/// Loads the config file at `path`: relative data directories resolve against its
/// directory (§7.7 step 0).
fn load(path: &Path) -> Result<Config, (Code, String)> {
    let text = std::fs::read_to_string(path).map_err(|err| {
        (
            Code::Config,
            format!("cannot read {}: {err}", path.display()),
        )
    })?;
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Config::parse(&text, dir).map_err(|err| (Code::Config, format!("{}: {err}", path.display())))
}

fn node_id(node: u64) -> Result<NodeId, (Code, String)> {
    NodeId::new(node).map_err(|err| (Code::Usage, format!("--node {node}: {err}")))
}

/// Prints `value` as JSON, or `human` otherwise, and exits with `code`.
fn finish(json: bool, value: &impl Serialize, human: &str, code: u8) -> ExitCode {
    if json {
        match serde_json::to_string_pretty(value) {
            Ok(text) => println!("{text}"),
            Err(err) => {
                eprintln!("keyval-server: cannot encode the output ({err}): {human}");
                return ExitCode::from(1);
            }
        }
    } else if code == 0 {
        print!("{human}");
    } else {
        eprint!("{human}");
    }
    ExitCode::from(code)
}

fn fail(json: bool, error: &ErrorReport) -> ExitCode {
    finish(json, error, &error.human(), error.code.exit_code())
}

fn init(config: &Path, node: u64, instance: &str, json: bool) -> ExitCode {
    let setup = load(config).and_then(|config| {
        let node = node_id(node)?;
        let instance = InstanceId::parse(instance)
            .map_err(|err| (Code::Usage, format!("--instance {instance}: {err}")))?;
        Ok((config, node, instance))
    });
    let (config, node, instance) = match setup {
        Ok(setup) => setup,
        Err((code, message)) => {
            return fail(json, &ErrorReport::new(Which::Init, code, message, None));
        }
    };
    match server::init::init(&config, node, instance) {
        Ok(initialised) => {
            let report = InitReport::new(&initialised);
            finish(json, &report, &report.human(), 0)
        }
        Err(refusal) => fail(json, &ErrorReport::init(&refusal)),
    }
}

fn run(config: &Path, node: u64, json: bool) -> ExitCode {
    let setup = load(config).and_then(|config| Ok((config, node_id(node)?)));
    let (config, node) = match setup {
        Ok(setup) => setup,
        Err((code, message)) => {
            return fail(json, &ErrorReport::new(Which::Run, code, message, None));
        }
    };
    match server::run::prepare(&config, node) {
        Ok(prepared) => fail(json, &ErrorReport::not_serving(&prepared)),
        Err(refusal) => fail(json, &ErrorReport::run(&refusal)),
    }
}

fn main() -> ExitCode {
    // Before parsing, so that even a usage error comes in the format asked for.
    let json = std::env::args_os().skip(1).any(|arg| arg == "--json");
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // Help and version are not errors; without --json, clap reports a usage error as
        // usual (text on stderr, exit 2).
        Err(err) if !json || !err.use_stderr() => err.exit(),
        Err(err) => {
            let which = if std::env::args_os().nth(1).is_some_and(|arg| arg == "run") {
                Which::Run
            } else {
                Which::Init
            };
            return fail(
                true,
                &ErrorReport::new(which, Code::Usage, err.to_string(), None),
            );
        }
    };
    match cli.command {
        Command::Init {
            config,
            node,
            instance,
            json,
        } => init(&config, node, &instance, json),
        Command::Run { config, node, json } => run(&config, node, json),
    }
}
