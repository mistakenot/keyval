//! Running external tools with a wall-clock limit and bounded, captured output.

use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Most bytes kept from one stream of one process (CD-9: a limit on everything).
/// Hitting it is an error, never silent truncation: callers parse this output.
pub const MAX_CAPTURE_BYTES: usize = 64 * 1024 * 1024;

const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The output of a process that exited within its time limit.
#[derive(Debug)]
pub struct Captured {
    /// How the process exited.
    pub status: ExitStatus,
    /// Everything it wrote to stdout, lossily decoded as UTF-8.
    pub stdout: String,
    /// Everything it wrote to stderr, lossily decoded as UTF-8.
    pub stderr: String,
}

/// Why a process did not produce a [`Captured`] result.
#[derive(Debug)]
pub enum RunError {
    /// The program is not installed (not found on `PATH`).
    NotFound,
    /// The program could not be started or waited for.
    Io(io::Error),
    /// The program ran longer than its limit and was killed.
    TimedOut(Duration),
    /// The program wrote more than [`MAX_CAPTURE_BYTES`] to one stream.
    OutputTooLarge,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "not found on PATH"),
            Self::Io(err) => write!(f, "cannot run: {err}"),
            Self::TimedOut(limit) => write!(f, "timed out after {}s", limit.as_secs()),
            Self::OutputTooLarge => {
                write!(f, "wrote more than {MAX_CAPTURE_BYTES} bytes to one stream")
            }
        }
    }
}

/// Runs `cmd` to completion, with stdin closed and stdout/stderr captured, killing it
/// if it is still running after `limit`.
pub fn run(cmd: &mut Command, limit: Duration) -> Result<Captured, RunError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => RunError::NotFound,
        _ => RunError::Io(err),
    })?;
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                // Best effort: the process may have exited in the meantime.
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::TimedOut(limit));
            }
            Ok(None) => thread::sleep(POLL_INTERVAL),
            Err(err) => return Err(RunError::Io(err)),
        }
    };
    Ok(Captured {
        status,
        stdout: collect(stdout)?,
        stderr: collect(stderr)?,
    })
}

/// Result of reading one stream: the bytes, or `None` if the cap was exceeded.
type Drained = io::Result<Option<Vec<u8>>>;

fn drain<R: Read + Send + 'static>(mut pipe: R) -> JoinHandle<Drained> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut chunk = [0_u8; 8 * 1024];
        let mut overflowed = false;
        loop {
            let n = pipe.read(&mut chunk)?;
            if n == 0 {
                return Ok((!overflowed).then_some(kept));
            }
            if kept.len() + n > MAX_CAPTURE_BYTES {
                // Keep reading so the child never blocks on a full pipe.
                overflowed = true;
            } else if let Some(bytes) = chunk.get(..n) {
                kept.extend_from_slice(bytes);
            }
        }
    })
}

fn collect(handle: Option<JoinHandle<Drained>>) -> Result<String, RunError> {
    let Some(handle) = handle else {
        return Ok(String::new());
    };
    let drained = handle
        .join()
        .map_err(|_| RunError::Io(io::Error::other("output reader thread panicked")))?;
    match drained {
        Ok(Some(bytes)) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Ok(None) => Err(RunError::OutputTooLarge),
        Err(err) => Err(RunError::Io(err)),
    }
}

/// The first executable file called `program` in the directories of `PATH`.
pub fn find_in_path(program: &str) -> Option<PathBuf> {
    find_in(&env::var_os("PATH")?, program)
}

/// The first executable file called `program` in the directories of `search_path` (a
/// `PATH`-style list), as a child given that `PATH` would find it.
pub fn find_in(search_path: &OsStr, program: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    env::split_paths(search_path)
        .map(|dir| dir.join(program))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_captures_stdout_stderr_and_status() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out; echo err >&2; exit 3"]);
        let captured = run(&mut cmd, Duration::from_secs(10)).unwrap();
        assert_eq!(captured.stdout, "out\n", "observed={captured:?}");
        assert_eq!(captured.stderr, "err\n", "observed={captured:?}");
        assert_eq!(captured.status.code(), Some(3), "observed={captured:?}");
    }

    #[test]
    fn run_reports_a_missing_program_as_not_found() {
        let mut cmd = Command::new("keyval-definitely-not-an-installed-program");
        let result = run(&mut cmd, Duration::from_secs(10));
        assert!(
            matches!(result, Err(RunError::NotFound)),
            "rule: a missing program is NotFound, not a generic I/O error; observed={result:?}"
        );
    }

    #[test]
    fn run_kills_a_process_that_exceeds_its_limit() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exec sleep 30"]);
        let started = Instant::now();
        let result = run(&mut cmd, Duration::from_millis(200));
        assert!(
            matches!(result, Err(RunError::TimedOut(_))),
            "observed={result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "rule: the limit is enforced promptly; elapsed={:?}",
            started.elapsed()
        );
    }

    #[test]
    fn find_in_path_finds_sh_and_rejects_nonsense() {
        assert!(
            find_in_path("sh").is_some(),
            "sh must be on PATH for the test"
        );
        assert_eq!(
            find_in_path("keyval-definitely-not-an-installed-program"),
            None
        );
    }
}
