use crate::{Error, Result};
use chrono::{DateTime, Duration, LocalResult, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Timing {
    Once {
        at: String,
    },
    /// Resolved once at creation/update; never restarted after a process restart.
    After {
        seconds: u32,
    },
    Interval {
        anchor: i64,
        every_seconds: u32,
    },
    Cron {
        expression: String,
        timezone: String,
    },
}
impl Timing {
    pub fn resolve(&self, now: i64) -> Result<Self> {
        match self {
            Self::After { seconds } if *seconds > 0 => {
                let at = now
                    .checked_add(i64::from(*seconds))
                    .and_then(|t| DateTime::from_timestamp(t, 0))
                    .ok_or(Error::Invalid("delay out of range"))?;
                Ok(Self::Once {
                    at: at.to_rfc3339(),
                })
            }
            Self::After { .. } => Err(Error::Invalid("delay must be positive")),
            _ => Ok(self.clone()),
        }
    }
    pub fn next_after(&self, after: i64) -> Result<Option<i64>> {
        let time = timestamp(after)?;
        match self {
            Self::Once { at } => {
                let at = once(at)?;
                Ok((at > after).then_some(at))
            }
            Self::After { .. } => Err(Error::Invalid("resolve relative delay before calculating")),
            Self::Interval {
                anchor,
                every_seconds,
            } => {
                timestamp(*anchor)?;
                if *every_seconds == 0 {
                    return Err(Error::Invalid("interval must be positive"));
                }
                let next = if *anchor > after {
                    *anchor
                } else {
                    let n = after
                        .checked_sub(*anchor)
                        .and_then(|d| d.checked_div(i64::from(*every_seconds)))
                        .and_then(|n| n.checked_add(1))
                        .ok_or(Error::Invalid("interval out of range"))?;
                    anchor
                        .checked_add(
                            n.checked_mul(i64::from(*every_seconds))
                                .ok_or(Error::Invalid("interval out of range"))?,
                        )
                        .ok_or(Error::Invalid("interval out of range"))?
                };
                timestamp(next)?;
                Ok(Some(next))
            }
            Self::Cron {
                expression,
                timezone,
            } => {
                let (cron, tz) = parse_cron(expression, timezone)?;
                cron_occurrence(&cron, tz, time, false)
            }
        }
    }
    pub(crate) fn at_or_before(&self, now: i64) -> Result<Option<i64>> {
        match self {
            Self::Cron {
                expression,
                timezone,
            } => {
                let (cron, tz) = parse_cron(expression, timezone)?;
                cron_occurrence(&cron, tz, timestamp(now)?, true)
            }
            Self::Interval {
                anchor,
                every_seconds,
            } => {
                self.next_after(now)?;
                Ok((now >= *anchor).then(|| {
                    anchor + (now - anchor) / i64::from(*every_seconds) * i64::from(*every_seconds)
                }))
            }
            Self::Once { at } => {
                let at = once(at)?;
                Ok((at <= now).then_some(at))
            }
            Self::After { .. } => Err(Error::Invalid("unresolved delay")),
        }
    }
}

// Evaluate wall-clock candidates without delegating DST to the cron parser.
// Missing wall times are skipped; repeated wall times use the earlier instant
// once. Compare UTC bounds after conversion so the cursor is strictly monotonic
// even when a service starts inside the second copy of a repeated hour.
fn cron_occurrence(
    cron: &croner::Cron,
    tz: Tz,
    bound: DateTime<Utc>,
    backward: bool,
) -> Result<Option<i64>> {
    let mut cursor = bound.with_timezone(&tz).naive_local().and_utc();
    if backward {
        if let LocalResult::Ambiguous(early, late) = tz.from_local_datetime(&cursor.naive_utc()) {
            if bound.timestamp() >= late.timestamp() {
                // In the second copy of a repeated hour, later wall-clock
                // slots from the first copy are still earlier UTC instants.
                cursor = cursor
                    .checked_add_signed(Duration::seconds(late.timestamp() - early.timestamp()))
                    .ok_or(Error::Invalid("timezone cursor out of range"))?;
            }
        }
    }
    let mut inclusive = backward;
    for _ in 0..4096 {
        let found = if backward {
            cron.find_previous_occurrence(&cursor, inclusive)
        } else {
            cron.find_next_occurrence(&cursor, inclusive)
        };
        let candidate = match found {
            Ok(candidate) => candidate,
            Err(_) => return Ok(None),
        };
        cursor = candidate;
        inclusive = false;
        let local = match tz.from_local_datetime(&candidate.naive_utc()) {
            LocalResult::Single(time) => time,
            LocalResult::Ambiguous(early, _) => early,
            LocalResult::None => continue,
        };
        let at = local.timestamp();
        if (backward && at <= bound.timestamp()) || (!backward && at > bound.timestamp()) {
            return Ok(Some(at));
        }
    }
    Err(Error::Invalid("cron timezone search exceeded its bound"))
}
fn timestamp(t: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp(t, 0).ok_or(Error::Invalid("timestamp out of range"))
}
fn once(at: &str) -> Result<i64> {
    if at.len() > 64 {
        return Err(Error::Invalid("invalid RFC3339 time"));
    }
    DateTime::parse_from_rfc3339(at)
        .map(|t| t.timestamp())
        .map_err(|_| Error::Invalid("once requires RFC3339 with offset"))
}
fn parse_cron(expression: &str, timezone: &str) -> Result<(croner::Cron, Tz)> {
    if expression.len() > 128 || expression.split_whitespace().count() != 5 {
        return Err(Error::Invalid("cron requires five fields"));
    }
    if timezone.len() > 64 {
        return Err(Error::Invalid("invalid timezone"));
    }
    let tz = Tz::from_str(timezone).map_err(|_| Error::Invalid("unknown IANA timezone"))?;
    let cron = croner::Cron::from_str(expression)
        .map_err(|_| Error::Invalid("invalid cron expression"))?;
    Ok((cron, tz))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn utc(text: &str) -> i64 {
        DateTime::parse_from_rfc3339(text).unwrap().timestamp()
    }
    #[test]
    fn latest_inside_second_repeated_hour_is_chronological_not_wall_clock_order() {
        let bound = utc("2026-11-01T06:10:00Z");
        let fixed = Timing::Cron {
            expression: "30 1 * * *".into(),
            timezone: "America/New_York".into(),
        };
        assert_eq!(
            fixed.at_or_before(bound).unwrap(),
            Some(utc("2026-11-01T05:30:00Z"))
        );
        let wildcard = Timing::Cron {
            expression: "* * * * *".into(),
            timezone: "America/New_York".into(),
        };
        assert_eq!(
            wildcard.at_or_before(bound).unwrap(),
            Some(utc("2026-11-01T05:59:00Z"))
        );
    }
}
