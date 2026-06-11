//! A small standard-5-field cron evaluator on top of jiff (no chrono).
//! Fields: minute hour day-of-month month day-of-week. Supports `*`, numbers,
//! `,` lists, `-` ranges, and `/step`. Day-of-week 0 and 7 both mean Sunday.
//! "Next occurrence" is grid-aligned (jiff handles DST), so there is no drift.

use jiff::civil::Weekday;
use jiff::tz::TimeZone;
use jiff::{RoundMode, Timestamp, ToSpan, Unit, Zoned, ZonedRound};

/// Upper bound on the minute-by-minute search (just over one year) so an
/// impossible expression (e.g. Feb 30) terminates instead of looping forever.
const SEARCH_LIMIT_MINUTES: u32 = 366 * 24 * 60 + 60;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid cron expression {expr:?}: {reason}")]
pub struct CronError {
    pub expr: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    minute: u64,
    hour: u64,
    day_of_month: u64,
    month: u64,
    day_of_week: u64,
    dom_restricted: bool,
    dow_restricted: bool,
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Self, CronError> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(err(expr, "expected 5 space-separated fields"));
        }
        let (minute, _) = parse_field(expr, fields[0], 0, 59)?;
        let (hour, _) = parse_field(expr, fields[1], 0, 23)?;
        let (day_of_month, dom_restricted) = parse_field(expr, fields[2], 1, 31)?;
        let (month, _) = parse_field(expr, fields[3], 1, 12)?;
        let (mut day_of_week, dow_restricted) = parse_field(expr, fields[4], 0, 7)?;
        // Fold Sunday-as-7 into Sunday-as-0 so matching uses 0..=6.
        if day_of_week & (1 << 7) != 0 {
            day_of_week |= 1 << 0;
            day_of_week &= !(1 << 7);
        }
        Ok(Self {
            minute,
            hour,
            day_of_month,
            month,
            day_of_week,
            dom_restricted,
            dow_restricted,
        })
    }

    /// First matching instant strictly after `after`, evaluated in its time zone.
    #[must_use]
    pub fn next_after(&self, after: &Zoned) -> Option<Zoned> {
        let truncated = after
            .round(
                ZonedRound::new()
                    .smallest(Unit::Minute)
                    .mode(RoundMode::Trunc),
            )
            .ok()?;
        let mut candidate = truncated.checked_add(1.minute()).ok()?;
        for _ in 0..SEARCH_LIMIT_MINUTES {
            if self.matches(&candidate) {
                return Some(candidate);
            }
            candidate = candidate.checked_add(1.minute()).ok()?;
        }
        None
    }

    fn matches(&self, zoned: &Zoned) -> bool {
        has(self.month, u8::try_from(zoned.month()).unwrap_or(0))
            && has(self.hour, u8::try_from(zoned.hour()).unwrap_or(0))
            && has(self.minute, u8::try_from(zoned.minute()).unwrap_or(0))
            && self.day_matches(
                u8::try_from(zoned.day()).unwrap_or(0),
                weekday_sunday_zero(zoned.weekday()),
            )
    }

    /// POSIX rule: if both DOM and DOW are restricted, a day matches either;
    /// if only one is restricted, that one must match; otherwise any day.
    fn day_matches(&self, dom: u8, dow: u8) -> bool {
        let dom_ok = has(self.day_of_month, dom);
        let dow_ok = has(self.day_of_week, dow);
        match (self.dom_restricted, self.dow_restricted) {
            (false, false) => true,
            (true, false) => dom_ok,
            (false, true) => dow_ok,
            (true, true) => dom_ok || dow_ok,
        }
    }
}

/// Next occurrence after a UTC timestamp string, evaluated in `tz`, returned in
/// the host's millisecond-UTC format so it compares lexically with DB timestamps.
#[must_use]
pub fn next_after_utc(cron: &Cron, after_utc: &str, tz: &TimeZone) -> Option<String> {
    let after: Timestamp = after_utc.parse().ok()?;
    let next = cron.next_after(&after.to_zoned(tz.clone()))?;
    Some(
        next.timestamp()
            .strftime("%Y-%m-%dT%H:%M:%S.000Z")
            .to_string(),
    )
}

fn has(mask: u64, value: u8) -> bool {
    value < 64 && mask & (1 << value) != 0
}

fn weekday_sunday_zero(weekday: Weekday) -> u8 {
    u8::try_from(weekday.to_sunday_zero_offset()).unwrap_or(0)
}

/// Returns the value mask and whether the field was restricted (anything but `*`).
fn parse_field(expr: &str, spec: &str, min: u8, max: u8) -> Result<(u64, bool), CronError> {
    if spec == "*" {
        return Ok((range_mask(min, max, 1), false));
    }
    let mut bits = 0u64;
    for part in spec.split(',') {
        let (range_spec, step) = match part.split_once('/') {
            Some((range, step)) => (
                range,
                step.parse::<u8>()
                    .ok()
                    .filter(|step| *step > 0)
                    .ok_or_else(|| err(expr, "step must be a positive integer"))?,
            ),
            None => (part, 1),
        };
        let (lo, hi) = if range_spec == "*" {
            (min, max)
        } else if let Some((start, end)) = range_spec.split_once('-') {
            (
                parse_value(expr, start, min, max)?,
                parse_value(expr, end, min, max)?,
            )
        } else {
            let value = parse_value(expr, range_spec, min, max)?;
            // `N/step` means from N to the field max; a bare `N` is a single value.
            if step == 1 {
                (value, value)
            } else {
                (value, max)
            }
        };
        if lo > hi {
            return Err(err(expr, "range start is after its end"));
        }
        bits |= range_mask(lo, hi, step);
    }
    Ok((bits, true))
}

fn parse_value(expr: &str, token: &str, min: u8, max: u8) -> Result<u8, CronError> {
    let value: u8 = token
        .parse()
        .map_err(|_| err(expr, "field value is not an integer"))?;
    if value < min || value > max {
        return Err(err(expr, "field value is out of range"));
    }
    Ok(value)
}

fn range_mask(lo: u8, hi: u8, step: u8) -> u64 {
    let mut bits = 0u64;
    let mut value = lo;
    while value <= hi {
        bits |= 1 << value;
        value += step;
    }
    bits
}

fn err(expr: &str, reason: &str) -> CronError {
    CronError {
        expr: expr.to_owned(),
        reason: reason.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc() -> TimeZone {
        TimeZone::UTC
    }

    fn next(expr: &str, after: &str) -> String {
        let cron = Cron::parse(expr).expect("parse");
        next_after_utc(&cron, after, &utc()).expect("next")
    }

    #[test]
    fn daily_at_nine_is_grid_aligned_regardless_of_completion_latency() {
        // Completed five minutes late; next fire stays on the 09:00 grid (no drift).
        assert_eq!(
            next("0 9 * * *", "2026-06-11T09:05:00.000Z"),
            "2026-06-12T09:00:00.000Z"
        );
    }

    #[test]
    fn every_five_minutes_skips_missed_windows() {
        // A run that overran to 12:07 jumps to 12:10, not back to the missed 12:05.
        assert_eq!(
            next("*/5 * * * *", "2026-06-11T12:07:30.000Z"),
            "2026-06-11T12:10:00.000Z"
        );
    }

    #[test]
    fn hourly_on_the_hour() {
        assert_eq!(
            next("0 * * * *", "2026-06-11T12:00:00.000Z"),
            "2026-06-11T13:00:00.000Z"
        );
    }

    #[test]
    fn lists_and_ranges() {
        let cron = Cron::parse("0 9,17 * * 1-5").expect("parse");
        // Friday 17:00 → next is the 09:00 slot, but Saturday/Sunday are skipped to Monday 09:00.
        let after = "2026-06-12T17:00:00.000Z"; // 2026-06-12 is a Friday
        assert_eq!(
            next_after_utc(&cron, after, &utc()).expect("next"),
            "2026-06-15T09:00:00.000Z" // Monday
        );
    }

    #[test]
    fn dom_and_dow_both_restricted_matches_either() {
        // "1st of the month OR any Monday" — POSIX OR semantics.
        let cron = Cron::parse("0 0 1 * 1").expect("parse");
        // From mid-June 2026: next Monday 0:00 comes before the 1st of July.
        let next = cron
            .next_after(
                &"2026-06-10T00:00:00Z"
                    .parse::<Timestamp>()
                    .unwrap()
                    .to_zoned(utc()),
            )
            .expect("next");
        assert_eq!(
            next.timestamp().strftime("%Y-%m-%d").to_string(),
            "2026-06-15" // Monday 2026-06-15, earlier than July 1
        );
    }

    #[test]
    fn timezone_is_respected() {
        let cron = Cron::parse("0 9 * * *").expect("parse");
        let tz = TimeZone::get("America/New_York").expect("tz");
        // 09:00 New York on 2026-06-11 is 13:00 UTC (EDT, UTC-4).
        let result = next_after_utc(&cron, "2026-06-11T00:00:00.000Z", &tz).expect("next");
        assert_eq!(result, "2026-06-11T13:00:00.000Z");
    }

    #[test]
    fn malformed_expressions_are_rejected() {
        for bad in [
            "",
            "* * * *",
            "60 * * * *",
            "* * * * 8/x",
            "0-99 * * * *",
            "5-1 * * * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn sunday_accepts_zero_and_seven() {
        let zero = Cron::parse("0 0 * * 0").expect("parse");
        let seven = Cron::parse("0 0 * * 7").expect("parse");
        assert_eq!(zero.day_of_week, seven.day_of_week);
    }
}
