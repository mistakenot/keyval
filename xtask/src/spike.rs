//! SPIKE-DETERMINISM: the W02 spike's fresh-process proof (PLAN.md §12.2 W02; W02.4).
//!
//! Builds the standalone `spike/` workspace into its own target directory and runs its
//! `prove` mode:
//! - **The proof:** every seed in `0..SEEDS`, each in `PROCESSES` fresh OS processes, must
//!   give byte-identical traces. Every run must also meet the spike's non-vacuity floor:
//!   leaderships, a re-election after a crash, two crash-restarts, applied writes.
//! - **The negative control:** with wall-clock entropy, at least one of `CONTROL_SEEDS`
//!   seeds must diverge, or the comparison could not detect nondeterminism at all.
//!
//! The claim is simulation-evidenced (§10.15): no divergence in this many seeds and
//! processes on this build. It is evidence, not proof.
//!
//! The spike builds into its own per-checkout target directory, marked for `./dev prune`
//! like a checkout's, and its traces stay there under `spike-proof/` for inspection.

use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cargo::{self, Cargo};
use crate::check::StaticOutcome;
use crate::process;

/// Seeds the proof runs.
pub const SEEDS: u64 = 100;

/// Fresh processes per seed.
pub const PROCESSES: u32 = 3;

/// Seeds the negative control runs.
pub const CONTROL_SEEDS: u64 = 5;

/// The spike workspace, relative to the root.
pub const DIR: &str = "spike";

/// Each cargo command: the build, or one proof run.
const LIMIT: Duration = Duration::from_mins(20);

// check: SPIKE-DETERMINISM
/// Builds the spike and runs the proof and the negative control. One case per (seed,
/// process) of the proof, plus one for the control.
pub fn check_spike(root: &Path) -> Result<StaticOutcome, String> {
    let spike = root.join(DIR);
    let target = target_dir(root, &spike)?;
    // Local, never rch: the proof's processes and trace files must be on this machine.
    let local = Cargo::local().with_target_dir(target.clone());
    let mut cmd = local.command(&spike, &["build", "--locked", "--quiet"]);
    let compiled = process::run(&mut cmd, LIMIT)
        .map_err(|err| format!("{}: {err}", local.describe(&["build"])))?;
    if !compiled.status.success() {
        return Err(format!(
            "building {DIR}/ failed ({}): {}",
            compiled.status,
            tail(&compiled.stderr)
        ));
    }
    let proofs = target.join("spike-proof");
    let mut violations = Vec::new();
    let mut cases = 0;

    let seeds = format!("0..{SEEDS}");
    let out = proofs.join("seeded");
    let processes = PROCESSES.to_string();
    let proof = run(
        &local,
        &spike,
        &[
            "prove",
            "--seeds",
            &seeds,
            "--processes",
            &processes,
            "--out",
        ],
        &out,
    )?;
    if proof.0 && identical(&proof.1) {
        cases += usize::try_from(SEEDS * u64::from(PROCESSES)).map_err(|e| e.to_string())?;
    } else {
        violations.push(format!(
            "{SEEDS} seeds x {PROCESSES} fresh processes are not byte-identical (traces kept under {}): {}",
            out.display(),
            tail(&proof.1)
        ));
    }

    let control_seeds = format!("0..{CONTROL_SEEDS}");
    let out = proofs.join("control");
    let control = run(
        &local,
        &spike,
        &[
            "prove",
            "--seeds",
            &control_seeds,
            "--processes",
            "2",
            "--entropy",
            "clock",
            "--out",
        ],
        &out,
    )?;
    cases += 1;
    match diverged(&control.1) {
        Some(n) if !control.0 && n > 0 => {}
        _ => violations.push(format!(
            "negative control: wall-clock entropy did not diverge, so the proof cannot detect \
             nondeterminism: {}",
            tail(&control.1)
        )),
    }
    Ok(StaticOutcome { cases, violations })
}

/// The spike's target directory: `keyval-checkout-<hash>` of its real path under the
/// shared base, as the `./dev` launcher makes for a checkout, with the marker that lets
/// `./dev prune` remove it once the spike's checkout is gone (Q9).
fn target_dir(root: &Path, spike: &Path) -> Result<PathBuf, String> {
    let real = fs::canonicalize(spike)
        .map_err(|err| format!("cannot resolve {}: {err}", spike.display()))?;
    let target = cargo::target_base(root).join(cargo::checkout_dir_name(&real));
    fs::create_dir_all(&target).map_err(|err| format!("create {}: {err}", target.display()))?;
    let mut marker = real.as_os_str().as_bytes().to_vec();
    marker.push(b'\n');
    let path = target.join(cargo::CHECKOUT_MARKER);
    fs::write(&path, marker).map_err(|err| format!("write {}: {err}", path.display()))?;
    Ok(target)
}

/// Runs the already-built spike through `cargo run` (a fixed program, never a computed
/// executable path) with `args` followed by `out`; returns (success, stdout).
fn run(cargo: &Cargo, spike: &Path, args: &[&str], out: &Path) -> Result<(bool, String), String> {
    let out = out
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 path {}", out.display()))?;
    let mut command_line = vec!["run", "--locked", "--quiet", "--"];
    command_line.extend_from_slice(args);
    command_line.push(out);
    let mut cmd = cargo.command(spike, &command_line);
    let captured = process::run(&mut cmd, LIMIT)
        .map_err(|err| format!("{}: {err}", cargo.describe(&command_line)))?;
    Ok((captured.status.success(), captured.stdout))
}

/// Whether `prove` reported every seed byte-identical.
fn identical(stdout: &str) -> bool {
    stdout
        .lines()
        .any(|line| line.starts_with("prove: ") && line.contains("byte-identical"))
}

/// The number of diverged seeds `prove` reported, if it reported any.
fn diverged(stdout: &str) -> Option<u64> {
    stdout.lines().find_map(|line| {
        let rest = line.strip_prefix("prove: ")?;
        let (count, after) = rest.split_once(" of ")?;
        after
            .contains("seeds diverged")
            .then(|| count.parse().ok())
            .flatten()
    })
}

/// The last few lines of `text`, for a failure message.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(6)..].join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proven_run_is_recognised() {
        let out = "prove: 100 seeds x 3 fresh processes: byte-identical (1 trace bytes; fingerprint 0; entropy=seeded)\n";
        assert!(identical(out));
        assert!(!identical(
            "MISMATCH seed 3 at event 2:\nprove: 1 of 100 seeds diverged across 3 processes\n"
        ));
        assert!(!identical(""), "rule: no output is no proof (P10)");
    }

    #[test]
    fn divergence_counts_are_parsed() {
        let out = "MISMATCH seed 0 at event 2:\n  a: x\n  b: y\nprove: 5 of 5 seeds diverged across 2 processes (entropy=clock); traces kept under /t\n";
        assert_eq!(diverged(out), Some(5));
        assert_eq!(
            diverged("prove: 100 seeds x 3 fresh processes: byte-identical\n"),
            None
        );
        assert_eq!(
            diverged(""),
            None,
            "rule: a silent control is a failed control"
        );
    }

    #[test]
    fn tails_are_bounded() {
        let text = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            tail(&text),
            "line 14 | line 15 | line 16 | line 17 | line 18 | line 19"
        );
        assert_eq!(tail(""), "");
    }
}
