pub fn fmt_bytes(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b < K {
        format!("{bytes} B")
    } else if b < K * K {
        format!("{:.1} KB", b / K)
    } else if b < K * K * K {
        format!("{:.1} MB", b / (K * K))
    } else if b < K * K * K * K {
        format!("{:.2} GB", b / (K * K * K))
    } else {
        format!("{:.2} TB", b / (K * K * K * K))
    }
}

pub fn fmt_speed(bps: f64) -> String {
    if !bps.is_finite() || bps <= 0.0 {
        return "-".to_string();
    }
    format!("{}/s", fmt_bytes(bps as u64))
}

pub fn fmt_date(ms: u64) -> String {
    use chrono::Local;
    match i64::try_from(ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
    {
        Some(dt) => dt
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
        None => String::new(),
    }
}

/// `<bytes>:<total>:<speed x1000>`, the engine's shared progress payload shape.
pub fn parse_progress(payload: &str) -> Option<(u64, u64, f64)> {
    let mut parts = payload.splitn(3, ':');
    let bytes = parts.next()?.parse::<u64>().ok()?;
    let total = parts.next()?.parse::<u64>().ok()?;
    let speed = parts.next()?.parse::<i64>().ok()? as f64 / 1000.0;
    Some((bytes, total, speed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_use_binary_units() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1536), "1.5 KB");
        assert_eq!(fmt_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    #[test]
    fn speed_of_nothing_is_a_dash() {
        assert_eq!(fmt_speed(0.0), "-");
        assert_eq!(fmt_speed(f64::NAN), "-");
        assert_eq!(fmt_speed(2048.0), "2.0 KB/s");
    }

    #[test]
    fn progress_payload_parsing() {
        assert_eq!(parse_progress("10:20:1500"), Some((10, 20, 1.5)));
        assert_eq!(parse_progress("10:20"), None);
        assert_eq!(parse_progress("x:20:1"), None);
    }

    #[test]
    fn bad_timestamps_format_empty() {
        assert_eq!(fmt_date(u64::MAX), "");
    }
}
