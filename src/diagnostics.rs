//! The one number every diagnostic derives its timing from.
//!
//! The slicer reports a statistics interval, the telemetry task samples the
//! GPU, the processes and the pipeline, and alignment judges the phase of the
//! displays, all on the same beat. Keeping that beat in one constant means the
//! charts line up, and every judgment expressed in wall-clock time (how long
//! out of phase before aligning, how much history to keep, when a value is
//! stale) keeps its meaning when the interval changes. Derive from
//! [`SAMPLE_INTERVAL`]; never write the number down again.

use std::time::Duration;

/// How often every diagnostic is sampled, and the length of one slicer
/// statistics interval. One number so charts line up and judgments
/// expressed in wall-clock time stay the same when it changes.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// How much history each panel keeps.
pub const HISTORY_WINDOW: Duration = Duration::from_secs(300);

/// Points a history buffer holds: [`HISTORY_WINDOW`] / [`SAMPLE_INTERVAL`].
pub const fn history_capacity() -> usize {
    (HISTORY_WINDOW.as_millis() / SAMPLE_INTERVAL.as_millis()) as usize
}

/// How many consecutive samples cover `window` (rounded up, at least 1).
pub const fn samples_in(window: Duration) -> u32 {
    let interval = SAMPLE_INTERVAL.as_millis();
    let count = window.as_millis().div_ceil(interval) as u32;
    if count == 0 {
        1
    } else {
        count
    }
}

/// After this long without a new sample a value is stale: 2.5 intervals,
/// which tolerates two late reports.
pub const fn stale_after() -> Duration {
    Duration::from_millis((SAMPLE_INTERVAL.as_millis() * 5 / 2) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Written out on purpose: changing the interval must be a deliberate
    /// edit of these numbers too.
    #[test]
    fn derived_values_at_five_seconds() {
        assert_eq!(SAMPLE_INTERVAL, Duration::from_secs(5));
        assert_eq!(history_capacity(), 60);
        assert_eq!(samples_in(Duration::from_secs(20)), 4);
        assert_eq!(stale_after(), Duration::from_millis(12_500));
    }

    #[test]
    fn samples_in_rounds_up_and_is_at_least_one() {
        assert_eq!(samples_in(Duration::ZERO), 1);
        assert_eq!(samples_in(Duration::from_secs(1)), 1);
        assert_eq!(samples_in(SAMPLE_INTERVAL), 1);
        assert_eq!(samples_in(SAMPLE_INTERVAL + Duration::from_millis(1)), 2);
    }
}
