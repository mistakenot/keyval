//! The fresh-process proof: run every seed in `processes` separate OS processes (this
//! binary's `run` mode), then compare their traces byte for byte. On a mismatch, report the
//! first differing event and keep both traces.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::trace::fnv1a;

/// One seed's traces that differ between processes.
#[derive(Debug)]
pub struct Mismatch {
    /// The seed.
    pub seed: u64,
    /// The two trace files, both kept.
    pub files: (PathBuf, PathBuf),
    /// 1-based line number of the first differing event.
    pub line: usize,
    /// That line in each trace (`<missing>` past the end).
    pub events: (String, String),
}

/// The proof's result.
#[derive(Debug)]
pub struct Report {
    /// Seeds run.
    pub seeds: u64,
    /// Fresh processes per seed.
    pub processes: u32,
    /// Seeds whose traces differ.
    pub mismatches: Vec<Mismatch>,
    /// FNV-1a over every seed's (first-process) trace fingerprint, in seed order.
    pub fingerprint: u64,
    /// Total trace bytes compared.
    pub bytes: u64,
}

fn trace_path(out: &Path, process: u32, seed: u64) -> PathBuf {
    out.join(format!("p{process}"))
        .join(format!("seed-{seed:04}.trace"))
}

/// Runs seeds `first..first + count`, each in `processes` fresh processes of this binary,
/// writing traces under `out`, and compares them. Each process is this binary re-executed
/// through the kernel's fixed path for the caller's own executable, `/proc/self/exe`
/// (Linux): a fixed path, never a computed one, because the proof only ever runs itself.
pub fn prove(
    first: u64,
    count: u64,
    processes: u32,
    entropy: &str,
    out: &Path,
) -> Result<Report, String> {
    if processes < 2 || count == 0 {
        return Err("prove needs at least one seed and two processes".to_owned());
    }
    for process in 0..processes {
        fs::create_dir_all(out.join(format!("p{process}")))
            .map_err(|e| format!("create {}: {e}", out.display()))?;
        for seed in first..first + count {
            let path = trace_path(out, process, seed);
            let status = Command::new("/proc/self/exe")
                .args([
                    "run",
                    "--seed",
                    &seed.to_string(),
                    "--entropy",
                    entropy,
                    "--out",
                ])
                .arg(&path)
                .env_clear()
                .output()
                .map_err(|e| format!("spawn /proc/self/exe: {e}"))?;
            if !status.status.success() {
                return Err(format!(
                    "process {process} seed {seed} failed ({}): {}",
                    status.status,
                    String::from_utf8_lossy(&status.stderr).trim()
                ));
            }
        }
    }
    let mut mismatches = Vec::new();
    let mut fingerprint = 0xcbf2_9ce4_8422_2325_u64;
    let mut bytes = 0_u64;
    for seed in first..first + count {
        let base_path = trace_path(out, 0, seed);
        let base =
            fs::read(&base_path).map_err(|e| format!("read {}: {e}", base_path.display()))?;
        bytes += base.len() as u64;
        fingerprint = (fingerprint ^ fnv1a(&base)).wrapping_mul(0x0000_0100_0000_01b3);
        for process in 1..processes {
            let other_path = trace_path(out, process, seed);
            let other =
                fs::read(&other_path).map_err(|e| format!("read {}: {e}", other_path.display()))?;
            bytes += other.len() as u64;
            if other != base {
                mismatches.push(first_difference(
                    seed,
                    (&base_path, &base),
                    (&other_path, &other),
                ));
            }
        }
    }
    Ok(Report {
        seeds: count,
        processes,
        mismatches,
        fingerprint,
        bytes,
    })
}

fn first_difference(seed: u64, a: (&Path, &[u8]), b: (&Path, &[u8])) -> Mismatch {
    let left: Vec<&[u8]> = a.1.split(|&c| c == b'\n').collect();
    let right: Vec<&[u8]> = b.1.split(|&c| c == b'\n').collect();
    let at = left.iter().zip(&right).take_while(|(x, y)| x == y).count();
    let show = |lines: &[&[u8]]| {
        lines.get(at).map_or_else(
            || "<missing>".to_owned(),
            |l| String::from_utf8_lossy(l).into_owned(),
        )
    };
    Mismatch {
        seed,
        files: (a.0.to_path_buf(), b.0.to_path_buf()),
        line: at + 1,
        events: (show(&left), show(&right)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_differing_event_is_reported() {
        let m = first_difference(
            3,
            (Path::new("a"), b"x\ny\nz\n"),
            (Path::new("b"), b"x\ny\nq\n"),
        );
        assert_eq!((m.seed, m.line), (3, 3));
        assert_eq!(m.events, ("z".to_owned(), "q".to_owned()));
        let short = first_difference(1, (Path::new("a"), b"x\n"), (Path::new("b"), b"x\ny\n"));
        assert_eq!(short.line, 2);
        assert_eq!(short.events.0, "");
    }
}
