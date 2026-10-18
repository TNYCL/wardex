use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix time in milliseconds.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
}
