//! `model-server`: the reference model server (PLAN.md §10.14 use 3, W30), a test-only,
//! single-node HTTP server over the reference model that implements `api/openapi.yaml`.
//!
//! ```text
//! model-server [--listen ADDR] [--node-id N] [--cluster-id ID]
//!              [--max-key-bytes N] [--max-value-bytes N]
//!              [--max-live-keys N] [--max-live-bytes N] [--max-log-entries N]
//! ```
//!
//! `--listen` defaults to `127.0.0.1:0` (a free port). Once bound, the server prints one
//! line, `model-server listening on http://ADDR`, so a test runner can read its URL. The
//! limits default to the plan's (§7.10). A usage error exits 2.

use std::io::{self, Write};
use std::process::ExitCode;

use model::server::{Config, Server};
use model::{HistoryLimits, Limits};

/// The command line, parsed.
struct Options {
    listen: String,
    node_id: u64,
    cluster_id: String,
    limits: [u64; 4],
    max_log_entries: u64,
}

fn parse(arguments: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut options = Options {
        listen: "127.0.0.1:0".to_owned(),
        node_id: 1,
        cluster_id: "keyval-model".to_owned(),
        limits: [
            Limits::DEFAULT.max_key_bytes(),
            Limits::DEFAULT.max_value_bytes(),
            Limits::DEFAULT.max_live_keys(),
            Limits::DEFAULT.max_live_bytes(),
        ],
        max_log_entries: HistoryLimits::DEFAULT.max_log_entries(),
    };
    let mut arguments = arguments;
    while let Some(flag) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?;
        let number = || {
            value
                .parse::<u64>()
                .map_err(|_| format!("{flag} takes a non-negative integer, not {value:?}"))
        };
        match flag.as_str() {
            "--listen" => options.listen.clone_from(&value),
            "--node-id" => options.node_id = number()?,
            "--cluster-id" => options.cluster_id.clone_from(&value),
            "--max-key-bytes" => options.limits[0] = number()?,
            "--max-value-bytes" => options.limits[1] = number()?,
            "--max-live-keys" => options.limits[2] = number()?,
            "--max-live-bytes" => options.limits[3] = number()?,
            "--max-log-entries" => options.max_log_entries = number()?,
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    Ok(options)
}

fn run() -> Result<(), String> {
    let options = parse(std::env::args().skip(1))?;
    let [key, value, keys, bytes] = options.limits;
    let limits = Limits::new(key, value, keys, bytes).map_err(|error| error.to_string())?;
    let history = HistoryLimits::new(options.max_log_entries).map_err(|error| error.to_string())?;
    let config = Config::new(options.node_id, options.cluster_id, limits, history)
        .map_err(|error| error.to_string())?;
    let server = Server::bind(&options.listen, config)
        .map_err(|error| format!("cannot listen on {}: {error}", options.listen))?;
    let address = server.local_addr().map_err(|error| error.to_string())?;
    let mut out = io::stdout().lock();
    writeln!(out, "model-server listening on http://{address}")
        .and_then(|()| out.flush())
        .map_err(|error| error.to_string())?;
    drop(out);
    server
        .serve()
        .map_err(|error| format!("serving stopped: {error}"))
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            let _ = writeln!(io::stderr(), "model-server: {why}");
            ExitCode::from(2)
        }
    }
}
