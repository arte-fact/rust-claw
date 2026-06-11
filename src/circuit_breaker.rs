//! Crash-loop guard: if claw keeps dying and restarting quickly, back off before
//! each restart instead of spinning. A marker file in the data dir records the
//! attempt count and last start; a clean shutdown clears it (next start = attempt 1).

use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::{Span, Timestamp, ToSpan};
use serde::{Deserialize, Serialize};

/// Restarts within this window of the previous start count as a crash loop.
const RESET_WINDOW_SECS: i64 = 3600;
const MARKER_FILE: &str = "circuit-breaker.json";

#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    attempt: u32,
    last_start: String,
}

pub struct CircuitBreaker {
    path: PathBuf,
}

impl CircuitBreaker {
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(MARKER_FILE),
        }
    }

    /// Records this start and returns how long the caller should sleep before
    /// proceeding. A fast restart bumps the attempt count; a start long after the
    /// previous one resets to attempt 1.
    pub fn record_start(&self, now: Timestamp) -> Duration {
        let previous = self.read();
        let attempt = match previous {
            Some(marker) if recent(&marker.last_start, now) => marker.attempt.saturating_add(1),
            _ => 1,
        };
        self.write(&Marker {
            attempt,
            last_start: now.strftime("%Y-%m-%dT%H:%M:%S.000Z").to_string(),
        });
        crash_backoff(attempt)
    }

    /// Clean shutdown — drop the marker so the next start begins at attempt 1.
    pub fn clear(&self) {
        let _ = std::fs::remove_file(&self.path);
    }

    fn read(&self) -> Option<Marker> {
        let raw = std::fs::read_to_string(&self.path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    fn write(&self, marker: &Marker) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(encoded) = serde_json::to_string(marker) {
            let _ = std::fs::write(&self.path, encoded);
        }
    }
}

/// Backoff schedule by attempt: 1,2 → 0; 3 → 10s; 4 → 30s; 5 → 2m; 6 → 5m; 7+ → 15m.
#[must_use]
pub fn crash_backoff(attempt: u32) -> Duration {
    match attempt {
        0..=2 => Duration::ZERO,
        3 => Duration::from_secs(10),
        4 => Duration::from_secs(30),
        5 => Duration::from_secs(120),
        6 => Duration::from_secs(300),
        _ => Duration::from_secs(900),
    }
}

fn recent(last_start: &str, now: Timestamp) -> bool {
    let Ok(previous) = last_start.parse::<Timestamp>() else {
        return false;
    };
    now.since(previous)
        .map(|span: Span| span.compare(RESET_WINDOW_SECS.seconds()).map(|o| o.is_lt()))
        .unwrap_or(Ok(false))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(iso: &str) -> Timestamp {
        iso.parse().expect("timestamp")
    }

    #[test]
    fn backoff_schedule() {
        assert_eq!(crash_backoff(1), Duration::ZERO);
        assert_eq!(crash_backoff(2), Duration::ZERO);
        assert_eq!(crash_backoff(3), Duration::from_secs(10));
        assert_eq!(crash_backoff(5), Duration::from_secs(120));
        assert_eq!(crash_backoff(100), Duration::from_secs(900));
    }

    #[test]
    fn fast_restarts_escalate_and_a_gap_resets() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let breaker = CircuitBreaker::new(tmp.path());
        let base = at("2026-06-11T12:00:00Z");

        assert_eq!(breaker.record_start(base), Duration::ZERO); // attempt 1
        assert_eq!(
            breaker.record_start(base.checked_add(1.second()).unwrap()),
            Duration::ZERO // attempt 2
        );
        assert_eq!(
            breaker.record_start(base.checked_add(2.seconds()).unwrap()),
            Duration::from_secs(10) // attempt 3
        );

        // A start two hours later is outside the reset window → back to attempt 1.
        assert_eq!(
            breaker.record_start(base.checked_add(2.hours()).unwrap()),
            Duration::ZERO
        );
    }

    #[test]
    fn clear_resets_the_counter() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let breaker = CircuitBreaker::new(tmp.path());
        let now = at("2026-06-11T12:00:00Z");

        breaker.record_start(now);
        breaker.record_start(now.checked_add(1.second()).unwrap());
        breaker.clear();
        // After a clean shutdown the next start is attempt 1 again.
        assert_eq!(
            breaker.record_start(now.checked_add(2.seconds()).unwrap()),
            Duration::ZERO
        );
    }
}
