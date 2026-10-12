//! Modification-time conversion: `SystemTime` -> DOS date/time + Info-ZIP
//! extended timestamp (0x5455).
//!
//! DOS time is local wall-clock time with 2-second granularity and a 1980..=2107
//! range; readers that understand the extended timestamp field use the exact
//! UTC seconds from it instead.

use std::time::{SystemTime, UNIX_EPOCH};

/// Header ID of the Info-ZIP "extended timestamp" extra field.
pub(crate) const EXT_TIMESTAMP_ID: u16 = 0x5455;

/// Broken-down wall-clock time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Civil {
    pub year: i64,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Seconds since the Unix epoch (negative before 1970), truncated toward
/// negative infinity.
pub(crate) fn unix_secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(e) => {
            let d = e.duration();
            let secs = i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
            if d.subsec_nanos() > 0 {
                -secs - 1
            } else {
                -secs
            }
        }
    }
}

/// UTC civil time for a Unix timestamp (Howard Hinnant's `civil_from_days`).
pub(crate) fn civil_from_unix_utc(secs: i64) -> Civil {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = yoe + era * 400 + i64::from(month <= 2);
    Civil {
        year,
        month,
        day,
        hour: (sod / 3600) as u8,
        minute: ((sod % 3600) / 60) as u8,
        second: (sod % 60) as u8,
    }
}

/// Local civil time for a Unix timestamp, using the zone rules (UTC offset and
/// daylight saving) in force at that instant rather than today's offset. Falls
/// back to UTC where the local zone can't be determined (and on platforms that
/// are neither Unix nor Windows).
pub(crate) fn civil_from_unix_local(secs: i64) -> Civil {
    #[cfg(unix)]
    {
        if let Some(c) = local_unix(secs) {
            return c;
        }
    }
    #[cfg(windows)]
    {
        if let Some(c) = local_windows(secs) {
            return c;
        }
    }
    civil_from_unix_utc(secs)
}

#[cfg(unix)]
fn local_unix(secs: i64) -> Option<Civil> {
    #[allow(clippy::useless_conversion)] // time_t is i32 on some targets
    let t: libc::time_t = secs.try_into().ok()?;
    // SAFETY: localtime_r is the re-entrant variant; it only writes into `tm`,
    // which is a plain-old-data struct for which all-zero is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::localtime_r(&t, &mut tm) };
    if r.is_null() {
        return None;
    }
    Some(Civil {
        year: i64::from(tm.tm_year) + 1900,
        month: u8::try_from(tm.tm_mon + 1).ok()?,
        day: u8::try_from(tm.tm_mday).ok()?,
        hour: u8::try_from(tm.tm_hour).ok()?,
        minute: u8::try_from(tm.tm_min).ok()?,
        // tm_sec can be 60 for a leap second; DOS can't represent it anyway.
        second: u8::try_from(tm.tm_sec.min(59)).ok()?,
    })
}

/// Windows: `SystemTimeToTzSpecificLocalTimeEx` with the current zone's
/// *dynamic* information applies the UTC offset and DST rules for the year of
/// the timestamp being converted (the registry's "Dynamic DST" data), the way
/// Explorer displays a file's date. `FileTimeToLocalFileTime` would be wrong
/// here: it applies today's bias to every date, so a summer mtime archived in
/// winter would come out an hour off.
///
/// The zone is queried per call (it's cheap) so a long-running app picks up a
/// time-zone change, as `localtime_r` does on Unix.
#[cfg(windows)]
fn local_windows(secs: i64) -> Option<Civil> {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::Time::{
        GetDynamicTimeZoneInformation, SystemTimeToTzSpecificLocalTimeEx,
        DYNAMIC_TIME_ZONE_INFORMATION, TIME_ZONE_ID_INVALID,
    };

    let u = civil_from_unix_utc(secs);
    // SYSTEMTIME covers 1601..=30827.
    let year = u16::try_from(u.year)
        .ok()
        .filter(|y| (1601..=30827).contains(y))?;
    let utc = SYSTEMTIME {
        wYear: year,
        wMonth: u16::from(u.month),
        wDayOfWeek: u16::from(weekday_from_unix(secs)),
        wDay: u16::from(u.day),
        wHour: u16::from(u.hour),
        wMinute: u16::from(u.minute),
        wSecond: u16::from(u.second),
        wMilliseconds: 0,
    };
    let mut tz = DYNAMIC_TIME_ZONE_INFORMATION::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: both calls only read/write the plain-old-data structs passed by
    // pointer, which live for the duration of the calls.
    unsafe {
        if GetDynamicTimeZoneInformation(&mut tz) == TIME_ZONE_ID_INVALID {
            return None;
        }
        if SystemTimeToTzSpecificLocalTimeEx(&tz, &utc, &mut local) == 0 {
            return None;
        }
    }
    civil_from_fields(
        local.wYear,
        local.wMonth,
        local.wDay,
        local.wHour,
        local.wMinute,
        local.wSecond,
    )
}

/// Day of the week of a Unix timestamp, 0 = Sunday (the `SYSTEMTIME` convention).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn weekday_from_unix(secs: i64) -> u8 {
    // 1970-01-01 was a Thursday.
    (secs.div_euclid(86_400) + 4).rem_euclid(7) as u8
}

/// Validated civil time from broken-down fields as an OS reports them (e.g. a
/// Windows `SYSTEMTIME`). A leap second (60) is folded into 59.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn civil_from_fields(
    year: u16,
    month: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
) -> Option<Civil> {
    let ok = (1..=12).contains(&month)
        && (1..=31).contains(&day)
        && hour < 24
        && minute < 60
        && second <= 60;
    ok.then(|| Civil {
        year: i64::from(year),
        month: month as u8,
        day: day as u8,
        hour: hour as u8,
        minute: minute as u8,
        second: second.min(59) as u8,
    })
}

/// Clamp a civil time into the DOS-representable range: anything before 1980
/// becomes 1980-01-01 00:00:00, anything after 2107 becomes 2107-12-31 23:59:58.
pub(crate) fn clamp_to_dos_range(c: Civil) -> Civil {
    if c.year < 1980 {
        Civil {
            year: 1980,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
        }
    } else if c.year > 2107 {
        Civil {
            year: 2107,
            month: 12,
            day: 31,
            hour: 23,
            minute: 59,
            second: 58,
        }
    } else {
        c
    }
}

/// DOS `(time, date)` fields (local wall clock, clamped, 2-second resolution)
/// for a modification time, as stored in ZIP headers.
pub(crate) fn dos_datetime(mtime: SystemTime) -> (u16, u16) {
    dos_fields(clamp_to_dos_range(civil_from_unix_local(unix_secs(mtime))))
}

/// DOS `(time, date)` for 1980-01-01 00:00:00, used when an entry has no mtime.
pub(crate) const DOS_EPOCH: (u16, u16) = (0, (1 << 5) | 1);

/// Pack an in-range civil time into DOS `(time, date)`; odd seconds round down.
fn dos_fields(c: Civil) -> (u16, u16) {
    let time = (u16::from(c.hour) << 11) | (u16::from(c.minute) << 5) | u16::from(c.second / 2);
    let date = (((c.year - 1980) as u16) << 9) | (u16::from(c.month) << 5) | u16::from(c.day);
    (time, date)
}

/// Payload of a 0x5455 extended-timestamp field carrying only the mtime
/// (flags byte with bit 0 set, then a little-endian signed 32-bit UTC time).
/// The same 5 bytes are valid in both the local and central headers.
/// Returns `None` for times that don't fit in an `i32` (before 1901 or after 2038).
pub(crate) fn ext_timestamp_payload(secs: i64) -> Option<[u8; 5]> {
    let t = i32::try_from(secs).ok()?;
    let b = t.to_le_bytes();
    Some([0x01, b[0], b[1], b[2], b[3]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn civ(year: i64, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Civil {
        Civil {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    }

    #[test]
    fn utc_conversion_known_values() {
        assert_eq!(civil_from_unix_utc(0), civ(1970, 1, 1, 0, 0, 0));
        assert_eq!(
            civil_from_unix_utc(951_782_400),
            civ(2000, 2, 29, 0, 0, 0),
            "leap day"
        );
        assert_eq!(
            civil_from_unix_utc(1_700_000_000),
            civ(2023, 11, 14, 22, 13, 20)
        );
        assert_eq!(civil_from_unix_utc(-1), civ(1969, 12, 31, 23, 59, 59));
        assert_eq!(civil_from_unix_utc(315_532_800), civ(1980, 1, 1, 0, 0, 0));
    }

    #[test]
    fn unix_secs_handles_pre_epoch() {
        assert_eq!(unix_secs(UNIX_EPOCH + Duration::from_secs(42)), 42);
        assert_eq!(unix_secs(UNIX_EPOCH - Duration::from_secs(10)), -10);
        assert_eq!(unix_secs(UNIX_EPOCH - Duration::from_millis(1500)), -2);
        assert_eq!(unix_secs(UNIX_EPOCH + Duration::from_millis(1500)), 1);
    }

    #[test]
    fn clamps_out_of_range_years() {
        assert_eq!(
            clamp_to_dos_range(civ(1975, 6, 15, 12, 0, 0)),
            civ(1980, 1, 1, 0, 0, 0)
        );
        assert_eq!(
            clamp_to_dos_range(civ(2200, 1, 1, 0, 0, 0)),
            civ(2107, 12, 31, 23, 59, 58)
        );
        let ok = civ(2024, 3, 4, 5, 6, 7);
        assert_eq!(clamp_to_dos_range(ok), ok);
    }

    fn unpack(f: (u16, u16)) -> (i64, u8, u8, u8, u8, u8) {
        let (t, d) = f;
        (
            1980 + i64::from(d >> 9),
            ((d >> 5) & 0xf) as u8,
            (d & 0x1f) as u8,
            (t >> 11) as u8,
            ((t >> 5) & 0x3f) as u8,
            ((t & 0x1f) * 2) as u8,
        )
    }

    #[test]
    fn dos_datetime_pre_1980_is_clamped() {
        assert_eq!(dos_datetime(UNIX_EPOCH), DOS_EPOCH);
        assert_eq!(unpack(DOS_EPOCH), (1980, 1, 1, 0, 0, 0));
    }

    #[test]
    fn dos_field_packing() {
        assert_eq!(
            unpack(dos_fields(civ(2107, 12, 31, 23, 59, 58))),
            (2107, 12, 31, 23, 59, 58)
        );
        assert_eq!(
            unpack(dos_fields(civ(2024, 2, 29, 13, 7, 9))),
            (2024, 2, 29, 13, 7, 8)
        );
    }

    #[test]
    fn dos_datetime_matches_local_civil() {
        let secs = 1_700_000_001; // odd second
        let got = unpack(dos_datetime(UNIX_EPOCH + Duration::from_secs(secs as u64)));
        let c = civil_from_unix_local(secs);
        assert_eq!(
            got,
            (c.year, c.month, c.day, c.hour, c.minute, c.second & !1)
        );
    }

    #[test]
    fn weekdays() {
        assert_eq!(weekday_from_unix(0), 4, "1970-01-01 was a Thursday");
        assert_eq!(weekday_from_unix(86_399), 4);
        assert_eq!(weekday_from_unix(-1), 3, "1969-12-31 was a Wednesday");
        assert_eq!(
            weekday_from_unix(1_700_000_000),
            2,
            "2023-11-14 was a Tuesday"
        );
        assert_eq!(
            weekday_from_unix(951_782_400),
            2,
            "2000-02-29 was a Tuesday"
        );
    }

    #[test]
    fn fields_to_civil() {
        assert_eq!(
            civil_from_fields(2024, 7, 1, 23, 59, 58),
            Some(civ(2024, 7, 1, 23, 59, 58))
        );
        assert_eq!(
            civil_from_fields(2016, 12, 31, 23, 59, 60),
            Some(civ(2016, 12, 31, 23, 59, 59)),
            "leap second folds into :59"
        );
        for bad in [
            (2024, 0, 1, 0, 0, 0),
            (2024, 13, 1, 0, 0, 0),
            (2024, 1, 0, 0, 0, 0),
            (2024, 1, 32, 0, 0, 0),
            (2024, 1, 1, 24, 0, 0),
            (2024, 1, 1, 0, 60, 0),
            (2024, 1, 1, 0, 0, 61),
        ] {
            let (y, mo, d, h, mi, s) = bad;
            assert_eq!(civil_from_fields(y, mo, d, h, mi, s), None, "{bad:?}");
        }
    }

    /// A broken-down local time (as the OS returns it) packs into exactly those
    /// DOS fields: no further zone shift happens after the OS conversion.
    #[test]
    fn local_fields_pack_unchanged() {
        let c = civil_from_fields(2023, 7, 15, 14, 30, 45).unwrap();
        assert_eq!(
            unpack(dos_fields(clamp_to_dos_range(c))),
            (2023, 7, 15, 14, 30, 44)
        );
        let old = civil_from_fields(1979, 12, 31, 23, 0, 0).unwrap();
        assert_eq!(
            unpack(dos_fields(clamp_to_dos_range(old))),
            (1980, 1, 1, 0, 0, 0)
        );
    }

    #[test]
    fn ext_timestamp_layout() {
        assert_eq!(
            ext_timestamp_payload(0x0102_0304),
            Some([1, 0x04, 0x03, 0x02, 0x01])
        );
        assert_eq!(ext_timestamp_payload(-1), Some([1, 0xff, 0xff, 0xff, 0xff]));
        assert_eq!(ext_timestamp_payload(i64::from(i32::MAX) + 1), None);
    }
}
