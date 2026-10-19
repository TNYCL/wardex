use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix time in milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Unix milliseconds of an RFC 3339 timestamp such as
/// `2026-10-18T09:15:02.123Z` or `2026-10-18T11:15:02+02:00`.
pub fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', 't', ' '])?;
    let mut d = date.splitn(3, '-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let zone_at = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
    let (time, zone) = rest.split_at(zone_at);
    let mut t = time.splitn(3, ':');
    let hour: i64 = t.next()?.parse().ok()?;
    let minute: i64 = t.next()?.parse().ok()?;
    let (sec, frac) = match t.next()?.split_once('.') {
        Some((s, f)) => (s, f),
        None => (t.next().unwrap_or("0"), ""),
    };
    let sec: i64 = sec.parse().ok()?;
    let mut millis = 0;
    for (i, c) in frac.chars().take(3).enumerate() {
        millis += i64::from(c.to_digit(10)?) * 10_i64.pow(2 - i as u32);
    }
    let offset_min = match zone {
        "" | "Z" | "z" => 0,
        z => {
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let (h, m) = z[1..].split_once(':').unwrap_or((&z[1..], "0"));
            sign * (h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?)
        }
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + sec - offset_min * 60;
    Some(secs * 1000 + millis)
}

/// `HH:MM:SS` (UTC) for a Unix timestamp in milliseconds.
pub fn clock(ts_ms: i64) -> String {
    let secs = ts_ms.div_euclid(1000).rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60
    )
}

/// Compact duration such as `45s`, `12m`, `3h05m`, `2d04h`.
pub fn human_duration(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86_400 => format!("{}h{:02}m", s / 3600, (s / 60) % 60),
        _ => format!("{}d{:02}h", s / 86_400, (s / 3600) % 24),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(clock(3_723_000), "01:02:03");
        assert_eq!(human_duration(45_000), "45s");
        assert_eq!(human_duration(125_000), "2m");
        assert_eq!(human_duration(3_900_000), "1h05m");
        assert_eq!(human_duration(2 * 86_400_000 + 4 * 3_600_000), "2d04h");
    }

    #[test]
    fn parses_rfc3339() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2026-10-18T09:15:02.123Z"),
            Some(1_792_314_902_123)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-10-18T11:15:02.123456+02:00"),
            Some(1_792_314_902_123)
        );
        assert_eq!(
            parse_rfc3339_ms("2024-02-29T00:00:00-01:30"),
            Some(1_709_170_200_000)
        );
        assert_eq!(parse_rfc3339_ms("not a date"), None);
        assert_eq!(parse_rfc3339_ms("2026-13-01T00:00:00Z"), None);
    }
}
