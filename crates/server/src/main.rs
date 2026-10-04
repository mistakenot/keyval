//! `keyval-server`: one keyval node (PLAN.md §8.7). This binary only parses arguments,
//! loads the configuration and prints; [`server::init`] and [`server::run`] do the work,
//! and [`server::output`] shapes what is printed.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use serde::Serialize;
use serde_json::json;
use server::logs::base;
use server::output::{Code, Command as Which, ErrorReport, InitReport};
use server::runtime::{Options, start};
use server::serve::{Responders, serve};
use types::{Config, InstanceId, LogLevel, NodeId};

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
    /// Start the node from its initialised data directory and serve the client API on its
    /// client address; a missing or uninitialised directory is refused (D-52).
    Run {
        /// The cluster config file.
        #[arg(long)]
        config: PathBuf,
        /// The node's ID in that config.
        #[arg(long)]
        node: u64,
        /// The structured log file (O-3); `<data_dir>/node.log` by default.
        #[arg(long)]
        log_file: Option<PathBuf>,
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

fn run(config: &Path, node: u64, log_file: Option<PathBuf>, json: bool) -> ExitCode {
    let setup = load(config).and_then(|config| Ok((config, node_id(node)?)));
    let (config, node) = match setup {
        Ok(setup) => setup,
        Err((code, message)) => {
            return fail(json, &ErrorReport::new(Which::Run, code, message, None));
        }
    };
    let prepared = match server::run::prepare(&config, node) {
        Ok(prepared) => prepared,
        Err(refusal) => return fail(json, &ErrorReport::run(&refusal)),
    };
    let data_dir = prepared.data_dir.clone();
    let log_file = log_file.unwrap_or_else(|| data_dir.join("node.log"));
    let Some(address) = config.node(node).map(types::NodeConfig::client_address) else {
        return fail(
            json,
            &ErrorReport::new(
                Which::Run,
                Code::UnknownNode,
                format!("no node {node}"),
                None,
            ),
        );
    };
    let listener = match TcpListener::bind(address) {
        Ok(listener) => listener,
        Err(err) => {
            let message = format!("cannot listen on the client address {address}: {err}");
            return fail(
                json,
                &ErrorReport::new(Which::Run, Code::Config, message, Some(&data_dir)),
            );
        }
    };
    let responders = Responders::new();
    let options = Options {
        log_file: log_file.clone(),
    };
    let running = match start(prepared, &config, node, responders.outputs(), &options) {
        Ok(running) => running,
        Err(refusal) => return fail(json, &ErrorReport::start(&refusal, &data_dir)),
    };
    for warning in server::serve::exposure_warnings(&config, node) {
        eprintln!("keyval-server: {warning}");
        let mut record = base(LogLevel::Warn, "address_not_loopback");
        record.insert("warning".to_owned(), json!(warning));
        running.log(record);
    }
    let serving = match serve(listener, Arc::new(running), responders, &config) {
        Ok(serving) => serving,
        Err(err) => {
            let message = format!("cannot start serving: {err}");
            return fail(
                json,
                &ErrorReport::new(Which::Run, Code::Storage, message, Some(&data_dir)),
            );
        }
    };
    eprintln!(
        "keyval-server: node {node} serving http://{} (data {}, log {})",
        serving.address(),
        data_dir.display(),
        log_file.display()
    );
    match serving.wait() {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("keyval-server: {why}");
            ExitCode::from(1)
        }
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
        Command::Run {
            config,
            node,
            log_file,
            json,
        } => run(&config, node, log_file, json),
    }
}
