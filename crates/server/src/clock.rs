//! The clock adapter (PLAN.md §7.2, §7.10, §7.12): the monotonic clock turned into ticks.
//!
//! A ticker thread wakes once per `tick_ms` and hands over every tick that elapsed since
//! boot and was not handed over yet. After a pause (the process stopped, the machine
//! suspended, the thread starved) the overdue ticks arrive as one batch. The controller
//! merges them and processes at most `TICKS_PER_TURN` before each queued input (§7.10), so
//! deadlines fire on time, on every input, even while a sync is outstanding (§7.12).

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::inbox::Inbox;

/// How many whole ticks of `tick` fit in `elapsed`.
#[must_use]
pub fn ticks_in(elapsed: Duration, tick: Duration) -> u64 {
    let tick = tick.as_nanos().max(1);
    u64::try_from(elapsed.as_nanos() / tick).unwrap_or(u64::MAX)
}

/// Starts the ticker: from now on, every `tick` the ticks elapsed since `start` and not yet
/// handed over go to `inbox`, until it is closed.
///
/// # Errors
///
/// The operating system's error if the thread cannot be started.
pub fn spawn(inbox: Arc<Inbox>, tick: Duration) -> std::io::Result<JoinHandle<()>> {
    let start = Instant::now();
    thread::Builder::new()
        .name("keyval-clock".to_owned())
        .spawn(move || {
            let mut delivered: u64 = 0;
            while !inbox.is_closed() {
                let next = delivered.saturating_add(1);
                let at = start + tick.saturating_mul(u32::try_from(next).unwrap_or(u32::MAX));
                let now = Instant::now();
                if at > now {
                    thread::sleep(at - now);
                }
                let elapsed = ticks_in(start.elapsed(), tick);
                inbox.tick(elapsed.saturating_sub(delivered));
                delivered = delivered.max(elapsed);
            }
        })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::{spawn, ticks_in};
    use crate::inbox::Inbox;

    #[test]
    fn whole_ticks_count_and_a_pause_comes_back_as_one_batch() {
        let tick = Duration::from_millis(50);
        assert_eq!(
            [
                ticks_in(Duration::ZERO, tick),
                ticks_in(Duration::from_millis(49), tick),
                ticks_in(Duration::from_millis(50), tick),
                ticks_in(Duration::from_millis(1_049), tick),
            ],
            [0, 0, 1, 20],
            "rule: the clock counts whole ticks of tick_ms since boot; a one-second pause is \
             twenty ticks owed at once (§7.10)"
        );
    }

    #[test]
    fn the_ticker_hands_over_elapsed_ticks_until_closed() {
        let inbox = Arc::new(Inbox::new(4, 2));
        let tick = Duration::from_millis(2);
        let started = Instant::now();
        let ticker = spawn(Arc::clone(&inbox), tick).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        inbox.close();
        ticker.join().unwrap();
        let elapsed = super::ticks_in(started.elapsed(), tick);
        let got = inbox.take().ticks;
        assert!(
            got >= 10 && got <= elapsed + 1,
            "rule: the ticker hands over the ticks the monotonic clock says elapsed, never \
             more (§7.10): handed over {got} in 100 ms at 2 ms per tick, elapsed {elapsed}"
        );
    }
}
