//! How many commits a session made, and how long each took from press to click.
//!
//! Until 2026-09-10 this also graded every commit against the synthetic provider's
//! noise-free point (hit, slip, miss); with the real tracker the only source there is
//! nothing to grade against, so what is left is the count and the latency.

use std::time::Duration;

/// Running tally over a session, printed as the exit summary.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scoreboard {
    pub commits : u64,
    /// Sum of press-to-click-issue latencies. Kept as a sum so the mean stays exact
    /// regardless of how many commits arrive.
    latency_s   : f64,
}

// --- Scoreboard ---

impl Scoreboard {
    /// Adds one commit.
    pub fn record(&mut self, latency: Duration) {
        self.commits   += 1;
        self.latency_s += latency.as_secs_f64();
    }

    /// Mean press-to-click-issue latency in seconds, or `0.0` with no commits.
    pub fn mean_latency_s(&self) -> f64 {
        if self.commits == 0 {
            return 0.0;
        }

        self.latency_s / self.commits as f64
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mean_latency_is_exact_over_the_commits() {
        let mut board = Scoreboard::default();

        assert_eq!(board.mean_latency_s(), 0.0);

        board.record(Duration::from_millis(10));
        board.record(Duration::from_millis(30));

        assert_eq!(board.commits, 2);
        assert!((board.mean_latency_s() - 0.02).abs() < 1e-12);
    }
}
