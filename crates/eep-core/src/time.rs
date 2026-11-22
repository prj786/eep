//! The few date conversions eep needs, without a date library: UTC only,
//! RFC 3339 `YYYY-MM-DDTHH:MM:SSZ` out, RFC 3339 and HTTP dates in.

use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

pub fn now_rfc3339() -> String {
    rfc3339(now_unix())
}

pub fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `2026-11-21T15:12:40Z` (also accepts a `+00:00` offset).
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z').or_else(|| s.strip_suffix("+00:00"))?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(str::parse::<i64>);
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time.splitn(3, ':');
    let (hh, mm) = (
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
    );
    let ss = t.next()?.split('.').next()?.parse::<i64>().ok()?;
    Some(days_from_civil(y, m, day) * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// `Sat, 21 Nov 2026 15:12:40 GMT` (the IMF-fixdate form servers send).
pub fn parse_http_date(s: &str) -> Option<i64> {
    let mut it = s.split_whitespace().skip(1);
    let day: i64 = it.next()?.parse().ok()?;
    let mon = it.next()?;
    let year: i64 = it.next()?.parse().ok()?;
    let mut t = it.next()?.split(':').map(str::parse::<i64>);
    let (hh, mm, ss) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let m = i64::try_from(MONTHS.iter().position(|x| *x == mon)?).ok()? + 1;
    Some(days_from_civil(year, m, day) * 86_400 + hh * 3600 + mm * 60 + ss)
}

// Howard Hinnant's algorithms (public domain), proleptic Gregorian calendar.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        let t = parse_http_date("Sat, 21 Nov 2026 15:12:40 GMT").expect("parses");
        assert_eq!(rfc3339(t), "2026-11-21T15:12:40Z");
        assert_eq!(parse_rfc3339("2026-11-21T15:12:40Z"), Some(t));
        assert_eq!(
            parse_rfc3339("2024-02-29T00:00:00Z")
                .map(rfc3339)
                .as_deref(),
            Some("2024-02-29T00:00:00Z")
        );
        assert!(parse_rfc3339("yesterday").is_none());
    }
}
