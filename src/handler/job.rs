use std::task::Context;
use std::time::Duration;

use tokio::time::{Interval, MissedTickBehavior};

use crate::error::{CdpError, Result};

/// Validate before opening a socket, spawning Chrome, or constructing timers.
pub(crate) fn validate_request_timeout(timeout: Duration) -> Result<()> {
    if timeout.is_zero() || std::time::Instant::now().checked_add(timeout).is_none() {
        return Err(CdpError::msg(
            "request_timeout must be nonzero and representable as an Instant",
        ));
    }
    Ok(())
}

pub(crate) fn eviction_interval(timeout: Duration) -> Result<Interval> {
    validate_request_timeout(timeout)?;
    // Eviction compares the actual request age. A bounded tick also keeps
    // Interval's internal rescheduling arithmetic away from Instant's limit.
    let period = timeout.min(Duration::from_secs(30));
    let start = tokio::time::Instant::now()
        .checked_add(period)
        .ok_or_else(|| CdpError::msg("request_timeout deadline overflow"))?;
    let mut interval = tokio::time::interval_at(start, period);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    Ok(interval)
}

/// A background job run periodically.
///
/// Uses `tokio::time::Interval` instead of a boxed `Sleep` to avoid
/// heap indirection and manual deadline resets.
#[derive(Debug)]
pub(crate) struct PeriodicJob {
    /// The interval timer that fires periodically.
    interval: Interval,
}

impl PeriodicJob {
    /// Returns `true` if the job is currently not running but ready
    /// to be run, `false` otherwise.
    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> bool {
        self.interval.poll_tick(cx).is_ready()
    }

    pub fn new(period: Duration) -> Result<Self> {
        Ok(Self {
            interval: eviction_interval(period)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_zero_and_unrepresentable_periods_without_constructing_a_timer() {
        for timeout in [Duration::ZERO, Duration::MAX] {
            assert!(validate_request_timeout(timeout).is_err());
            assert!(PeriodicJob::new(timeout).is_err());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn valid_periods_keep_their_cadence_and_large_periods_use_bounded_ticks() {
        let mut interval = eviction_interval(Duration::from_millis(10)).unwrap();
        let start = tokio::time::Instant::now();
        interval.tick().await;
        assert_eq!(start.elapsed(), Duration::from_millis(10));
        assert_eq!(
            eviction_interval(Duration::from_secs(86400))
                .unwrap()
                .period(),
            Duration::from_secs(30)
        );
    }
}
