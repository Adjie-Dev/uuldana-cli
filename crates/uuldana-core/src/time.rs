//! Minimal wall-clock timestamp type.
//!
//! `uuldana-core` does not depend on a date/time crate. Timestamps are stored
//! as milliseconds since the Unix epoch, which is enough for ordering events
//! and recording limit reset times.
//!
//! The core never reads the system clock (ADR 0001). The runtime acquires
//! the time at its boundary, for example
//! `Timestamp::from_system_time(SystemTime::now())`, and passes it in.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch (UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timestamp {
    unix_millis: u64,
}

impl Timestamp {
    /// Creates a timestamp from milliseconds since the Unix epoch.
    pub const fn from_unix_millis(unix_millis: u64) -> Self {
        Self { unix_millis }
    }

    /// Converts a `SystemTime` value. This is a pure conversion and does not
    /// read the clock. Times before the Unix epoch saturate to the epoch;
    /// times beyond `u64::MAX` milliseconds saturate to `u64::MAX`.
    pub fn from_system_time(time: SystemTime) -> Self {
        let millis = time
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        Self::from_unix_millis(millis)
    }

    /// Returns milliseconds since the Unix epoch.
    pub const fn unix_millis(self) -> u64 {
        self.unix_millis
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn round_trips_millis_and_orders() {
        let a = Timestamp::from_unix_millis(1_000);
        let b = Timestamp::from_unix_millis(2_000);
        assert_eq!(a.unix_millis(), 1_000);
        assert!(a < b);
    }

    #[test]
    fn converts_fixed_system_times() {
        assert_eq!(Timestamp::from_system_time(UNIX_EPOCH).unix_millis(), 0);
        let later = UNIX_EPOCH + Duration::from_millis(1_234_567);
        assert_eq!(Timestamp::from_system_time(later).unix_millis(), 1_234_567);
        // Sub-millisecond precision is truncated, not rounded.
        let fractional = UNIX_EPOCH + Duration::from_micros(2_999);
        assert_eq!(Timestamp::from_system_time(fractional).unix_millis(), 2);
    }

    #[test]
    fn pre_epoch_time_saturates_to_epoch() {
        // Not every platform can represent times before 1970.
        if let Some(before) = UNIX_EPOCH.checked_sub(Duration::from_secs(60)) {
            assert_eq!(Timestamp::from_system_time(before).unix_millis(), 0);
        }
    }
}
