//! A sliding-window request counter (common/throttler.ts).

use std::collections::VecDeque;

use tokio::time::Instant;

/// Counts requests inside a moving window, so a caller can wait for a free
/// slot instead of dropping work that is over the limit.
#[derive(Debug)]
pub struct Throttler {
    requests: VecDeque<Instant>,
    max_requests: usize,
    window_ms: u128,
}

impl Throttler {
    pub fn new(max_requests: usize, window_ms: u64) -> Throttler {
        Throttler { requests: VecDeque::new(), max_requests, window_ms: u128::from(window_ms) }
    }

    fn prune(&mut self, now: Instant) {
        let window_ms = self.window_ms;
        self.requests.retain(|time| now.saturating_duration_since(*time).as_millis() < window_ms);
    }

    /// Records one request against the window.
    pub fn record(&mut self) {
        let now = Instant::now();
        self.prune(now);
        self.requests.push_back(now);
    }

    /// Milliseconds until a new request fits inside the window (0 = fits
    /// now). Does not record: a caller that gets 0 calls [`Throttler::record`]
    /// before it proceeds.
    pub fn ms_until_free(&mut self) -> u64 {
        let now = Instant::now();
        self.prune(now);
        if self.requests.len() < self.max_requests {
            return 0;
        }
        let blocking = self.requests[self.requests.len() - self.max_requests];
        let elapsed = now.saturating_duration_since(blocking).as_millis();
        self.window_ms.saturating_sub(elapsed).max(1) as u64
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_full_window_reports_the_wait_for_its_oldest_request() {
        let mut throttler = Throttler::new(2, 1000);
        assert_eq!(throttler.ms_until_free(), 0);
        throttler.record();
        tokio::time::advance(Duration::from_millis(300)).await;
        throttler.record();
        assert_eq!(throttler.ms_until_free(), 700);
        tokio::time::advance(Duration::from_millis(700)).await;
        assert_eq!(throttler.ms_until_free(), 0);
        throttler.record();
        assert_eq!(throttler.ms_until_free(), 300);
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_is_never_reported_as_zero_while_the_window_is_full() {
        let mut throttler = Throttler::new(1, 1000);
        throttler.record();
        tokio::time::advance(Duration::from_micros(999_600)).await;
        assert_eq!(throttler.ms_until_free(), 1);
    }
}
