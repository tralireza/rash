//! Restart backoff — a port of autossh's `grace_time()` (autossh.c:1115-1154).
//!
//! Restarts that keep failing quickly are spaced further and further apart, up
//! to the poll interval. A session that stays up long enough resets the count.
//!
//! The arithmetic is deliberately kept in `f64` with a truncating cast, exactly
//! as the C computes it, so the two agree second for second.

use std::time::Duration;

/// `N_FAST_TRIES` (autossh.c:110): this many quick retries before any delay.
const FAST_TRIES: u32 = 5;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    tries: u32,
}

impl Backoff {
    /// Record a restart whose predecessor ran for `uptime`, and say how long to
    /// wait before starting the next one.
    ///
    /// For the very first start there is no predecessor; pass `Duration::MAX`.
    pub fn next_delay(&mut self, uptime: Duration, poll: Duration) -> Duration {
        let poll_secs = poll.as_secs();

        // Stay up for a tenth of the poll interval — at least 10s — and the
        // count resets. Integer division, as in the C.
        let min_time = (poll_secs / 10).max(10);
        if uptime.as_secs() >= min_time {
            self.tries = 0;
        } else {
            self.tries += 1;
        }

        if self.tries <= FAST_TRIES {
            return Duration::ZERO;
        }

        // interval = (poll / 100) * t^2 / 3, capped at poll. The C truncates to
        // int before comparing against the cap, so this does too.
        let t = f64::from(self.tries - FAST_TRIES);
        let n = ((poll_secs as f64 / 100.0) * (t * (t / 3.0))) as u64;
        Duration::from_secs(n.min(poll_secs))
    }

    /// How many quick restarts in a row have been seen.
    pub fn tries(&self) -> u32 {
        self.tries
    }
}
