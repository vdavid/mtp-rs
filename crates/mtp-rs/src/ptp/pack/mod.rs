//! Binary serialization/deserialization primitives for MTP/PTP.
//!
//! All multi-byte values are little-endian.

mod datetime;

pub use datetime::{pack_datetime, unpack_datetime, DateTime, UtcOffset};

// --- Primitive packing functions ---

/// Pack a u8 value into a 1-byte array.
#[inline]
pub fn pack_u8(val: u8) -> [u8; 1] {
    [val]
}

/// Pack a u16 value into a 2-byte array (little-endian).
#[inline]
pub fn pack_u16(val: u16) -> [u8; 2] {
    val.to_le_bytes()
}

/// Pack a u32 value into a 4-byte array (little-endian).
#[inline]
pub fn pack_u32(val: u32) -> [u8; 4] {
    val.to_le_bytes()
}

/// Pack a u64 value into an 8-byte array (little-endian).
#[inline]
pub fn pack_u64(val: u64) -> [u8; 8] {
    val.to_le_bytes()
}

/// Pack a signed 8-bit integer.
#[inline]
pub fn pack_i8(val: i8) -> [u8; 1] {
    [val as u8]
}

/// Pack a signed 16-bit integer (little-endian).
#[inline]
pub fn pack_i16(val: i16) -> [u8; 2] {
    val.to_le_bytes()
}

/// Pack a signed 32-bit integer (little-endian).
#[inline]
pub fn pack_i32(val: i32) -> [u8; 4] {
    val.to_le_bytes()
}

/// Pack a signed 64-bit integer (little-endian).
#[inline]
pub fn pack_i64(val: i64) -> [u8; 8] {
    val.to_le_bytes()
}

// --- Primitive unpacking functions ---

/// Unpack a u8 value from a buffer.
pub fn unpack_u8(buf: &[u8]) -> Result<u8, crate::PtpError> {
    if buf.is_empty() {
        return Err(crate::PtpError::invalid_data(
            "insufficient bytes for u8: need 1, have 0",
        ));
    }
    Ok(buf[0])
}

/// Unpack a u16 value from a buffer (little-endian).
pub fn unpack_u16(buf: &[u8]) -> Result<u16, crate::PtpError> {
    if buf.len() < 2 {
        return Err(crate::PtpError::invalid_data(format!(
            "insufficient bytes for u16: need 2, have {}",
            buf.len()
        )));
    }
    Ok(u16::from_le_bytes([buf[0], buf[1]]))
}

/// Unpack a u32 value from a buffer (little-endian).
pub fn unpack_u32(buf: &[u8]) -> Result<u32, crate::PtpError> {
    if buf.len() < 4 {
        return Err(crate::PtpError::invalid_data(format!(
            "insufficient bytes for u32: need 4, have {}",
            buf.len()
        )));
    }
    Ok(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

/// Unpack a u64 value from a buffer (little-endian).
pub fn unpack_u64(buf: &[u8]) -> Result<u64, crate::PtpError> {
    if buf.len() < 8 {
        return Err(crate::PtpError::invalid_data(format!(
            "insufficient bytes for u64: need 8, have {}",
            buf.len()
        )));
    }
    Ok(u64::from_le_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]))
}

/// Unpack a signed 8-bit integer from a buffer.
pub fn unpack_i8(buf: &[u8]) -> Result<i8, crate::PtpError> {
    if buf.is_empty() {
        return Err(crate::PtpError::invalid_data(
            "insufficient bytes for i8: need 1, have 0",
        ));
    }
    Ok(buf[0] as i8)
}

/// Unpack a signed 16-bit integer from a buffer (little-endian).
pub fn unpack_i16(buf: &[u8]) -> Result<i16, crate::PtpError> {
    if buf.len() < 2 {
        return Err(crate::PtpError::invalid_data(format!(
            "insufficient bytes for i16: need 2, have {}",
            buf.len()
        )));
    }
    Ok(i16::from_le_bytes([buf[0], buf[1]]))
}

/// Unpack a signed 32-bit integer from a buffer (little-endian).
pub fn unpack_i32(buf: &[u8]) -> Result<i32, crate::PtpError> {
    if buf.len() < 4 {
        return Err(crate::PtpError::invalid_data(format!(
            "insufficient bytes for i32: need 4, have {}",
            buf.len()
        )));
    }
    Ok(i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

/// Unpack a signed 64-bit integer from a buffer (little-endian).
pub fn unpack_i64(buf: &[u8]) -> Result<i64, crate::PtpError> {
    if buf.len() < 8 {
        return Err(crate::PtpError::invalid_data(format!(
            "insufficient bytes for i64: need 8, have {}",
            buf.len()
        )));
    }
    Ok(i64::from_le_bytes([
        buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
    ]))
}

// --- String encoding/decoding ---

/// The longest string PTP can carry, in UTF-16 code units, not counting the null terminator.
///
/// A PTP string's one-byte length prefix counts the terminator too, so it tops out at 255 units
/// including it (PIMA 15740 § 5.3.4). AOSP's MTP server uses the same cap
/// (`MTP_STRING_MAX_CHARACTER_NUMBER = 255` in `MtpStringBuffer.cpp`). An emoji or other
/// character outside the Basic Multilingual Plane takes two units.
pub const MAX_STRING_UNITS: usize = 254;

/// Pack a string into MTP format (UTF-16LE with length prefix).
///
/// MTP strings are encoded as:
/// 1. 1 byte: Number of characters (including null terminator)
/// 2. N * 2 bytes: UTF-16LE encoded characters
/// 3. 2 bytes: Null terminator (0x0000)
///
/// Empty string: Single byte 0x00
///
/// # Errors
///
/// [`PtpError::StringTooLong`](crate::PtpError::StringTooLong) when `s` is longer than
/// [`MAX_STRING_UNITS`] UTF-16 code units. The length byte can't express it, and truncating would
/// silently write something other than what the caller asked for (a different file name).
pub fn pack_string(s: &str) -> Result<Vec<u8>, crate::PtpError> {
    if s.is_empty() {
        return Ok(vec![0x00]);
    }

    // Encode to UTF-16
    let utf16: Vec<u16> = s.encode_utf16().collect();
    if utf16.len() > MAX_STRING_UNITS {
        return Err(crate::PtpError::StringTooLong { units: utf16.len() });
    }

    // Length includes the null terminator; the check above keeps it within a u8.
    let len = utf16.len() + 1;

    // Allocate result: 1 byte length + (len * 2) bytes for UTF-16 data
    let mut result = Vec::with_capacity(1 + len * 2);

    // Length byte (number of characters including null terminator)
    result.push(len as u8);

    // UTF-16LE encoded characters
    for code_unit in &utf16 {
        result.extend_from_slice(&code_unit.to_le_bytes());
    }

    // Null terminator
    result.extend_from_slice(&[0x00, 0x00]);

    Ok(result)
}

/// Unpack an MTP string from a buffer.
///
/// Returns the decoded string and the number of bytes consumed. A lone surrogate becomes U+FFFD.
pub fn unpack_string(buf: &[u8]) -> Result<(String, usize), crate::PtpError> {
    if buf.is_empty() {
        return Err(crate::PtpError::invalid_data(
            "insufficient bytes for string length",
        ));
    }

    let len = buf[0] as usize;

    // Empty string
    if len == 0 {
        return Ok((String::new(), 1));
    }

    // Calculate required bytes: 1 (length) + len * 2 (UTF-16 code units)
    let required = 1 + len * 2;
    if buf.len() < required {
        return Err(crate::PtpError::invalid_data(format!(
            "insufficient bytes for string: need {}, have {}",
            required,
            buf.len()
        )));
    }

    // Decode UTF-16LE code units, stopping at the first NUL. PTP strings are
    // null-terminated, so anything after the first NUL is padding garbage.
    // Some devices (Panasonic Lumix DMC-TZ61, issue #12) pad fixed-width
    // fields like the serial number with multiple NULs; keeping them would
    // embed control characters in the decoded String.
    let mut code_units = Vec::with_capacity(len);
    for i in 0..len {
        let offset = 1 + i * 2;
        let code_unit = u16::from_le_bytes([buf[offset], buf[offset + 1]]);
        if code_unit == 0 {
            break;
        }
        code_units.push(code_unit);
    }

    // Lossy on purpose: a device that cuts an over-long name at the cap can split a surrogate pair
    // (#50), and failing here would fail the whole object-info read, so one bad name would hide
    // its whole folder. Handles address objects, so a replaced character doesn't lose the object.
    Ok((String::from_utf16_lossy(&code_units), required))
}

// --- Array encoding/decoding ---

/// Pack a u16 array into MTP format.
///
/// Arrays are encoded as:
/// 1. 4 bytes: Element count (u32, little-endian)
/// 2. N * 2 bytes: Elements (u16, little-endian each)
pub fn pack_u16_array(arr: &[u16]) -> Vec<u8> {
    let mut result = Vec::with_capacity(4 + arr.len() * 2);

    // Element count
    result.extend_from_slice(&pack_u32(arr.len() as u32));

    // Elements
    for &val in arr {
        result.extend_from_slice(&pack_u16(val));
    }

    result
}

/// Pack a u32 array into MTP format.
///
/// Arrays are encoded as:
/// 1. 4 bytes: Element count (u32, little-endian)
/// 2. N * 4 bytes: Elements (u32, little-endian each)
pub fn pack_u32_array(arr: &[u32]) -> Vec<u8> {
    let mut result = Vec::with_capacity(4 + arr.len() * 4);

    // Element count
    result.extend_from_slice(&pack_u32(arr.len() as u32));

    // Elements
    for &val in arr {
        result.extend_from_slice(&pack_u32(val));
    }

    result
}

/// Unpack a u16 array from a buffer.
///
/// Returns the array and the number of bytes consumed.
pub fn unpack_u16_array(buf: &[u8]) -> Result<(Vec<u16>, usize), crate::PtpError> {
    if buf.len() < 4 {
        return Err(crate::PtpError::invalid_data(format!(
            "insufficient bytes for array count: need 4, have {}",
            buf.len()
        )));
    }

    let count = unpack_u32(buf)? as usize;
    let required = 4 + count * 2;

    if buf.len() < required {
        return Err(crate::PtpError::invalid_data(format!(
            "insufficient bytes for u16 array: need {}, have {}",
            required,
            buf.len()
        )));
    }

    let mut result = Vec::with_capacity(count);
    for i in 0..count {
        let offset = 4 + i * 2;
        result.push(unpack_u16(&buf[offset..])?);
    }

    Ok((result, required))
}

/// Unpack a u32 array from a buffer.
///
/// Returns the array and the number of bytes consumed.
pub fn unpack_u32_array(buf: &[u8]) -> Result<(Vec<u32>, usize), crate::PtpError> {
    if buf.len() < 4 {
        return Err(crate::PtpError::invalid_data(format!(
            "insufficient bytes for array count: need 4, have {}",
            buf.len()
        )));
    }

    let count = unpack_u32(buf)? as usize;
    let required = 4 + count * 4;

    if buf.len() < required {
        return Err(crate::PtpError::invalid_data(format!(
            "insufficient bytes for u32 array: need {}, have {}",
            required,
            buf.len()
        )));
    }

    let mut result = Vec::with_capacity(count);
    for i in 0..count {
        let offset = 4 + i * 4;
        result.push(unpack_u32(&buf[offset..])?);
    }

    Ok((result, required))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // --- Primitive pack/unpack tests ---

    #[test]
    fn pack_primitives_little_endian() {
        // Verify little-endian byte order
        assert_eq!(pack_u16(0x1234), [0x34, 0x12]);
        assert_eq!(pack_u32(0x12345678), [0x78, 0x56, 0x34, 0x12]);
        assert_eq!(
            pack_u64(0x0102030405060708),
            [0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]
        );
        assert_eq!(pack_i16(-1), [0xFF, 0xFF]);
        assert_eq!(pack_i32(-1), [0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn unpack_insufficient_bytes() {
        assert!(unpack_u8(&[]).is_err());
        assert!(unpack_u16(&[0x00]).is_err());
        assert!(unpack_u32(&[0x00, 0x00, 0x00]).is_err());
        assert!(unpack_u64(&[0x00; 7]).is_err());
        assert!(unpack_i8(&[]).is_err());
        assert!(unpack_i16(&[0x00]).is_err());
        assert!(unpack_i32(&[0x00, 0x00, 0x00]).is_err());
        assert!(unpack_i64(&[0x00; 7]).is_err());
    }

    // --- String tests ---

    #[test]
    fn pack_string_formats() {
        assert_eq!(pack_string("").unwrap(), vec![0x00]);
        assert_eq!(
            pack_string("Hi").unwrap(),
            vec![0x03, 0x48, 0x00, 0x69, 0x00, 0x00, 0x00] // len=3, 'H', 'i', null
        );
    }

    #[test]
    fn pack_string_accepts_the_longest_string_ptp_can_carry() {
        let s = "a".repeat(MAX_STRING_UNITS);
        let packed = pack_string(&s).unwrap();
        assert_eq!(packed[0], 255); // 254 units + the terminator fills the length byte
        assert_eq!(unpack_string(&packed).unwrap(), (s, packed.len()));
    }

    #[test]
    fn pack_string_refuses_a_string_one_unit_too_long() {
        // Regression anchor (mtp-rs#49): this used to wrap the length byte to 0, so the device
        // read an empty name and then misparsed the 255 code units that followed.
        let s = "a".repeat(MAX_STRING_UNITS + 1);
        assert!(matches!(
            pack_string(&s),
            Err(crate::PtpError::StringTooLong { units: 255 })
        ));
    }

    #[test]
    fn pack_string_counts_utf16_units_not_chars() {
        // 127 emoji are 254 units (fits); 128 are 256 units, though only 128 chars.
        let fits = "\u{1F600}".repeat(127);
        assert_eq!(pack_string(&fits).unwrap()[0], 255);
        let too_long = "\u{1F600}".repeat(128);
        assert!(matches!(
            pack_string(&too_long),
            Err(crate::PtpError::StringTooLong { units: 256 })
        ));
    }

    #[test]
    fn pack_string_emoji_surrogate_pair() {
        let packed = pack_string("\u{1F600}").unwrap();
        assert_eq!(packed[0], 3); // surrogate pair (2 units) + null
        assert_eq!(&packed[1..5], &[0x3D, 0xD8, 0x00, 0xDE]); // 0xD83D, 0xDE00
    }

    #[test]
    fn unpack_string_errors() {
        assert!(unpack_string(&[]).is_err());
        assert!(unpack_string(&[0x03, 0x41, 0x00]).is_err()); // truncated
    }

    #[test]
    fn unpack_string_replaces_a_name_cut_mid_surrogate_pair() {
        // A device that cuts an over-long name at the 254-unit cap can land between the two
        // halves of an emoji (#50). Buffer: len=3, 'A', lone high surrogate, NUL.
        let buf: &[u8] = &[0x03, b'A', 0x00, 0x3D, 0xD8, 0x00, 0x00];
        let (s, consumed) = unpack_string(buf).unwrap();
        assert_eq!(s, "A\u{FFFD}");
        assert_eq!(consumed, 7);
    }

    #[test]
    fn unpack_string_replaces_a_lone_low_surrogate_mid_string() {
        // Buffer: len=4, 'A', lone low surrogate, 'B', NUL.
        let buf: &[u8] = &[0x04, b'A', 0x00, 0x00, 0xDE, b'B', 0x00, 0x00, 0x00];
        let (s, _) = unpack_string(buf).unwrap();
        assert_eq!(s, "A\u{FFFD}B");
    }

    #[test]
    fn roundtrip_strings() {
        for s in ["", "Hello", "\u{3053}\u{3093}", "\u{1F600}"] {
            let (unpacked, _) = unpack_string(&pack_string(s).unwrap()).unwrap();
            assert_eq!(unpacked, s);
        }
    }

    #[test]
    fn unpack_string_truncates_at_first_null() {
        // Panasonic Lumix DMC-TZ61 (issue #12) pads its serial number to a
        // fixed width with multiple NULs. Anything from the first NUL on is
        // padding per the PTP spec (strings are null-terminated), so it must
        // not leak into the decoded String. Buffer: len=5, "AB\0C\0".
        let buf: &[u8] = &[
            0x05, b'A', 0x00, b'B', 0x00, 0x00, 0x00, b'C', 0x00, 0x00, 0x00,
        ];
        let (s, consumed) = unpack_string(buf).unwrap();
        assert_eq!(s, "AB");
        // The whole declared length is still consumed, so following fields
        // stay aligned.
        assert_eq!(consumed, 11);
    }

    // --- Array tests ---

    #[test]
    fn pack_arrays() {
        assert_eq!(pack_u16_array(&[]), vec![0x00, 0x00, 0x00, 0x00]);
        assert_eq!(
            pack_u16_array(&[1, 2]),
            vec![0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02, 0x00]
        );
        assert_eq!(
            pack_u32_array(&[1]),
            vec![0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn unpack_array_errors() {
        assert!(unpack_u16_array(&[]).is_err());
        assert!(unpack_u32_array(&[0x00, 0x00, 0x00]).is_err());
        // Count says 2, only 1 element
        assert!(unpack_u16_array(&[0x02, 0x00, 0x00, 0x00, 0x01, 0x00]).is_err());
        assert!(unpack_u32_array(&[0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]).is_err());
    }

    #[test]
    fn roundtrip_arrays() {
        for arr in [vec![], vec![1u16, 2, 3], vec![0xFFFF]] {
            let (unpacked, _) = unpack_u16_array(&pack_u16_array(&arr)).unwrap();
            assert_eq!(unpacked, arr);
        }
        for arr in [vec![], vec![1u32, 2, 3], vec![0xFFFFFFFF]] {
            let (unpacked, _) = unpack_u32_array(&pack_u32_array(&arr)).unwrap();
            assert_eq!(unpacked, arr);
        }
    }

    // --- Property-based tests ---

    fn valid_utf16_string() -> impl Strategy<Value = String> {
        // NUL is excluded: PTP strings are null-terminated, so a NUL can
        // never appear inside one (unpack_string truncates at the first NUL).
        prop::collection::vec(
            prop::char::range('\u{0001}', '\u{D7FF}')
                .prop_union(prop::char::range('\u{E000}', '\u{FFFF}')),
            0..100,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    proptest! {
        #[test]
        fn prop_roundtrip_primitives(
            u8_val: u8, u16_val: u16, u32_val: u32, u64_val: u64,
            i8_val: i8, i16_val: i16, i32_val: i32, i64_val: i64
        ) {
            prop_assert_eq!(unpack_u8(&pack_u8(u8_val)).unwrap(), u8_val);
            prop_assert_eq!(unpack_u16(&pack_u16(u16_val)).unwrap(), u16_val);
            prop_assert_eq!(unpack_u32(&pack_u32(u32_val)).unwrap(), u32_val);
            prop_assert_eq!(unpack_u64(&pack_u64(u64_val)).unwrap(), u64_val);
            prop_assert_eq!(unpack_i8(&pack_i8(i8_val)).unwrap(), i8_val);
            prop_assert_eq!(unpack_i16(&pack_i16(i16_val)).unwrap(), i16_val);
            prop_assert_eq!(unpack_i32(&pack_i32(i32_val)).unwrap(), i32_val);
            prop_assert_eq!(unpack_i64(&pack_i64(i64_val)).unwrap(), i64_val);
        }

        #[test]
        fn prop_roundtrip_string(s in valid_utf16_string()) {
            let s: String = s.chars().take(254).collect();
            let packed = pack_string(&s).unwrap();
            let (unpacked, consumed) = unpack_string(&packed).unwrap();
            prop_assert_eq!(&unpacked, &s);
            prop_assert_eq!(consumed, packed.len());
        }

        #[test]
        fn prop_roundtrip_arrays(
            u16_arr in prop::collection::vec(any::<u16>(), 0..50),
            u32_arr in prop::collection::vec(any::<u32>(), 0..50)
        ) {
            let packed16 = pack_u16_array(&u16_arr);
            let (unpacked16, consumed16) = unpack_u16_array(&packed16).unwrap();
            prop_assert_eq!(&unpacked16, &u16_arr);
            prop_assert_eq!(consumed16, packed16.len());

            let packed32 = pack_u32_array(&u32_arr);
            let (unpacked32, consumed32) = unpack_u32_array(&packed32).unwrap();
            prop_assert_eq!(&unpacked32, &u32_arr);
            prop_assert_eq!(consumed32, packed32.len());
        }

        #[test]
        fn prop_unpack_ignores_extra_bytes(val: u32, extra in prop::collection::vec(any::<u8>(), 0..10)) {
            let mut buf = pack_u32(val).to_vec();
            buf.extend_from_slice(&extra);
            prop_assert_eq!(unpack_u32(&buf).unwrap(), val);
        }

        #[test]
        fn fuzz_truncated_buffers(bytes in prop::collection::vec(any::<u8>(), 1..8)) {
            if bytes.len() < 2 { prop_assert!(unpack_u16(&bytes).is_err()); }
            if bytes.len() < 4 { prop_assert!(unpack_u32(&bytes).is_err()); }
            if bytes.len() < 8 { prop_assert!(unpack_u64(&bytes).is_err()); }
            if bytes.len() < 2 { prop_assert!(unpack_i16(&bytes).is_err()); }
            if bytes.len() < 4 { prop_assert!(unpack_i32(&bytes).is_err()); }
            if bytes.len() < 8 { prop_assert!(unpack_i64(&bytes).is_err()); }
        }

        #[test]
        fn fuzz_array_invalid_count(
            claimed_count in 2u32..100u32,
            actual_elements in prop::collection::vec(any::<u32>(), 0..5)
        ) {
            let mut buf = pack_u32(claimed_count).to_vec();
            for elem in &actual_elements {
                buf.extend_from_slice(&pack_u32(*elem));
            }
            if claimed_count as usize > actual_elements.len() {
                prop_assert!(unpack_u32_array(&buf).is_err());
            }
        }

        #[test]
        fn fuzz_large_array_count(count in (u32::MAX - 100)..=u32::MAX) {
            prop_assert!(unpack_u32_array(&pack_u32(count)).is_err());
            prop_assert!(unpack_u16_array(&pack_u32(count)).is_err());
        }
    }

    // Fuzz tests - verify parsers don't panic on arbitrary input
    crate::fuzz_bytes_fn!(fuzz_unpack_string, unpack_string, 100);
    crate::fuzz_bytes_fn!(fuzz_unpack_u16_array, unpack_u16_array, 50);
    crate::fuzz_bytes_fn!(fuzz_unpack_u32_array, unpack_u32_array, 50);
}
