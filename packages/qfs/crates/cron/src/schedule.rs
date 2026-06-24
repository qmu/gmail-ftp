//! [`Schedule`] — the cadence of a JOB (RFD §8: `EVERY <interval>` / a restricted 5-field cron).
//!
//! `Schedule::next_after(from)` is the **pure** schedule math the due-set computation drives. It
//! takes the project's standard epoch-seconds [`Instant`] and returns the next fire boundary
//! strictly after `from` (or `None` for a cron that never matches in a bounded look-ahead).
//!
//! Cron expressions are parsed + validated **at load time** ([`CronExpr::parse`]) into a
//! structured form; an invalid expression is a structured [`ScheduleError`], never a panic — the
//! same discipline as the parser's other load-time errors.

use serde::{Deserialize, Serialize};

/// The project's standard instant: an epoch **second** (matches `JobDef.last_run` and
/// `qfs_core::Value::Timestamp`). No `chrono`/vendor type — the scheduler is time-type-frugal.
pub type Instant = i64;

/// A duration as the project's standard type: a count of seconds (the `EVERY <interval>` payload).
pub type Seconds = i64;

/// The cadence of a JOB: a fixed interval anchored at an epoch, or a restricted 5-field cron.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Schedule {
    /// Fire every `interval` seconds, anchored at `anchor` (the first boundary is the first
    /// `anchor + n*interval` strictly after `from`). `anchor` defaults to epoch 0.
    Every {
        /// The interval in seconds (must be > 0; validated at construction).
        interval: Seconds,
        /// The anchor epoch the `n*interval` boundaries are measured from.
        anchor: Instant,
    },
    /// A restricted 5-field cron (`min hour dom mon dow`), UTC.
    Cron(CronExpr),
}

impl Schedule {
    /// Construct an `EVERY <interval>` schedule anchored at epoch 0.
    ///
    /// # Errors
    /// [`ScheduleError::ZeroInterval`] if `interval <= 0` (a non-advancing cadence).
    pub fn every(interval: Seconds) -> Result<Self, ScheduleError> {
        if interval <= 0 {
            return Err(ScheduleError::ZeroInterval);
        }
        Ok(Schedule::Every {
            interval,
            anchor: 0,
        })
    }

    /// Construct an `EVERY <interval>` schedule anchored at `anchor`.
    ///
    /// # Errors
    /// [`ScheduleError::ZeroInterval`] if `interval <= 0`.
    pub fn every_anchored(interval: Seconds, anchor: Instant) -> Result<Self, ScheduleError> {
        if interval <= 0 {
            return Err(ScheduleError::ZeroInterval);
        }
        Ok(Schedule::Every { interval, anchor })
    }

    /// Parse a restricted 5-field cron expression at load time.
    ///
    /// # Errors
    /// [`ScheduleError`] if the expression is not a valid restricted cron (wrong field count,
    /// out-of-range value, malformed range/step).
    pub fn cron(expr: &str) -> Result<Self, ScheduleError> {
        Ok(Schedule::Cron(CronExpr::parse(expr)?))
    }

    /// The next fire boundary strictly after `from`, or `None` if there is none within a bounded
    /// look-ahead (only possible for a pathological cron — `Every` always advances).
    #[must_use]
    pub fn next_after(&self, from: Instant) -> Option<Instant> {
        match self {
            Schedule::Every { interval, anchor } => {
                // The first anchor + n*interval strictly greater than `from`.
                let delta = from - anchor;
                let n = if delta < 0 { 0 } else { delta / interval + 1 };
                Some(anchor + n * interval)
            }
            Schedule::Cron(c) => c.next_after(from),
        }
    }

    /// The most recent fire boundary **at or before** `now` — the first-eligibility boundary a
    /// JOB that has never run should fire on. Derived FROM the schedule (not a fixed window).
    ///
    /// For `Every`, the exact arithmetic boundary `anchor + n*interval ≤ now` (works for ANY
    /// interval, including one larger than a day, so an `EVERY '7d'` job fires on first eligibility
    /// rather than deferring up to a full interval — the Obs-3 correctness fix). For `Cron`, the
    /// prior matching minute, scanned back over a bounded look-back.
    ///
    /// Returns `None` if no boundary is at-or-before `now` (the anchor is still in the future, or
    /// the cron has no match in the bounded look-back).
    #[must_use]
    pub fn prev_at_or_before(&self, now: Instant) -> Option<Instant> {
        match self {
            Schedule::Every { interval, anchor } => {
                if now < *anchor {
                    // The cadence has not started yet; nothing is due.
                    return None;
                }
                // The largest anchor + n*interval ≤ now (exact; no fixed-window cap).
                let n = (now - anchor) / interval;
                Some(anchor + n * interval)
            }
            Schedule::Cron(c) => c.prev_at_or_before(now),
        }
    }
}

/// A restricted 5-field cron expression (`min hour dom mon dow`), all UTC. Each field is a
/// [`CronField`]; matching is minute-granular (the smallest cron unit).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronExpr {
    /// Minute, 0..=59.
    pub minute: CronField,
    /// Hour, 0..=23.
    pub hour: CronField,
    /// Day-of-month, 1..=31.
    pub dom: CronField,
    /// Month, 1..=12.
    pub month: CronField,
    /// Day-of-week, 0..=6 (0 = Sunday).
    pub dow: CronField,
}

/// One cron field: an explicit allowed-value set over the field's domain. `*` expands to the whole
/// domain; `a,b`, `a-b`, `*/step` all expand into the same set at parse time, so matching is a
/// single membership test (no per-tick re-parse).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronField {
    /// The allowed values in this field's domain (sorted, deduplicated).
    allowed: Vec<u32>,
}

impl CronField {
    fn parse(spec: &str, min: u32, max: u32, field: &'static str) -> Result<Self, ScheduleError> {
        let mut set: Vec<u32> = Vec::new();
        for part in spec.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(ScheduleError::BadField {
                    field,
                    value: spec.to_string(),
                });
            }
            // `*/step` or `*`
            if let Some(step_str) = part.strip_prefix("*/") {
                let step = parse_u32(step_str, field, spec)?;
                if step == 0 {
                    return Err(ScheduleError::BadField {
                        field,
                        value: spec.to_string(),
                    });
                }
                let mut v = min;
                while v <= max {
                    set.push(v);
                    v += step;
                }
                continue;
            }
            if part == "*" {
                for v in min..=max {
                    set.push(v);
                }
                continue;
            }
            // `a-b`
            if let Some((lo, hi)) = part.split_once('-') {
                let lo = parse_u32(lo, field, spec)?;
                let hi = parse_u32(hi, field, spec)?;
                if lo > hi || lo < min || hi > max {
                    return Err(ScheduleError::OutOfRange {
                        field,
                        value: part.to_string(),
                        min,
                        max,
                    });
                }
                for v in lo..=hi {
                    set.push(v);
                }
                continue;
            }
            // `n`
            let v = parse_u32(part, field, spec)?;
            if v < min || v > max {
                return Err(ScheduleError::OutOfRange {
                    field,
                    value: part.to_string(),
                    min,
                    max,
                });
            }
            set.push(v);
        }
        set.sort_unstable();
        set.dedup();
        if set.is_empty() {
            return Err(ScheduleError::BadField {
                field,
                value: spec.to_string(),
            });
        }
        Ok(CronField { allowed: set })
    }

    fn matches(&self, value: u32) -> bool {
        self.allowed.binary_search(&value).is_ok()
    }
}

fn parse_u32(s: &str, field: &'static str, full: &str) -> Result<u32, ScheduleError> {
    s.trim()
        .parse::<u32>()
        .map_err(|_| ScheduleError::BadField {
            field,
            value: full.to_string(),
        })
}

impl CronExpr {
    /// Parse a restricted 5-field cron (`min hour dom mon dow`).
    ///
    /// # Errors
    /// [`ScheduleError`] on the wrong number of fields, an out-of-range value, or a malformed
    /// range/step.
    pub fn parse(expr: &str) -> Result<Self, ScheduleError> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(ScheduleError::FieldCount {
                got: fields.len(),
                expr: expr.to_string(),
            });
        }
        Ok(CronExpr {
            minute: CronField::parse(fields[0], 0, 59, "minute")?,
            hour: CronField::parse(fields[1], 0, 23, "hour")?,
            dom: CronField::parse(fields[2], 1, 31, "day-of-month")?,
            month: CronField::parse(fields[3], 1, 12, "month")?,
            dow: CronField::parse(fields[4], 0, 6, "day-of-week")?,
        })
    }

    /// The next minute boundary strictly after `from` (epoch seconds) matching this expression.
    /// Scans forward minute-by-minute over a bounded look-ahead (4 years) so a never-matching
    /// expression terminates with `None` rather than looping forever.
    #[must_use]
    pub fn next_after(&self, from: Instant) -> Option<Instant> {
        const SECS_PER_MIN: i64 = 60;
        // Bounded look-ahead: 4 years of minutes covers any Feb-29 / leap interaction.
        const MAX_MINUTES: i64 = 4 * 366 * 24 * 60;
        // Advance to the start of the next whole minute strictly after `from`.
        let mut t = (from / SECS_PER_MIN + 1) * SECS_PER_MIN;
        let mut scanned = 0;
        while scanned < MAX_MINUTES {
            let parts = civil_from_epoch_secs(t);
            if self.minute.matches(parts.minute)
                && self.hour.matches(parts.hour)
                && self.dom.matches(parts.day)
                && self.month.matches(parts.month)
                && self.dow.matches(parts.dow)
            {
                return Some(t);
            }
            t += SECS_PER_MIN;
            scanned += 1;
        }
        None
    }

    /// The most recent minute boundary **at or before** `now` matching this expression. Scans
    /// backward minute-by-minute over a bounded look-back (4 years) so a never-matching expression
    /// terminates with `None`. The cron analogue of [`Schedule::prev_at_or_before`].
    #[must_use]
    pub fn prev_at_or_before(&self, now: Instant) -> Option<Instant> {
        const SECS_PER_MIN: i64 = 60;
        const MAX_MINUTES: i64 = 4 * 366 * 24 * 60;
        // Start at the whole minute at-or-before `now`.
        let mut t = (now / SECS_PER_MIN) * SECS_PER_MIN;
        let mut scanned = 0;
        while scanned < MAX_MINUTES {
            let parts = civil_from_epoch_secs(t);
            if self.minute.matches(parts.minute)
                && self.hour.matches(parts.hour)
                && self.dom.matches(parts.day)
                && self.month.matches(parts.month)
                && self.dow.matches(parts.dow)
            {
                return Some(t);
            }
            t -= SECS_PER_MIN;
            scanned += 1;
        }
        None
    }
}

/// A civil (UTC) breakdown of an epoch instant, for cron matching.
struct Civil {
    minute: u32,
    hour: u32,
    day: u32,
    month: u32,
    dow: u32,
}

/// Convert epoch seconds (UTC) to a civil breakdown. Uses Howard Hinnant's well-known
/// `days_from_civil` inverse (`civil_from_days`) — a small, exact, dependency-free conversion
/// (no chrono). Valid for the whole proleptic Gregorian calendar; the scheduler only feeds it
/// non-negative epochs.
fn civil_from_epoch_secs(secs: i64) -> Civil {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let hour = (secs_of_day / 3600) as u32;
    let minute = ((secs_of_day % 3600) / 60) as u32;

    // Day of week: 1970-01-01 was a Thursday (4). Sunday = 0.
    let dow = (((days % 7) + 4).rem_euclid(7)) as u32;

    // civil_from_days (Hinnant): days since 1970-01-01 -> (y, m, d).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    let _ = year; // year is not needed for cron matching (no year field), but documents the form.

    Civil {
        minute,
        hour,
        day: d as u32,
        month: m as u32,
        dow,
    }
}

/// A structured, load-time schedule parse error (never a panic). Secret-free.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ScheduleError {
    /// `EVERY <interval>` with a non-positive interval (a cadence that never advances).
    #[error("EVERY interval must be a positive number of seconds")]
    ZeroInterval,
    /// A cron expression with the wrong number of fields (a restricted cron has exactly 5).
    #[error("cron must have exactly 5 fields (min hour dom mon dow), got {got}: {expr:?}")]
    FieldCount {
        /// The number of whitespace-separated fields actually present.
        got: usize,
        /// The offending expression.
        expr: String,
    },
    /// A cron field value outside the field's allowed domain.
    #[error("cron {field} value {value:?} out of range {min}..={max}")]
    OutOfRange {
        /// The field name (`minute`/`hour`/…).
        field: &'static str,
        /// The offending value text.
        value: String,
        /// The inclusive lower bound of the field domain.
        min: u32,
        /// The inclusive upper bound of the field domain.
        max: u32,
    },
    /// A malformed cron field (empty, non-numeric, or a bad range/step).
    #[error("cron {field} field is malformed: {value:?}")]
    BadField {
        /// The field name.
        field: &'static str,
        /// The offending field text.
        value: String,
    },
}
