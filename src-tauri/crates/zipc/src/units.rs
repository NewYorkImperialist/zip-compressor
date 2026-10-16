//! Pure formatting helpers for human-readable output.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Format a byte count with binary units, e.g. `1.5 MiB`. Values under 1 KiB
/// are printed exactly (`512 B`).
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Output size as a percentage of input size (`None` when the input is empty).
pub fn ratio_percent(bytes_in: u64, bytes_out: u64) -> Option<f64> {
    if bytes_in == 0 {
        None
    } else {
        Some(bytes_out as f64 / bytes_in as f64 * 100.0)
    }
}

/// Percent of work done, clamped to 0..=100. An empty job counts as done.
pub fn percent_done(done: u64, total: u64) -> f64 {
    if total == 0 {
        100.0
    } else {
        (done as f64 / total as f64 * 100.0).min(100.0)
    }
}

/// Throughput in megabytes (10^6 bytes) per second, or `None` for a zero duration.
pub fn throughput_mb_s(bytes: u64, elapsed: Duration) -> Option<f64> {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 {
        None
    } else {
        Some(bytes as f64 / 1_000_000.0 / secs)
    }
}

/// Format a duration compactly: `850 ms`, `12.34 s`, `3m 05s`.
pub fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else if d.as_secs() < 60 {
        format!("{:.2} s", d.as_secs_f64())
    } else {
        let s = d.as_secs();
        format!("{}m {:02}s", s / 60, s % 60)
    }
}

/// Shorten `s` to at most `max` characters, eliding the start with `…` so the
/// file name (the most informative part of a path) stays visible.
pub fn truncate_start(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let keep = max - 1;
    let tail: String = s.chars().skip(len - keep).collect();
    format!("…{tail}")
}

/// A UTC instant as RFC 3339, e.g. `2024-03-04T05:06:07Z` (whole seconds,
/// rounded down).
pub fn format_rfc3339_utc(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => {
            let d = e.duration();
            let s = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
            if d.subsec_nanos() > 0 {
                -s - 1
            } else {
                -s
            }
        }
    };
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

/// Year, month, day for days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// A local wall-clock time as `2024-03-04 05:06:07`.
pub fn format_local(t: zipcore::LocalTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// Unix permission bits as a 4-digit octal string, e.g. `0755`.
pub fn format_mode(mode: u32) -> String {
    format!("{mode:04o}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_small_exact() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
    }

    #[test]
    fn bytes_scaled() {
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
        assert_eq!(format_bytes(u64::MAX), "16.0 EiB");
    }

    #[test]
    fn ratio() {
        assert_eq!(ratio_percent(0, 10), None);
        assert_eq!(ratio_percent(200, 50), Some(25.0));
        assert_eq!(ratio_percent(100, 120), Some(120.0));
    }

    #[test]
    fn percent() {
        assert_eq!(percent_done(0, 0), 100.0);
        assert_eq!(percent_done(1, 4), 25.0);
        assert_eq!(percent_done(10, 4), 100.0);
    }

    #[test]
    fn throughput() {
        assert_eq!(throughput_mb_s(10, Duration::ZERO), None);
        assert_eq!(
            throughput_mb_s(5_000_000, Duration::from_secs(2)),
            Some(2.5)
        );
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(Duration::from_millis(850)), "850 ms");
        assert_eq!(format_duration(Duration::from_millis(12_340)), "12.34 s");
        assert_eq!(format_duration(Duration::from_secs(185)), "3m 05s");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_start("abc", 5), "abc");
        assert_eq!(truncate_start("abcdef", 4), "…def");
        assert_eq!(truncate_start("abcdef", 0), "");
        assert_eq!(truncate_start("ééééé", 3), "…éé");
    }

    #[test]
    fn rfc3339() {
        let at = |s: u64| UNIX_EPOCH + Duration::from_secs(s);
        assert_eq!(format_rfc3339_utc(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339_utc(at(951_782_400)), "2000-02-29T00:00:00Z");
        assert_eq!(
            format_rfc3339_utc(at(1_700_000_000) + Duration::from_millis(999)),
            "2023-11-14T22:13:20Z"
        );
        assert_eq!(
            format_rfc3339_utc(UNIX_EPOCH - Duration::from_millis(500)),
            "1969-12-31T23:59:59Z"
        );
    }

    #[test]
    fn local_and_mode() {
        let t = zipcore::LocalTime {
            year: 2024,
            month: 3,
            day: 4,
            hour: 5,
            minute: 6,
            second: 7,
        };
        assert_eq!(format_local(t), "2024-03-04 05:06:07");
        assert_eq!(format_mode(0o755), "0755");
        assert_eq!(format_mode(0o4755), "4755");
        assert_eq!(format_mode(0), "0000");
    }
}
