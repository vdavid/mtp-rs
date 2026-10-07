//! DateTime struct and serialization for MTP/PTP.

use super::{pack_string, unpack_string};

/// An offset from UTC, as an MTP datetime's `Z` or `±hhmm` suffix writes it.
///
/// Holds whole minutes east of UTC, within ±23:59 (the most `±hhmm` can express).
///
/// # Example
///
/// ```
/// use mtp_rs::ptp::UtcOffset;
///
/// let ist = UtcOffset::from_minutes(5 * 60 + 30).unwrap();
/// assert_eq!(ist.minutes(), 330);
/// assert_eq!(UtcOffset::UTC.minutes(), 0);
/// assert!(UtcOffset::from_minutes(24 * 60).is_none());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UtcOffset {
    minutes: i16,
}

impl UtcOffset {
    /// UTC itself, written as `Z`.
    pub const UTC: UtcOffset = UtcOffset { minutes: 0 };

    /// The offset `minutes` east of UTC (negative for west). `None` beyond ±23:59.
    #[must_use]
    pub const fn from_minutes(minutes: i16) -> Option<Self> {
        if minutes > -24 * 60 && minutes < 24 * 60 {
            Some(UtcOffset { minutes })
        } else {
            None
        }
    }

    /// Minutes east of UTC (negative for west).
    #[must_use]
    pub const fn minutes(self) -> i16 {
        self.minutes
    }

    /// The offset in seconds east of UTC.
    const fn seconds(self) -> i64 {
        self.minutes as i64 * 60
    }
}

/// Date and time structure for MTP/PTP.
///
/// Wire format: `YYYYMMDDThhmmss`, optionally followed by tenths of a second (`.s`) and a UTC
/// offset (`Z` or `±hhmm`), per the PTP spec's ISO 8601 subset.
///
/// Most devices write no offset. Android writes the phone's local time with none, so a parsed
/// value has [`offset`](Self::offset) `None`, which means wall-clock time in the device's zone and
/// never UTC. See [`crate::mtp::DateTime`] for how to resolve one into an instant.
///
/// # Validation
///
/// DateTime values must satisfy these constraints:
/// - Year: 0-9999 (4-digit representation)
/// - Month: 1-12
/// - Day: 1 to the month's real length, leap years included (proleptic Gregorian)
/// - Hour: 0-23
/// - Minute: 0-59
/// - Second: 0-59
///
/// Use [`DateTime::new()`] to create validated instances, or [`DateTime::is_valid()`]
/// to check existing instances.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DateTime {
    /// Year (0-9999)
    pub year: u16,
    /// Month (1-12)
    pub month: u8,
    /// Day (1 to the month's length)
    pub day: u8,
    /// Hour (0-23)
    pub hour: u8,
    /// Minute (0-59)
    pub minute: u8,
    /// Second (0-59)
    pub second: u8,
    /// The UTC offset the string carried, or `None` for a zoneless value (the usual case).
    pub offset: Option<UtcOffset>,
}

/// Seconds since the Unix epoch at 0000-01-01T00:00:00Z, the earliest instant MTP can write.
const MIN_UNIX_SECONDS: i64 = -62_167_219_200;
/// Seconds since the Unix epoch at 9999-12-31T23:59:59Z, the latest instant MTP can write.
const MAX_UNIX_SECONDS: i64 = 253_402_300_799;

const SECONDS_PER_DAY: i64 = 86_400;

fn is_leap_year(year: u16) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// The month's length in days, or 0 for a month outside 1-12.
fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days from 1970-01-01 to the given proleptic Gregorian date (Howard Hinnant's `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12; // March = 0
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian `(year, month, day)` for a day count from 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Parse exactly `N` ASCII digits. `str::parse` would also accept a leading `+`.
fn parse_digits<const N: usize>(bytes: &[u8]) -> Option<u16> {
    if bytes.len() != N || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        bytes
            .iter()
            .fold(0u16, |acc, &b| acc * 10 + u16::from(b - b'0')),
    )
}

/// Parse the optional `[.s][Z|±hhmm]` suffix. `Some(offset)` on a recognized suffix (including an
/// empty one, which is zoneless), `None` on anything else.
fn parse_suffix(mut tail: &[u8]) -> Option<Option<UtcOffset>> {
    // Tenths of a second. The spec writes one digit; accept more and drop them all.
    if let Some(rest) = tail.strip_prefix(b".") {
        let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        tail = &rest[digits..];
    }
    match tail {
        [] => Some(None),
        [b'Z'] => Some(Some(UtcOffset::UTC)),
        [sign @ (b'+' | b'-'), hhmm @ ..] => {
            let hours = parse_digits::<2>(hhmm.get(..2)?)?;
            let minutes = parse_digits::<2>(hhmm.get(2..)?)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            let total = (hours * 60 + minutes) as i16;
            UtcOffset::from_minutes(if *sign == b'-' { -total } else { total }).map(Some)
        }
        _ => None,
    }
}

impl DateTime {
    /// Create a new zoneless DateTime with validation.
    ///
    /// Returns `None` if any value is out of range, including a day past the month's real length
    /// (`2023-02-29`, `2024-04-31`).
    ///
    /// # Example
    ///
    /// ```
    /// use mtp_rs::ptp::DateTime;
    ///
    /// let dt = DateTime::new(2024, 3, 15, 14, 30, 22).unwrap();
    /// assert_eq!(dt.year, 2024);
    /// assert_eq!(dt.offset, None);
    ///
    /// // Invalid values return None
    /// assert!(DateTime::new(2024, 13, 1, 0, 0, 0).is_none()); // month > 12
    /// assert!(DateTime::new(2024, 1, 1, 0, 60, 0).is_none()); // minute > 59
    /// assert!(DateTime::new(2023, 2, 29, 0, 0, 0).is_none()); // not a leap year
    /// ```
    #[must_use]
    pub fn new(year: u16, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> Option<Self> {
        let dt = DateTime {
            year,
            month,
            day,
            hour,
            minute,
            second,
            offset: None,
        };
        if dt.is_valid() {
            Some(dt)
        } else {
            None
        }
    }

    /// The same wall-clock fields, marked as being at `offset` from UTC.
    #[must_use]
    pub fn with_offset(self, offset: UtcOffset) -> Self {
        Self {
            offset: Some(offset),
            ..self
        }
    }

    /// Check if this DateTime has valid values.
    ///
    /// Returns `true` if all fields are within valid ranges:
    /// - Year: 0-9999
    /// - Month: 1-12
    /// - Day: 1 to the month's real length, leap years included
    /// - Hour: 0-23
    /// - Minute: 0-59
    /// - Second: 0-59
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.year <= 9999
            && (1..=days_in_month(self.year, self.month)).contains(&self.day)
            && self.hour <= 23
            && self.minute <= 59
            && self.second <= 59
    }

    /// Parse a datetime string in MTP format.
    ///
    /// Format: `YYYYMMDDThhmmss`, then optional tenths of a second (`.s`, accepted and dropped),
    /// then an optional UTC offset (`Z` or `±hhmm`, kept in [`offset`](Self::offset)).
    ///
    /// Returns `None` if the string is malformed, contains invalid values, or ends in anything
    /// else: an unrecognized suffix might be an offset, and reporting the value as zoneless would
    /// silently shift it.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        if b.len() < 15 || b[8] != b'T' {
            return None;
        }
        let narrow = |v: u16| u8::try_from(v).ok();
        let mut dt = Self::new(
            parse_digits::<4>(&b[0..4])?,
            narrow(parse_digits::<2>(&b[4..6])?)?,
            narrow(parse_digits::<2>(&b[6..8])?)?,
            narrow(parse_digits::<2>(&b[9..11])?)?,
            narrow(parse_digits::<2>(&b[11..13])?)?,
            narrow(parse_digits::<2>(&b[13..15])?)?,
        )?;
        dt.offset = parse_suffix(&b[15..])?;
        Some(dt)
    }

    /// The instant at `secs` seconds since the Unix epoch, as wall-clock time at `offset`. The
    /// result carries that offset. `None` when the year falls outside 0-9999.
    #[must_use]
    pub fn from_unix_seconds(secs: i64, offset: UtcOffset) -> Option<Self> {
        let local = secs.checked_add(offset.seconds())?;
        if !(MIN_UNIX_SECONDS..=MAX_UNIX_SECONDS).contains(&local) {
            return None;
        }
        let (year, month, day) = civil_from_days(local.div_euclid(SECONDS_PER_DAY));
        let time = local.rem_euclid(SECONDS_PER_DAY);
        // Every value below fits its field: the range check bounds the year to 0-9999.
        Some(DateTime {
            year: year as u16,
            month: month as u8,
            day: day as u8,
            hour: (time / 3600) as u8,
            minute: (time / 60 % 60) as u8,
            second: (time % 60) as u8,
            offset: Some(offset),
        })
    }

    /// Seconds since the Unix epoch, when the value carries an offset. `None` for a zoneless value
    /// or invalid fields.
    #[must_use]
    pub fn to_unix_seconds(&self) -> Option<i64> {
        self.offset
            .and_then(|o| self.to_unix_seconds_with_fallback(o))
    }

    /// Seconds since the Unix epoch, reading a zoneless value as wall-clock time at `fallback`.
    /// A value with its own offset uses that one. `None` only for invalid fields.
    #[must_use]
    pub fn to_unix_seconds_with_fallback(&self, fallback: UtcOffset) -> Option<i64> {
        if !self.is_valid() {
            return None;
        }
        let days = days_from_civil(
            i64::from(self.year),
            i64::from(self.month),
            i64::from(self.day),
        );
        let time =
            i64::from(self.hour) * 3600 + i64::from(self.minute) * 60 + i64::from(self.second);
        Some(days * SECONDS_PER_DAY + time - self.offset.unwrap_or(fallback).seconds())
    }

    /// Format the datetime as an MTP string.
    ///
    /// Returns `Some("YYYYMMDDThhmmss")` if the values are valid, followed by `Z` or `±hhmm` when
    /// the value carries an offset. Returns `None` if any value is out of range.
    ///
    /// # Example
    ///
    /// ```
    /// use mtp_rs::ptp::{DateTime, UtcOffset};
    ///
    /// let dt = DateTime::new(2024, 3, 15, 14, 30, 22).unwrap();
    /// assert_eq!(dt.format(), Some("20240315T143022".to_string()));
    /// assert_eq!(
    ///     dt.with_offset(UtcOffset::UTC).format(),
    ///     Some("20240315T143022Z".to_string())
    /// );
    ///
    /// // Invalid DateTime returns None
    /// let invalid = DateTime { month: 13, ..dt };
    /// assert_eq!(invalid.format(), None);
    /// ```
    #[must_use]
    pub fn format(&self) -> Option<String> {
        if !self.is_valid() {
            return None;
        }
        let mut s = format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        );
        match self.offset {
            None => {}
            Some(UtcOffset::UTC) => s.push('Z'),
            Some(o) => {
                let sign = if o.minutes < 0 { '-' } else { '+' };
                let abs = o.minutes.unsigned_abs();
                s.push_str(&format!("{sign}{:02}{:02}", abs / 60, abs % 60));
            }
        }
        Some(s)
    }
}

/// Pack a DateTime into MTP string format.
///
/// Returns an error if the DateTime contains invalid values.
pub fn pack_datetime(dt: &DateTime) -> Result<Vec<u8>, crate::PtpError> {
    let formatted = dt.format().ok_or_else(|| {
        crate::PtpError::invalid_data(format!(
            "invalid DateTime: year={}, month={}, day={}, hour={}, minute={}, second={}",
            dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
        ))
    })?;
    Ok(pack_string(&formatted))
}

/// Unpack a DateTime from a buffer.
///
/// Returns the datetime (or None for an empty or unparseable string) and the
/// number of bytes consumed.
///
/// Unparseable datetimes are treated as absent rather than as an error:
/// datetime fields are informational metadata, and real devices put
/// non-spec values in them. The Panasonic Lumix DMC-TZ61 reports
/// `20480000T000000` (month 0, day 0) as its "no date" sentinel, and a hard
/// error here would fail the whole `ObjectInfo` parse and with it the whole
/// listing (issue #12). The string itself was already length-prefixed and
/// consumed correctly, so only its interpretation is skipped.
pub fn unpack_datetime(buf: &[u8]) -> Result<(Option<DateTime>, usize), crate::PtpError> {
    let (s, consumed) = unpack_string(buf)?;
    Ok((DateTime::parse(&s), consumed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // --- DateTime parsing tests ---

    #[test]
    fn datetime_parse_basic() {
        let dt = DateTime::parse("20240315T143022").unwrap();
        assert_eq!(
            (dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second),
            (2024, 3, 15, 14, 30, 22)
        );
    }

    #[test]
    fn datetime_parse_with_timezone_z() {
        assert!(DateTime::parse("20240315T143022Z").is_some());
    }

    #[test]
    fn datetime_parse_with_timezone_positive() {
        assert!(DateTime::parse("20240315T143022+0530").is_some());
    }

    #[test]
    fn datetime_parse_with_timezone_negative() {
        assert!(DateTime::parse("20240315T143022-0800").is_some());
    }

    #[test]
    fn datetime_parse_invalid_too_short() {
        for s in ["2024031", ""] {
            assert!(DateTime::parse(s).is_none());
        }
    }

    #[test]
    fn datetime_parse_invalid_no_t_separator() {
        for s in ["20240315 143022", "20240315143022"] {
            assert!(DateTime::parse(s).is_none());
        }
    }

    #[test]
    fn datetime_parse_invalid_month() {
        for s in ["20240015T143022", "20241315T143022"] {
            // month=0, month=13
            assert!(DateTime::parse(s).is_none());
        }
    }

    #[test]
    fn datetime_parse_invalid_day() {
        for s in ["20240100T143022", "20240132T143022"] {
            // day=0, day=32
            assert!(DateTime::parse(s).is_none());
        }
    }

    #[test]
    fn datetime_parse_invalid_hour() {
        assert!(DateTime::parse("20240315T243022").is_none()); // hour=24
    }

    #[test]
    fn datetime_parse_invalid_minute() {
        assert!(DateTime::parse("20240315T146022").is_none()); // minute=60
    }

    #[test]
    fn datetime_parse_invalid_second() {
        assert!(DateTime::parse("20240315T143060").is_none()); // second=60
    }

    // --- Offsets, fractions, and exact parsing ---

    fn offset(minutes: i16) -> UtcOffset {
        UtcOffset::from_minutes(minutes).unwrap()
    }

    #[test]
    fn datetime_parse_without_suffix_is_zoneless() {
        // Android writes this form, in the phone's local time: there's no
        // offset to report, and inventing UTC would shift the time silently.
        assert_eq!(DateTime::parse("20240315T143022").unwrap().offset, None);
    }

    #[test]
    fn datetime_parse_keeps_the_offset() {
        for (s, want) in [
            ("20240315T143022Z", UtcOffset::UTC),
            ("20240315T143022+0530", offset(330)),
            ("20240315T143022-0800", offset(-480)),
            ("20240315T143022+0000", UtcOffset::UTC),
        ] {
            let dt = DateTime::parse(s).unwrap();
            assert_eq!(dt.offset, Some(want), "{s}");
            assert_eq!((dt.hour, dt.minute, dt.second), (14, 30, 22), "{s}");
        }
    }

    #[test]
    fn datetime_parse_accepts_and_drops_tenths() {
        for (s, want) in [
            ("20240315T143022.5", None),
            ("20240315T143022.5Z", Some(UtcOffset::UTC)),
            ("20240315T143022.0-0800", Some(offset(-480))),
        ] {
            let dt = DateTime::parse(s).unwrap();
            assert_eq!((dt.second, dt.offset), (22, want), "{s}");
        }
    }

    #[test]
    fn datetime_parse_rejects_an_unrecognized_suffix() {
        // A suffix we can't read might be an offset we'd be ignoring, so the
        // value is dropped rather than reported as zoneless.
        for s in [
            "20240315T143022X",
            "20240315T143022z",
            "20240315T143022Z1",
            "20240315T143022 ",
            "20240315T143022.",
            "20240315T143022.Z",
            "20240315T143022+05",
            "20240315T143022+05:30",
            "20240315T143022+2400",
            "20240315T143022+0560",
            "20240315T143022+05300",
        ] {
            assert!(DateTime::parse(s).is_none(), "{s}");
        }
    }

    #[test]
    fn datetime_parse_rejects_signs_inside_numeric_fields() {
        // `u16::from_str` accepts a leading `+`, which would let these through.
        for s in ["+0240315T143022", "2024+315T143022", "20240315T+43022"] {
            assert!(DateTime::parse(s).is_none(), "{s}");
        }
    }

    #[test]
    fn datetime_parse_checks_real_month_lengths() {
        for (s, valid) in [
            ("20240229T000000", true),  // leap year
            ("20230229T000000", false), // common year
            ("20000229T000000", true),  // divisible by 400
            ("21000229T000000", false), // divisible by 100 only
            ("20240430T000000", true),
            ("20240431T000000", false),
            ("20240631T000000", false),
            ("20240931T000000", false),
            ("20241131T000000", false),
            ("20241231T000000", true),
        ] {
            assert_eq!(DateTime::parse(s).is_some(), valid, "{s}");
        }
    }

    #[test]
    fn datetime_new_checks_real_month_lengths() {
        assert!(DateTime::new(2024, 2, 29, 0, 0, 0).is_some());
        assert!(DateTime::new(2023, 2, 29, 0, 0, 0).is_none());
        assert!(DateTime::new(2024, 4, 31, 0, 0, 0).is_none());
    }

    #[test]
    fn datetime_format_writes_the_offset() {
        let dt = DateTime::new(2024, 3, 15, 14, 30, 22).unwrap();
        assert_eq!(dt.format().unwrap(), "20240315T143022");
        assert_eq!(
            dt.with_offset(UtcOffset::UTC).format().unwrap(),
            "20240315T143022Z"
        );
        assert_eq!(
            dt.with_offset(offset(330)).format().unwrap(),
            "20240315T143022+0530"
        );
        assert_eq!(
            dt.with_offset(offset(-480)).format().unwrap(),
            "20240315T143022-0800"
        );
    }

    #[test]
    fn utc_offset_bounds() {
        assert_eq!(
            UtcOffset::from_minutes(1439).map(UtcOffset::minutes),
            Some(1439)
        );
        assert_eq!(
            UtcOffset::from_minutes(-1439).map(UtcOffset::minutes),
            Some(-1439)
        );
        assert!(UtcOffset::from_minutes(1440).is_none());
        assert!(UtcOffset::from_minutes(-1440).is_none());
    }

    #[test]
    fn datetime_to_unix_seconds_needs_an_offset() {
        let dt = DateTime::new(2024, 3, 15, 14, 30, 22).unwrap();
        assert_eq!(dt.to_unix_seconds(), None);
        assert_eq!(
            dt.with_offset(UtcOffset::UTC).to_unix_seconds(),
            Some(1_710_513_022)
        );
        // 14:30:22 at +05:30 is 09:00:22 UTC.
        assert_eq!(
            dt.with_offset(offset(330)).to_unix_seconds(),
            Some(1_710_513_022 - 330 * 60)
        );
    }

    #[test]
    fn datetime_to_unix_seconds_with_fallback_prefers_its_own_offset() {
        let dt = DateTime::new(2024, 3, 15, 14, 30, 22).unwrap();
        assert_eq!(
            dt.to_unix_seconds_with_fallback(offset(60)),
            Some(1_710_513_022 - 3600)
        );
        assert_eq!(
            dt.with_offset(UtcOffset::UTC)
                .to_unix_seconds_with_fallback(offset(60)),
            Some(1_710_513_022)
        );
    }

    #[test]
    fn datetime_to_unix_seconds_rejects_invalid_fields() {
        let mut dt = DateTime::new(2024, 3, 15, 14, 30, 22)
            .unwrap()
            .with_offset(UtcOffset::UTC);
        dt.day = 31;
        dt.month = 4;
        assert_eq!(dt.to_unix_seconds(), None);
    }

    #[test]
    fn datetime_from_unix_seconds_known_values() {
        let cases = [
            (0, UtcOffset::UTC, (1970, 1, 1, 0, 0, 0)),
            (-1, UtcOffset::UTC, (1969, 12, 31, 23, 59, 59)),
            (951_782_400, UtcOffset::UTC, (2000, 2, 29, 0, 0, 0)),
            (1_710_513_022, UtcOffset::UTC, (2024, 3, 15, 14, 30, 22)),
            (0, offset(330), (1970, 1, 1, 5, 30, 0)),
            (0, offset(-60), (1969, 12, 31, 23, 0, 0)),
            (-62_167_219_200, UtcOffset::UTC, (0, 1, 1, 0, 0, 0)),
            (253_402_300_799, UtcOffset::UTC, (9999, 12, 31, 23, 59, 59)),
        ];
        for (secs, off, want) in cases {
            let dt = DateTime::from_unix_seconds(secs, off).unwrap();
            assert_eq!(
                (dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second),
                want,
                "{secs} at {off:?}"
            );
            assert_eq!(dt.offset, Some(off));
        }
    }

    #[test]
    fn datetime_from_unix_seconds_rejects_years_mtp_cannot_write() {
        assert!(DateTime::from_unix_seconds(-62_167_219_201, UtcOffset::UTC).is_none());
        assert!(DateTime::from_unix_seconds(253_402_300_800, UtcOffset::UTC).is_none());
        assert!(DateTime::from_unix_seconds(i64::MIN, UtcOffset::UTC).is_none());
        assert!(DateTime::from_unix_seconds(i64::MAX, UtcOffset::UTC).is_none());
    }

    // --- DateTime format tests ---

    #[test]
    fn datetime_format() {
        assert_eq!(
            DateTime::new(2024, 3, 15, 14, 30, 22).unwrap().format(),
            Some("20240315T143022".into())
        );
    }

    #[test]
    fn datetime_format_with_leading_zeros() {
        assert_eq!(
            DateTime::new(2024, 1, 5, 9, 5, 3).unwrap().format(),
            Some("20240105T090503".into())
        );
    }

    #[test]
    fn datetime_roundtrip() {
        let original = DateTime::new(2024, 12, 31, 23, 59, 59).unwrap();
        assert_eq!(
            DateTime::parse(&original.format().unwrap()).unwrap(),
            original
        );
    }

    #[test]
    fn datetime_format_invalid_returns_none() {
        let invalid_cases = [
            (2024, 13, 1, 0, 0, 0), // invalid month
            (2024, 1, 1, 0, 60, 0), // invalid minute
            (10000, 1, 1, 0, 0, 0), // year too large
        ];
        for (y, mo, d, h, mi, s) in invalid_cases {
            let dt = DateTime {
                year: y,
                month: mo,
                day: d,
                hour: h,
                minute: mi,
                second: s,
                offset: None,
            };
            assert_eq!(dt.format(), None);
        }
    }

    #[test]
    fn datetime_default() {
        let dt = DateTime::default();
        assert_eq!(
            (dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second),
            (0, 0, 0, 0, 0, 0)
        );
    }

    // --- Pack/unpack tests ---

    #[test]
    fn pack_datetime_basic() {
        let packed = pack_datetime(&DateTime::new(2024, 3, 15, 14, 30, 22).unwrap()).unwrap();
        assert_eq!(packed[0], 16); // 15 chars + null terminator
    }

    #[test]
    fn pack_datetime_invalid_returns_error() {
        let invalid = DateTime {
            year: 2024,
            month: 13,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            offset: None,
        };
        assert!(pack_datetime(&invalid).is_err());
    }

    #[test]
    fn unpack_datetime_basic() {
        let dt = DateTime::new(2024, 3, 15, 14, 30, 22).unwrap();
        let (unpacked, _) = unpack_datetime(&pack_datetime(&dt).unwrap()).unwrap();
        assert_eq!(unpacked, Some(dt));
    }

    #[test]
    fn unpack_datetime_empty_string() {
        let (dt, consumed) = unpack_datetime(&[0x00]).unwrap();
        assert_eq!((dt, consumed), (None, 1));
    }

    #[test]
    fn unpack_datetime_unparseable_string_returns_none() {
        // Receive-side leniency: garbage in a datetime field must not fail the
        // surrounding dataset parse (it's informational metadata).
        let packed = pack_string("not a date");
        let (dt, consumed) = unpack_datetime(&packed).unwrap();
        assert_eq!(dt, None);
        assert_eq!(consumed, packed.len());
    }

    #[test]
    fn unpack_datetime_panasonic_no_date_sentinel_returns_none() {
        // Panasonic Lumix DMC-TZ61 reports "20480000T000000" (month 0, day 0)
        // as its "no date" value. Seen in the wild (issue #12); must parse as
        // None, not error.
        let packed = pack_string("20480000T000000");
        let (dt, consumed) = unpack_datetime(&packed).unwrap();
        assert_eq!(dt, None);
        assert_eq!(consumed, packed.len());
    }

    #[test]
    fn datetime_pack_unpack_roundtrip() {
        for dt in [
            DateTime::new(2024, 1, 1, 0, 0, 0).unwrap(),
            DateTime::new(2024, 12, 31, 23, 59, 59).unwrap(),
            DateTime::new(1999, 6, 15, 12, 30, 45).unwrap(),
        ] {
            let (unpacked, _) = unpack_datetime(&pack_datetime(&dt).unwrap()).unwrap();
            assert_eq!(unpacked, Some(dt));
        }
    }

    // --- Boundary tests ---

    #[test]
    fn datetime_boundary_day_31() {
        let dt = DateTime {
            year: 2024,
            month: 1,
            day: 31,
            hour: 0,
            minute: 0,
            second: 0,
            offset: None,
        };
        let parsed = DateTime::parse(&dt.format().unwrap()).unwrap();
        assert_eq!(parsed.day, 31);
    }

    #[test]
    fn datetime_boundary_year_0() {
        let dt = DateTime {
            year: 0,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            offset: None,
        };
        if let Some(p) = DateTime::parse(&dt.format().unwrap()) {
            assert_eq!(p.year, 0);
        }
    }

    #[test]
    fn datetime_boundary_year_9999() {
        let dt = DateTime {
            year: 9999,
            month: 12,
            day: 31,
            hour: 23,
            minute: 59,
            second: 59,
            offset: None,
        };
        assert_eq!(DateTime::parse(&dt.format().unwrap()).unwrap().year, 9999);
    }

    #[test]
    fn datetime_boundary_year_10000() {
        let dt = DateTime {
            year: 10000,
            month: 1,
            day: 1,
            hour: 0,
            minute: 0,
            second: 0,
            offset: None,
        };
        assert!(dt.format().is_none());
        assert!(pack_datetime(&dt).is_err());
    }

    // --- Property-based tests ---

    fn valid_datetime() -> impl Strategy<Value = DateTime> {
        (
            1000u16..9999u16,
            1u8..=12u8,
            1u8..=28u8,
            0u8..=23u8,
            0u8..=59u8,
            0u8..=59u8,
        )
            .prop_map(|(year, month, day, hour, minute, second)| DateTime {
                year,
                month,
                day,
                hour,
                minute,
                second,
                offset: None,
            })
    }

    proptest! {
        #[test]
        fn prop_datetime_format_parse_roundtrip(dt in valid_datetime()) {
            let formatted = dt.format().unwrap();
            prop_assert_eq!(DateTime::parse(&formatted).unwrap(), dt);
        }

        #[test]
        fn prop_datetime_pack_unpack_roundtrip(dt in valid_datetime()) {
            let packed = pack_datetime(&dt).unwrap();
            let (unpacked, consumed) = unpack_datetime(&packed).unwrap();
            prop_assert_eq!(unpacked, Some(dt));
            prop_assert_eq!(consumed, packed.len());
        }

        #[test]
        fn prop_datetime_format_length(dt in valid_datetime()) {
            prop_assert_eq!(dt.format().unwrap().len(), 15);
        }

        #[test]
        fn prop_datetime_with_offset_format_parse_roundtrip(
            dt in valid_datetime(),
            minutes in -1439i16..=1439i16,
        ) {
            let dt = dt.with_offset(UtcOffset::from_minutes(minutes).unwrap());
            prop_assert_eq!(DateTime::parse(&dt.format().unwrap()).unwrap(), dt);
        }

        #[test]
        fn prop_datetime_unix_seconds_roundtrip(
            // A day inside each end of the 0000-9999 range, so every offset fits.
            secs in -62_167_132_800i64..=253_402_214_399i64,
            minutes in -1439i16..=1439i16,
        ) {
            let off = UtcOffset::from_minutes(minutes).unwrap();
            let dt = DateTime::from_unix_seconds(secs, off).unwrap();
            prop_assert!(dt.is_valid());
            prop_assert_eq!(dt.to_unix_seconds(), Some(secs));
        }

        #[test]
        fn prop_datetime_parse_accepts_every_real_day(
            year in 0u16..=9999u16,
            month in 1u8..=12u8,
            day in 1u8..=31u8,
        ) {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            let len = match month {
                2 if leap => 29,
                2 => 28,
                4 | 6 | 9 | 11 => 30,
                _ => 31,
            };
            let s = format!("{year:04}{month:02}{day:02}T000000");
            prop_assert_eq!(DateTime::parse(&s).is_some(), day <= len);
        }

        #[test]
        fn fuzz_datetime_invalid_month(
            year in 1900u16..2100u16,
            month in prop::sample::select(vec![0u8, 13, 14, 99, 255]),
            day in 1u8..=28u8, hour in 0u8..=23u8, minute in 0u8..=59u8, second in 0u8..=59u8,
        ) {
            let dt = DateTime { year, month, day, hour, minute, second, offset: None };
            prop_assert!(dt.format().is_none());
            prop_assert!(pack_datetime(&dt).is_err());
        }

        #[test]
        fn fuzz_datetime_invalid_day(
            year in 1900u16..2100u16, month in 1u8..=12u8,
            day in prop::sample::select(vec![0u8, 32, 33, 99, 255]),
            hour in 0u8..=23u8, minute in 0u8..=59u8, second in 0u8..=59u8,
        ) {
            let dt = DateTime { year, month, day, hour, minute, second, offset: None };
            prop_assert!(dt.format().is_none());
            prop_assert!(pack_datetime(&dt).is_err());
        }

        #[test]
        fn fuzz_datetime_invalid_hour(
            year in 1900u16..2100u16, month in 1u8..=12u8, day in 1u8..=28u8,
            hour in prop::sample::select(vec![24u8, 25, 99, 255]),
            minute in 0u8..=59u8, second in 0u8..=59u8,
        ) {
            let dt = DateTime { year, month, day, hour, minute, second, offset: None };
            prop_assert!(dt.format().is_none());
            prop_assert!(pack_datetime(&dt).is_err());
        }

        #[test]
        fn fuzz_datetime_invalid_minute(
            year in 1900u16..2100u16, month in 1u8..=12u8, day in 1u8..=28u8, hour in 0u8..=23u8,
            minute in prop::sample::select(vec![60u8, 61, 99]),
            second in 0u8..=59u8,
        ) {
            let dt = DateTime { year, month, day, hour, minute, second, offset: None };
            prop_assert!(dt.format().is_none());
            prop_assert!(pack_datetime(&dt).is_err());
        }

        #[test]
        fn fuzz_datetime_minute_overflow(
            year in 1900u16..2100u16, month in 1u8..=12u8, day in 1u8..=28u8, hour in 0u8..=23u8,
            minute in 100u8..=255u8, second in 0u8..=59u8,
        ) {
            let dt = DateTime { year, month, day, hour, minute, second, offset: None };
            prop_assert!(dt.format().is_none());
            prop_assert!(pack_datetime(&dt).is_err());
        }

        #[test]
        fn fuzz_datetime_invalid_second(
            year in 1900u16..2100u16, month in 1u8..=12u8, day in 1u8..=28u8, hour in 0u8..=23u8, minute in 0u8..=59u8,
            second in prop::sample::select(vec![60u8, 61, 99]),
        ) {
            let dt = DateTime { year, month, day, hour, minute, second, offset: None };
            prop_assert!(dt.format().is_none());
            prop_assert!(pack_datetime(&dt).is_err());
        }

        #[test]
        fn fuzz_datetime_second_overflow(
            year in 1900u16..2100u16, month in 1u8..=12u8, day in 1u8..=28u8, hour in 0u8..=23u8, minute in 0u8..=59u8,
            second in 100u8..=255u8,
        ) {
            let dt = DateTime { year, month, day, hour, minute, second, offset: None };
            prop_assert!(dt.format().is_none());
            prop_assert!(pack_datetime(&dt).is_err());
        }

        #[test]
        fn fuzz_datetime_parse_garbage(s in ".*") {
            let _ = DateTime::parse(&s);
        }

        #[test]
        fn fuzz_datetime_parse_malformed(prefix in "[0-9]{0,20}", suffix in "[^T]*") {
            let _ = DateTime::parse(&format!("{}{}", prefix, suffix));
        }
    }

    crate::fuzz_bytes_fn!(fuzz_unpack_datetime, unpack_datetime, 50);
}
