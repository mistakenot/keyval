//! W02 Raft determinism spike (PLAN.md §7.3, §12.2 W02; W02.4).
//!
//! A seeded three-node raft-rs cluster, driven in one process through the patched vendored
//! `RawNode` (W02.3's mandatory `Entropy` source), with crash-restarts and re-elections.
//! It produces a spike-local canonical trace. `prove` runs every seed in several fresh OS
//! processes and requires byte-identical traces. On a mismatch it reports the first
//! differing event and keeps both traces.
//!
//! ```text
//! keyval-raft-spike run   --seed N --out FILE [--entropy seeded|clock]
//! keyval-raft-spike prove --seeds A..B --processes P --out DIR [--entropy seeded|clock]
//! ```
//!
//! `--entropy clock` is the negative control: each node's entropy comes from the wall clock,
//! so `prove` must detect divergence. Exit status: 0 proven, 1 failed (mismatch, vacuous run
//! or error), 2 usage.

mod cluster;
mod prove;
mod trace;

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use cluster::EntropyMode;

fn usage(why: &str) -> ExitCode {
    eprintln!(
        "{why}\nusage: keyval-raft-spike run --seed N --out FILE [--entropy seeded|clock]\n       \
         keyval-raft-spike prove --seeds A..B --processes P --out DIR [--entropy seeded|clock]"
    );
    ExitCode::from(2)
}

/// The value after `--flag`.
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn entropy(args: &[String]) -> Result<EntropyMode, String> {
    match flag(args, "--entropy").as_deref() {
        None | Some("seeded") => Ok(EntropyMode::Seeded),
        Some("clock") => Ok(EntropyMode::Clock),
        Some(other) => Err(format!("unknown --entropy {other:?}")),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => run(&args),
        Some("prove") => prove(&args),
        _ => usage("a command is required"),
    }
}

fn run(args: &[String]) -> ExitCode {
    let (Some(seed), Some(out)) = (flag(args, "--seed"), flag(args, "--out")) else {
        return usage("run needs --seed and --out");
    };
    let Ok(seed) = seed.parse::<u64>() else {
        return usage("--seed must be an unsigned integer");
    };
    let mode = match entropy(args) {
        Ok(mode) => mode,
        Err(why) => return usage(&why),
    };
    let outcome = match cluster::run(seed, mode) {
        Ok(outcome) => outcome,
        Err(why) => {
            eprintln!("seed {seed}: {why}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = fs::write(&out, &outcome.trace) {
        eprintln!("write {out}: {e}");
        return ExitCode::from(1);
    }
    if let Some(why) = outcome.stats.vacuity() {
        eprintln!("seed {seed}: {why}; stats={:?}", outcome.stats);
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn prove(args: &[String]) -> ExitCode {
    let (Some(seeds), Some(processes), Some(out)) = (
        flag(args, "--seeds"),
        flag(args, "--processes"),
        flag(args, "--out"),
    ) else {
        return usage("prove needs --seeds, --processes and --out");
    };
    let Some((first, end)) = seeds
        .split_once("..")
        .and_then(|(a, b)| Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?)))
        .filter(|(a, b)| a < b)
    else {
        return usage("--seeds must be A..B with A < B");
    };
    let Ok(processes) = processes.parse::<u32>() else {
        return usage("--processes must be an unsigned integer");
    };
    let mode = flag(args, "--entropy").unwrap_or_else(|| "seeded".to_owned());
    if entropy(args).is_err() {
        return usage("--entropy must be seeded or clock");
    }
    match prove::prove(first, end - first, processes, &mode, &PathBuf::from(&out)) {
        Err(why) => {
            eprintln!("prove: {why}");
            ExitCode::from(1)
        }
        Ok(report) if report.mismatches.is_empty() => {
            println!(
                "prove: {} seeds x {} fresh processes: byte-identical ({} trace bytes; fingerprint {:016x}; entropy={mode})",
                report.seeds, report.processes, report.bytes, report.fingerprint
            );
            ExitCode::SUCCESS
        }
        Ok(report) => {
            for m in &report.mismatches {
                println!(
                    "MISMATCH seed {} at event {}:\n  {}: {}\n  {}: {}",
                    m.seed,
                    m.line,
                    m.files.0.display(),
                    m.events.0,
                    m.files.1.display(),
                    m.events.1
                );
            }
            println!(
                "prove: {} of {} seeds diverged across {} processes (entropy={mode}); traces kept under {out}",
                report.mismatches.len(),
                report.seeds,
                report.processes
            );
            ExitCode::from(1)
        }
    }
}
