//! Five-field cron patterns (`minute hour day-of-month month day-of-week`) and
//! their next match in a time zone. Pure: the zone comes from the caller.

use chrono::{Datelike, NaiveDate, TimeZone};

/// Each field is `*`, `n` or `a-b`, optionally `/step`, comma-separated, e.g.
/// `*/15 9-17 * * 1-5`. No names (`MON`, `JAN`). Day-of-week is 0-7, both 0 and 7
/// Sunday. As in cron, when both day fields are restricted a day matching either runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Cron {
    src: String,
    /// Bit n set: value n matches.
    minute: u64,
    hour: u64,
    dom: u64,
    month: u64,
    dow: u64,
    /// The day field starts with `*`, so only the other one restricts the day.
    dom_any: bool,
    dow_any: bool,
}

/// How far ahead to look: covers Feb 29 even across a skipped leap year (2100).
const HORIZON_DAYS: u32 = 9 * 366;

impl std::str::FromStr for Cron {
    type Err = String;

    fn from_str(src: &str) -> Result<Self, String> {
        let fields: Vec<&str> = src.split_whitespace().collect();
        let [minute, hour, dom, month, dow] = fields[..] else {
            return Err(format!(
                "cron `{src}`: want 5 fields (minute hour day month weekday)"
            ));
        };
        let field = |s: &str, what, min, max| {
            mask(s, min, max).map_err(|why| format!("cron `{src}`: {what} `{s}`: {why}"))
        };
        let mut dow_mask = field(dow, "weekday", 0, 7)?;
        // 7 is Sunday too.
        if dow_mask & (1 << 7) != 0 {
            dow_mask = (dow_mask | 1) & !(1 << 7);
        }
        Ok(Cron {
            src: fields.join(" "),
            minute: field(minute, "minute", 0, 59)?,
            hour: field(hour, "hour", 0, 23)?,
            dom: field(dom, "day", 1, 31)?,
            month: field(month, "month", 1, 12)?,
            dow: dow_mask,
            dom_any: dom.starts_with('*'),
            dow_any: dow.starts_with('*'),
        })
    }
}

/// Values `s` names in `min..=max`, as a bit mask.
fn mask(s: &str, min: u32, max: u32) -> Result<u64, String> {
    let num = |n: &str| match n.parse::<u32>() {
        Ok(v) if (min..=max).contains(&v) => Ok(v),
        _ => Err(format!("want {min}-{max}")),
    };
    let mut bits = 0u64;
    for part in s.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => match st.parse::<u32>() {
                Ok(st) if st > 0 => (r, Some(st)),
                _ => return Err("step must be a positive number".into()),
            },
            None => (part, None),
        };
        let (lo, hi) = match range.split_once('-') {
            _ if range == "*" => (min, max),
            Some((a, b)) => (num(a)?, num(b)?),
            // `5/15` means 5-max every 15, as in cron.
            None if step.is_some() => (num(range)?, max),
            None => (num(range)?, num(range)?),
        };
        if lo > hi {
            return Err(format!("{lo}-{hi} is backwards"));
        }
        for v in (lo..=hi).step_by(step.unwrap_or(1) as usize) {
            bits |= 1 << v;
        }
    }
    Ok(bits)
}

impl Cron {
    /// The pattern as written (whitespace collapsed).
    pub fn as_str(&self) -> &str {
        &self.src
    }

    fn day_matches(&self, d: NaiveDate) -> bool {
        let bit = |mask: u64, v: u32| mask & (1 << v) != 0;
        let dom = bit(self.dom, d.day());
        let dow = bit(self.dow, d.weekday().num_days_from_sunday());
        let day = match (self.dom_any, self.dow_any) {
            (false, false) => dom || dow,
            _ => dom && dow,
        };
        day && bit(self.month, d.month())
    }

    /// First matching minute strictly after `after_ms` (Unix millis), in `tz`'s
    /// wall-clock time. As in cron, each wall-clock match runs once: a time DST
    /// skips never matches, one it repeats matches its first instance. None: no
    /// match within years (e.g. Feb 30).
    pub fn next_after<Tz: TimeZone>(&self, after_ms: u64, tz: &Tz) -> Option<u64> {
        let start = tz.timestamp_millis_opt(after_ms as i64).single()?;
        let mut day = start.naive_local().date();
        for _ in 0..HORIZON_DAYS {
            if self.day_matches(day) {
                let hours = (0..24).filter(|h| self.hour & (1 << h) != 0);
                for h in hours {
                    for m in (0..60).filter(|m| self.minute & (1 << m) != 0) {
                        let local = day.and_hms_opt(h, m, 0)?;
                        let Some(at) = tz.from_local_datetime(&local).earliest() else {
                            continue;
                        };
                        let at = at.timestamp_millis();
                        if at > after_ms as i64 {
                            return Some(at as u64);
                        }
                    }
                }
            }
            day = day.succ_opt()?;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Utc};

    fn ms(s: &str) -> u64 {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .timestamp_millis() as u64
    }

    fn next(cron: &str, after: &str) -> String {
        let c: Cron = cron.parse().unwrap();
        let at = c.next_after(ms(after), &Utc).unwrap();
        let t = Utc.timestamp_millis_opt(at as i64).unwrap();
        t.format("%Y-%m-%d %a %H:%M").to_string()
    }

    #[test]
    fn weekday_mornings() {
        // 2026-11-06 is a Friday.
        let c = "0 9 * * 1-5";
        assert_eq!(next(c, "2026-11-06T08:59:00Z"), "2026-11-06 Fri 09:00");
        // Strictly after: at 09:00 itself, the next is Monday's.
        assert_eq!(next(c, "2026-11-06T09:00:00Z"), "2026-11-09 Mon 09:00");
    }

    #[test]
    fn steps_lists_and_ranges() {
        assert_eq!(
            next("*/15 * * * *", "2026-11-06T10:01:00Z"),
            "2026-11-06 Fri 10:15"
        );
        assert_eq!(
            next("5/20 * * * *", "2026-11-06T10:30:00Z"),
            "2026-11-06 Fri 10:45"
        );
        assert_eq!(
            next("0 8,20 * * *", "2026-11-06T09:00:00Z"),
            "2026-11-06 Fri 20:00"
        );
        assert_eq!(
            next("30 9-17/4 * * *", "2026-11-06T13:31:00Z"),
            "2026-11-06 Fri 17:30"
        );
        // Sunday as 7; month and day restricted.
        assert_eq!(
            next("0 0 * * 7", "2026-11-06T00:00:00Z"),
            "2026-11-08 Sun 00:00"
        );
        assert_eq!(
            next("0 0 29 2 *", "2026-03-01T00:00:00Z"),
            "2028-02-29 Tue 00:00"
        );
    }

    #[test]
    fn both_day_fields_restricted_means_either() {
        // The 1st, or any Monday.
        assert_eq!(
            next("0 0 1 * 1", "2026-11-03T00:00:00Z"),
            "2026-11-09 Mon 00:00"
        );
        assert_eq!(
            next("0 0 1 * 1", "2026-11-28T00:00:00Z"),
            "2026-11-30 Mon 00:00"
        );
        assert_eq!(
            next("0 0 1 * 1", "2026-11-30T00:00:00Z"),
            "2026-12-01 Tue 00:00"
        );
    }

    #[test]
    fn reads_wall_clock_in_the_given_zone() {
        let c: Cron = "0 9 * * *".parse().unwrap();
        let tz = FixedOffset::east_opt(2 * 3600).unwrap();
        // 09:00 at +02:00 is 07:00 UTC.
        assert_eq!(
            c.next_after(ms("2026-11-06T06:00:00Z"), &tz),
            Some(ms("2026-11-06T07:00:00Z"))
        );
    }

    #[test]
    fn never_matching_is_none() {
        let c: Cron = "0 0 30 2 *".parse().unwrap();
        assert_eq!(c.next_after(0, &Utc), None);
    }

    #[test]
    fn bad_patterns_say_which_field() {
        for (src, why) in [
            ("0 9 * *", "want 5 fields"),
            ("60 * * * *", "minute `60`"),
            ("* 24 * * *", "hour `24`"),
            ("* * 0 * *", "day `0`"),
            ("* * * 13 *", "month `13`"),
            ("* * * * 8", "weekday `8`"),
            ("*/0 * * * *", "step"),
            ("5-1 * * * *", "backwards"),
            ("* * * * MON", "weekday `MON`"),
        ] {
            let err = src.parse::<Cron>().unwrap_err();
            assert!(err.contains(why), "{src}: {err}");
        }
    }
}
