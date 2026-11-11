//! Device dates in and out of the CLI: how `ls` prints them, how `get` turns one into a local
//! file's mtime, and how `put` sends a local file's mtime to the device.
//!
//! Most devices write zoneless dates, which mean "wall-clock time in the device's zone", never UTC
//! (`AGENTS.md` § Known device quirks). The CLI never makes a zone up when it prints one, and when
//! it needs an instant (an mtime), it reads a zoneless date in the host's zone: a phone usually sits
//! on the same desk as the computer it's plugged into.

use jiff::tz::TimeZone;
use mtp_rs::{DateTime, UtcOffset};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// ISO 8601, with the offset only when the device sent one: `2026-11-03T14:22:05` or
/// `2026-11-03T14:22:05+01:00`. Never a `Z` the device didn't write, since that would claim a zone.
pub fn iso(dt: &DateTime) -> String {
    let mut s = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
    );
    if let Some(offset) = dt.offset {
        let minutes = offset.minutes();
        let sign = if minutes < 0 { '-' } else { '+' };
        let abs = minutes.unsigned_abs();
        s.push_str(&format!("{sign}{:02}:{:02}", abs / 60, abs % 60));
    }
    s
}

/// The host's time zone, DST rules included (`TZ` first, then the system setting). Falls back to
/// UTC with a warning when there's none to find, so a download still completes.
pub fn host_zone() -> TimeZone {
    TimeZone::try_system().unwrap_or_else(|e| {
        eprintln!("warning: couldn't find the local time zone ({e}); reading device dates as UTC");
        TimeZone::UTC
    })
}

/// The instant a device date names. A date with an offset converts exactly; a zoneless one is read
/// as wall-clock time in `zone`, at the offset in force on that date (so a summer photo gets
/// summer time). A wall-clock time that doesn't exist or repeats around a DST change resolves the
/// way `mktime` does. `None` for fields that don't form a real date.
pub fn device_date_to_system_time(dt: &DateTime, zone: &TimeZone) -> Option<SystemTime> {
    let secs = match dt.offset {
        Some(_) => dt.to_unix_seconds()?,
        None => {
            let civil = jiff::civil::DateTime::new(
                i16::try_from(dt.year).ok()?,
                i8::try_from(dt.month).ok()?,
                i8::try_from(dt.day).ok()?,
                i8::try_from(dt.hour).ok()?,
                i8::try_from(dt.minute).ok()?,
                i8::try_from(dt.second).ok()?,
                0,
            )
            .ok()?;
            zone.to_ambiguous_zoned(civil)
                .compatible()
                .ok()?
                .timestamp()
                .as_second()
        }
    };
    if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(secs.unsigned_abs()))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
    }
}

/// A local file's mtime as the date to send with an upload, in UTC with a `Z`.
///
/// UTC because it's the one form that lands the right instant on Android whatever zone the phone
/// is in: AOSP's `parseDateTime` (`MtpUtils.cpp`) honors `Z`, reads any `±hhmm` as phone-local
/// time (so a real offset would shift the date by the difference), and reads a zoneless date as
/// phone-local too (right only when the phone and the computer share a zone). Sub-second precision
/// is dropped, since MTP dates stop at seconds. `None` past the years MTP can write (0-9999).
pub fn system_time_to_device_date(time: SystemTime) -> Option<DateTime> {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).ok()?,
        Err(before) => {
            let before = before.duration();
            // Floor, so 0.5 s before the epoch is -1, not 0.
            let whole = i64::try_from(before.as_secs()).ok()?;
            if before.subsec_nanos() > 0 {
                -whole - 1
            } else {
                -whole
            }
        }
    };
    DateTime::from_unix_seconds(secs, UtcOffset::UTC)
}

/// Sets a file's or folder's modification time.
pub fn set_mtime(path: &Path, time: SystemTime) -> std::io::Result<()> {
    open_for_times(path)?.set_modified(time)
}

#[cfg(not(windows))]
fn open_for_times(path: &Path) -> std::io::Result<std::fs::File> {
    // `futimens` needs no write access, only ownership, and a read-only open works on folders too.
    std::fs::File::open(path)
}

#[cfg(windows)]
fn open_for_times(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    // `SetFileTime` needs this access right, which a plain read open doesn't ask for, and opening a
    // folder at all needs backup semantics.
    const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    std::fs::OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(name: &str) -> TimeZone {
        TimeZone::get(name).unwrap()
    }

    fn unix(time: SystemTime) -> i64 {
        match time.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        }
    }

    #[test]
    fn iso_prints_no_zone_for_a_zoneless_date() {
        let dt = DateTime::new(2026, 11, 3, 14, 22, 5).unwrap();
        assert_eq!(iso(&dt), "2026-11-03T14:22:05");
    }

    #[test]
    fn iso_prints_the_offset_the_device_sent() {
        let dt = DateTime::new(2026, 11, 3, 14, 22, 5).unwrap();
        let plus = dt.with_offset(UtcOffset::from_minutes(60).unwrap());
        let minus = dt.with_offset(UtcOffset::from_minutes(-(3 * 60 + 30)).unwrap());
        assert_eq!(iso(&plus), "2026-11-03T14:22:05+01:00");
        assert_eq!(iso(&minus), "2026-11-03T14:22:05-03:30");
        assert_eq!(
            iso(&dt.with_offset(UtcOffset::UTC)),
            "2026-11-03T14:22:05+00:00"
        );
    }

    #[test]
    fn a_zoneless_date_reads_at_the_offset_in_force_on_that_date() {
        let stockholm = zone("Europe/Stockholm");
        // 2026-07-01 12:00 CEST (UTC+2) and 2026-01-15 12:00 CET (UTC+1).
        let summer = DateTime::new(2026, 7, 1, 12, 0, 0).unwrap();
        let winter = DateTime::new(2026, 1, 15, 12, 0, 0).unwrap();
        assert_eq!(
            unix(device_date_to_system_time(&summer, &stockholm).unwrap()),
            1_782_900_000
        );
        assert_eq!(
            unix(device_date_to_system_time(&winter, &stockholm).unwrap()),
            1_768_474_800
        );
    }

    #[test]
    fn a_date_with_an_offset_ignores_the_host_zone() {
        let dt = DateTime::new(2026, 7, 1, 12, 0, 0)
            .unwrap()
            .with_offset(UtcOffset::from_minutes(120).unwrap());
        for name in ["UTC", "Asia/Kolkata", "America/New_York"] {
            assert_eq!(
                unix(device_date_to_system_time(&dt, &zone(name)).unwrap()),
                1_782_900_000,
                "{name}"
            );
        }
    }

    #[test]
    fn dates_before_the_epoch_convert_both_ways() {
        let dt = DateTime::new(1969, 12, 31, 23, 0, 0).unwrap();
        let time = device_date_to_system_time(&dt, &TimeZone::UTC).unwrap();
        assert_eq!(unix(time), -3600);
        let back = system_time_to_device_date(time).unwrap();
        assert_eq!(
            (back.year, back.hour, back.offset),
            (1969, 23, Some(UtcOffset::UTC))
        );
    }

    #[test]
    fn an_upload_date_is_utc_and_drops_subseconds() {
        let time = UNIX_EPOCH + Duration::from_millis(1_782_900_000_750);
        let dt = system_time_to_device_date(time).unwrap();
        assert_eq!(dt.offset, Some(UtcOffset::UTC));
        assert_eq!(dt.to_unix_seconds(), Some(1_782_900_000));

        let half_before_epoch = UNIX_EPOCH - Duration::from_millis(500);
        assert_eq!(
            system_time_to_device_date(half_before_epoch)
                .unwrap()
                .to_unix_seconds(),
            Some(-1)
        );
    }

    #[test]
    fn set_mtime_works_on_files_and_folders() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let folder = dir.path().join("sub");
        std::fs::create_dir(&folder).unwrap();
        let when = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        for path in [&file, &folder] {
            set_mtime(path, when).unwrap();
            assert_eq!(std::fs::metadata(path).unwrap().modified().unwrap(), when);
        }
    }
}
