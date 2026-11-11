//! OLE automation dates (`VT_DATE`) to and from [`DateTime`], with no Windows API in sight.
//!
//! Kept apart from [`super::props`] so the conversion compiles and runs its tests on every host:
//! `backend/mod.rs` also builds this file into non-Windows test builds. The COM side
//! (`props::read_date`, `props::write_date`) and the zone reasoning live in `props.rs`.

use crate::mtp::{DateTime, UtcOffset};

/// Days between the OLE automation epoch (1899-12-30) and the Unix epoch (1970-01-01).
const OLE_TO_UNIX_DAYS: f64 = 25_569.0;

/// Convert an OLE automation date (days since 1899-12-30, fraction = time of day) to a zoneless
/// [`DateTime`]. `None` for 0 (the "no date" value), anything before it, and non-finite input.
pub(crate) fn ole_date_to_datetime(days: f64) -> Option<DateTime> {
    if !days.is_finite() || days <= 0.0 {
        return None;
    }
    let secs = ((days - OLE_TO_UNIX_DAYS) * 86_400.0).round() as i64;
    let d = DateTime::from_unix_seconds(secs, UtcOffset::UTC)?;
    Some(DateTime { offset: None, ..d })
}

/// Convert a date's wall-clock fields (its offset ignored) to an OLE automation date, the inverse
/// of [`ole_date_to_datetime`]. `None` for fields that don't form a real date, and for anything at
/// or before 1899-12-30 00:00, which the reading side treats as "no date".
pub(crate) fn datetime_to_ole_date(date: DateTime) -> Option<f64> {
    let secs = DateTime {
        offset: None,
        ..date
    }
    .to_unix_seconds_with_fallback(UtcOffset::UTC)?;
    let days = secs as f64 / 86_400.0 + OLE_TO_UNIX_DAYS;
    (days > 0.0).then_some(days)
}

/// The zoneless wall-clock time the phone should show for an upload dated `date`.
///
/// A zoneless value is already that. A value with an offset names an instant, so it becomes that
/// instant as wall-clock time at `local_offset_at(instant)` (the PC's zone on Windows). When that
/// gives nothing, the value's own wall-clock fields stand.
pub(crate) fn device_wall_clock(
    date: DateTime,
    local_offset_at: impl FnOnce(i64) -> Option<UtcOffset>,
) -> DateTime {
    let zoneless = DateTime {
        offset: None,
        ..date
    };
    if date.offset.is_none() {
        return zoneless;
    }
    date.to_unix_seconds()
        .and_then(|secs| DateTime::from_unix_seconds(secs, local_offset_at(secs)?))
        .map(|d| DateTime { offset: None, ..d })
        .unwrap_or(zoneless)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zoneless(y: u16, mo: u8, d: u8, h: u8, mi: u8, s: u8) -> Option<DateTime> {
        DateTime::new(y, mo, d, h, mi, s)
    }

    fn offset(minutes: i16) -> UtcOffset {
        UtcOffset::from_minutes(minutes).unwrap()
    }

    #[test]
    fn ole_date_at_the_unix_epoch() {
        assert_eq!(
            ole_date_to_datetime(25_569.0),
            zoneless(1970, 1, 1, 0, 0, 0)
        );
    }

    #[test]
    fn ole_date_fraction_is_the_time_of_day() {
        assert_eq!(
            ole_date_to_datetime(36_585.25),
            zoneless(2000, 2, 29, 6, 0, 0)
        );
    }

    #[test]
    fn ole_date_rounds_to_the_nearest_second() {
        // The PR #41 hardware sample: IMG_20260809_190833.jpg. Not exact in f64, so this pins
        // the rounding too.
        let days = 46_243.0 + 68_913.0 / 86_400.0;
        assert_eq!(ole_date_to_datetime(days), zoneless(2026, 8, 9, 19, 8, 33));
    }

    #[test]
    fn ole_date_is_zoneless() {
        assert_eq!(ole_date_to_datetime(46_243.5).unwrap().offset, None);
    }

    #[test]
    fn ole_date_zero_negative_and_non_finite_are_no_date() {
        for days in [0.0, -1.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(ole_date_to_datetime(days), None, "{days}");
        }
    }

    #[test]
    fn datetime_to_ole_date_known_values() {
        assert_eq!(
            datetime_to_ole_date(zoneless(1970, 1, 1, 0, 0, 0).unwrap()),
            Some(25_569.0)
        );
        assert_eq!(
            datetime_to_ole_date(zoneless(2000, 2, 29, 6, 0, 0).unwrap()),
            Some(36_585.25)
        );
    }

    #[test]
    fn datetime_to_ole_date_round_trips_through_the_reading_side() {
        for d in [
            zoneless(2019, 6, 14, 10, 23, 0),
            zoneless(2026, 8, 9, 19, 8, 33),
            zoneless(1900, 3, 1, 0, 0, 1),
            zoneless(9999, 12, 31, 23, 59, 59),
        ] {
            let d = d.unwrap();
            assert_eq!(
                ole_date_to_datetime(datetime_to_ole_date(d).unwrap()),
                Some(d)
            );
        }
    }

    #[test]
    fn datetime_to_ole_date_ignores_the_offset() {
        // The caller resolves zones first (device_wall_clock); this sees wall-clock fields only.
        let d = zoneless(2019, 6, 14, 10, 23, 0).unwrap();
        assert_eq!(
            datetime_to_ole_date(d.with_offset(offset(120))),
            datetime_to_ole_date(d)
        );
    }

    #[test]
    fn datetime_to_ole_date_refuses_the_no_date_value_and_earlier() {
        assert_eq!(
            datetime_to_ole_date(zoneless(1899, 12, 30, 0, 0, 0).unwrap()),
            None
        );
        assert_eq!(
            datetime_to_ole_date(zoneless(1800, 1, 1, 12, 0, 0).unwrap()),
            None
        );
        assert!(datetime_to_ole_date(zoneless(1899, 12, 30, 0, 0, 1).unwrap()).is_some());
    }

    #[test]
    fn datetime_to_ole_date_refuses_fields_that_are_no_date() {
        let mut d = zoneless(2023, 2, 28, 0, 0, 0).unwrap();
        d.day = 29;
        assert_eq!(datetime_to_ole_date(d), None);
    }

    #[test]
    fn zoneless_dates_go_out_as_they_are() {
        let d = zoneless(2019, 6, 14, 10, 23, 0).unwrap();
        let wall = device_wall_clock(d, |_| panic!("a zoneless date needs no zone lookup"));
        assert_eq!(wall, d);
    }

    #[test]
    fn utc_dates_become_pc_local_wall_clock_time() {
        // 2019-06-14 08:23Z on a PC at UTC+2 (Stockholm summer time) reads 10:23 on the phone.
        let utc = zoneless(2019, 6, 14, 8, 23, 0)
            .unwrap()
            .with_offset(UtcOffset::UTC);
        let wall = device_wall_clock(utc, |_| Some(offset(120)));
        assert_eq!(Some(wall), zoneless(2019, 6, 14, 10, 23, 0));
    }

    #[test]
    fn the_zone_lookup_gets_the_dates_own_instant() {
        // DST depends on the date, so the PC's offset must be asked for this instant, not now.
        let d = zoneless(2019, 1, 15, 12, 0, 0)
            .unwrap()
            .with_offset(offset(-300));
        let expected = d.to_unix_seconds().unwrap();
        let wall = device_wall_clock(d, |secs| {
            assert_eq!(secs, expected);
            Some(offset(60))
        });
        assert_eq!(Some(wall), zoneless(2019, 1, 15, 18, 0, 0));
    }

    #[test]
    fn an_offset_date_keeps_its_fields_when_the_pc_zone_is_unknown() {
        let d = zoneless(2019, 6, 14, 10, 23, 0).unwrap();
        let wall = device_wall_clock(d.with_offset(offset(120)), |_| None);
        assert_eq!(wall, d);
    }
}
