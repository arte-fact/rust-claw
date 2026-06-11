//! Run-failure recovery policy: retry backoff and the no-activity watchdog.
//! The decisions are pure; the supervisor applies them.

use std::time::Duration;

use jiff::{Timestamp, ToSpan};

/// After this many attempts a message is abandoned (status `failed`).
pub const MAX_TRIES: i64 = 5;
/// A run that emits no provider event for this long is presumed stuck and aborted.
pub const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Give up — mark the message `failed`.
    Fail,
    /// Reschedule the message this far in the future (exponential backoff).
    After(Duration),
}

/// `tries` is the attempt count *after* incrementing for the failure just seen.
/// 1→5s, 2→10s, 3→20s, 4→40s, then `>= MAX_TRIES` → fail.
#[must_use]
pub fn retry_decision(tries: i64, max_tries: i64) -> Retry {
    if tries >= max_tries {
        return Retry::Fail;
    }
    let exponent = u32::try_from((tries - 1).max(0)).unwrap_or(0);
    Retry::After(Duration::from_secs(5 * 2u64.pow(exponent)))
}

/// `now` (a UTC DB timestamp) plus `seconds`, in the host's millisecond-UTC
/// format so it compares lexically with other DB timestamps.
#[must_use]
pub fn add_seconds_utc(now: &str, seconds: u64) -> Option<String> {
    let timestamp: Timestamp = now.parse().ok()?;
    let next = timestamp
        .checked_add(i64::try_from(seconds).ok()?.seconds())
        .ok()?;
    Some(next.strftime("%Y-%m-%dT%H:%M:%S.000Z").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_then_fails() {
        assert_eq!(
            retry_decision(1, MAX_TRIES),
            Retry::After(Duration::from_secs(5))
        );
        assert_eq!(
            retry_decision(2, MAX_TRIES),
            Retry::After(Duration::from_secs(10))
        );
        assert_eq!(
            retry_decision(3, MAX_TRIES),
            Retry::After(Duration::from_secs(20))
        );
        assert_eq!(
            retry_decision(4, MAX_TRIES),
            Retry::After(Duration::from_secs(40))
        );
        assert_eq!(retry_decision(5, MAX_TRIES), Retry::Fail);
        assert_eq!(retry_decision(99, MAX_TRIES), Retry::Fail);
    }

    #[test]
    fn add_seconds_advances_and_keeps_db_format() {
        assert_eq!(
            add_seconds_utc("2026-06-11T12:00:00.000Z", 40).as_deref(),
            Some("2026-06-11T12:00:40.000Z")
        );
        // Rolls over minutes/hours correctly.
        assert_eq!(
            add_seconds_utc("2026-06-11T12:59:50.000Z", 20).as_deref(),
            Some("2026-06-11T13:00:10.000Z")
        );
        assert_eq!(add_seconds_utc("not a timestamp", 5), None);
    }
}
