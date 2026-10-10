//! The bounded stale-read poll (PLAN.md §10.8, C-7).
//!
//! Reads are legitimately stale, so a test that writes and then reads polls the target
//! node until `served_by.applied_index` reaches the write's `index`, and only then asserts
//! values. The poll is bounded in time and in attempts; running out is a failure (P10),
//! never a pass. A test that tolerates staleness says so by not calling this.

use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

/// A log index as the API reports it (`index`, `served_by.applied_index`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Index(pub u64);

impl Index {
    /// Reads `served_by.applied_index` from a read, scan or write response body.
    #[must_use]
    pub fn applied_in(body: &Value) -> Option<Self> {
        body.pointer("/served_by/applied_index")
            .and_then(Value::as_u64)
            .map(Self)
    }

    /// Reads the top-level `index` of a write response body.
    #[must_use]
    pub fn write_index_in(body: &Value) -> Option<Self> {
        body.get("index").and_then(Value::as_u64).map(Self)
    }
}

/// Time source and sleeper; tests inject a fake so the poll's bound is proven without waiting.
pub trait Clock {
    /// Time since the poll began.
    fn elapsed(&self) -> Duration;
    /// Waits.
    fn sleep(&mut self, how_long: Duration);
}

/// The real clock.
#[derive(Debug)]
pub struct SystemClock(Instant);

impl SystemClock {
    /// Starts counting now.
    #[must_use]
    pub fn start() -> Self {
        Self(Instant::now())
    }
}

impl Clock for SystemClock {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }

    fn sleep(&mut self, how_long: Duration) {
        thread::sleep(how_long);
    }
}

/// How long and how often to poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// Total budget.
    pub timeout: Duration,
    /// Pause between attempts; never zero.
    pub interval: Duration,
}

impl Policy {
    /// The suite default: 10 s budget, 50 ms apart.
    pub const DEFAULT: Self = Self {
        timeout: Duration::from_secs(10),
        interval: Duration::from_millis(50),
    };

    /// A policy; a zero interval is raised to 1 ms so the attempt count stays bounded.
    #[must_use]
    pub fn new(timeout: Duration, interval: Duration) -> Self {
        Self {
            timeout,
            interval: interval.max(Duration::from_millis(1)),
        }
    }
}

/// Why waiting ended without a usable read.
#[derive(Debug, PartialEq, Eq)]
pub enum PollError<E> {
    /// The node never reached `wanted` within the policy.
    Timeout {
        /// The index waited for.
        wanted: Index,
        /// The highest applied index seen, if any read succeeded.
        last_seen: Option<Index>,
        /// Reads attempted.
        attempts: u32,
    },
    /// A read itself failed; polling does not paper over transport or protocol errors.
    Read(E),
}

impl<E: fmt::Display> fmt::Display for PollError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout {
                wanted,
                last_seen,
                attempts,
            } => write!(
                f,
                "applied_index never reached {} (last seen {:?}) after {attempts} reads",
                wanted.0,
                last_seen.map(|i| i.0)
            ),
            Self::Read(err) => write!(f, "read failed while waiting: {err}"),
        }
    }
}

/// Calls `read` until it reports an applied index of at least `wanted`, and returns that
/// read's payload.
///
/// # Errors
/// [`PollError::Timeout`] when the budget is spent; [`PollError::Read`] on the first
/// failing read.
pub fn wait_applied<T, E>(
    wanted: Index,
    policy: Policy,
    clock: &mut impl Clock,
    mut read: impl FnMut() -> Result<(Index, T), E>,
) -> Result<T, PollError<E>> {
    let mut last_seen: Option<Index> = None;
    let mut attempts: u32 = 0;
    loop {
        attempts = attempts.saturating_add(1);
        let (applied, payload) = read().map_err(PollError::Read)?;
        if applied >= wanted {
            return Ok(payload);
        }
        last_seen = last_seen.max(Some(applied));
        if clock.elapsed() >= policy.timeout {
            return Err(PollError::Timeout {
                wanted,
                last_seen,
                attempts,
            });
        }
        clock.sleep(policy.interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Time advances only when the poll sleeps.
    struct Fake(Duration);

    impl Clock for Fake {
        fn elapsed(&self) -> Duration {
            self.0
        }
        fn sleep(&mut self, how_long: Duration) {
            self.0 += how_long;
        }
    }

    const POLICY: Policy = Policy {
        timeout: Duration::from_millis(100),
        interval: Duration::from_millis(10),
    };

    #[test]
    fn returns_the_read_that_caught_up() {
        let mut applied = [3, 4, 5, 6].into_iter();
        let got = wait_applied(Index(5), POLICY, &mut Fake(Duration::ZERO), || {
            let i = applied.next().unwrap();
            Ok::<_, String>((Index(i), format!("read at {i}")))
        });
        assert_eq!(
            got,
            Ok("read at 5".to_owned()),
            "rule: first read with applied >= wanted wins"
        );
    }

    #[test]
    fn already_caught_up_reads_once() {
        let mut reads = 0;
        let got = wait_applied(Index(2), POLICY, &mut Fake(Duration::ZERO), || {
            reads += 1;
            Ok::<_, String>((Index(9), ()))
        });
        assert_eq!((got, reads), (Ok(()), 1));
    }

    #[test]
    fn times_out_with_a_bounded_number_of_reads() {
        let got: Result<(), PollError<String>> =
            wait_applied(Index(10), POLICY, &mut Fake(Duration::ZERO), || {
                Ok((Index(4), ()))
            });
        assert_eq!(
            got,
            Err(PollError::Timeout {
                wanted: Index(10),
                last_seen: Some(Index(4)),
                attempts: 11
            }),
            "rule: 100 ms budget at 10 ms apart is 11 reads, then failure"
        );
    }

    #[test]
    fn a_failing_read_ends_the_poll_at_once() {
        let mut reads = 0;
        let got: Result<(), PollError<&str>> =
            wait_applied(Index(1), POLICY, &mut Fake(Duration::ZERO), || {
                reads += 1;
                Err("connection refused")
            });
        assert_eq!(
            (got, reads),
            (Err(PollError::Read("connection refused")), 1)
        );
    }

    #[test]
    fn a_zero_interval_is_raised() {
        assert_eq!(
            Policy::new(Duration::ZERO, Duration::ZERO).interval,
            Duration::from_millis(1)
        );
    }

    #[test]
    fn reads_indexes_from_bodies() {
        let body = json!({"index": 7, "served_by": {"node_id": 1, "applied_index": 9}});
        assert_eq!(Index::write_index_in(&body), Some(Index(7)));
        assert_eq!(Index::applied_in(&body), Some(Index(9)));
        assert_eq!(
            Index::applied_in(&json!({"served_by": {"applied_index": "9"}})),
            None
        );
        assert_eq!(Index::write_index_in(&json!({})), None);
    }

    #[test]
    fn the_system_clock_moves_forward() {
        let mut clock = SystemClock::start();
        clock.sleep(Duration::from_millis(2));
        assert!(clock.elapsed() >= Duration::from_millis(2));
    }
}
