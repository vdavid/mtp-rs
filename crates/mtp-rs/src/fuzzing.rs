//! Fuzzing entry points for the `fuzz/` targets. **Not a public API.**
//!
//! Gated behind the `fuzzing` feature, which only exists so `cargo-fuzz` targets can reach the
//! crate-private parsers. Applications must not enable it: it's unstable by design and exempt from
//! SemVer.
//!
//! Every function takes bytes a device chose and must return, never panic. Some also assert a
//! property, written as `assert!` so it survives release builds; a failed one panics, which the
//! fuzzer reports as a crash. Each one is single-threaded and reads nothing but its input (no
//! clock, no environment, no files), so OSS Verify can prove it: see `fuzz/CLAUDE.md`.
//!
//! Targets (one per function, same name, in `fuzz/fuzz_targets/`):
//!
//! - [`fuzz_container_parse`]: the USB container header, as the data, response, and event
//!   parsers read it. Data containers must round-trip.
//! - [`fuzz_device_info_parse`] / [`fuzz_storage_info_parse`]: the `GetDeviceInfo` and
//!   `GetStorageInfo` datasets, read when a device opens.
//! - [`fuzz_object_info_parse`]: the `GetObjectInfo` dataset, read for every file in a listing.
//!   A parsed one must re-encode to an equal one.
//! - [`fuzz_object_prop_list_parse`]: the `GetObjectPropList` dataset, the fast listing path.
//! - [`fuzz_device_prop_desc_parse`]: `GetDevicePropDesc`, with its enumeration and range forms.
//! - [`fuzz_property_value_parse`]: one typed value, which must round-trip.
//! - [`fuzz_string_parse`]: the length-prefixed UTF-16 string and the two array shapes.
//! - [`fuzz_datetime_parse`]: the `YYYYMMDDThhmmss[.s][Z|±hhmm]` date string, which must
//!   round-trip through [`DateTime::format`](crate::ptp::DateTime::format).
//! - [`fuzz_device_status_parse`]: the SIC `GET_DEVICE_STATUS` control response.

use crate::ptp::{
    container_type, pack_string, unpack_datetime, unpack_string, unpack_u16_array,
    unpack_u32_array, ContainerType, DataContainer, DateTime, DeviceInfo, DevicePropDesc,
    EventContainer, ObjectInfo, ObjectPropListElement, PropertyDataType, PropertyValue,
    ResponseContainer, StorageInfo, MAX_STRING_UNITS,
};

/// The USB container header and the three parsers that read a container from a device.
///
/// Property: a data container that parses re-encodes to bytes that parse to the same container.
/// The receive path hands `payload` to the dataset parsers, so a lossy header parse would feed
/// them the wrong bytes.
pub fn fuzz_container_parse(data: &[u8]) {
    let _ = container_type(data);

    if let Ok(container) = DataContainer::from_bytes(data) {
        let reparsed = DataContainer::from_bytes(&container.to_bytes());
        assert_eq!(
            reparsed.as_ref().ok(),
            Some(&container),
            "a data container did not survive to_bytes and from_bytes"
        );
    }

    if let Ok(response) = ResponseContainer::from_bytes(data) {
        // At most five parameters fit in the 32-byte cap a response has on the wire, but the
        // parser takes the header's word for it. What must hold is that it never claims more
        // parameters than the buffer had bytes for.
        assert!(
            12 + response.params.len() * 4 <= data.len(),
            "a response claimed {} parameters from {} bytes",
            response.params.len(),
            data.len()
        );
    }

    if let Ok(event) = EventContainer::from_bytes(data) {
        assert_eq!(
            container_type(data).ok(),
            Some(ContainerType::Event),
            "an event parsed from a container that isn't one: {event:?}"
        );
    }
}

/// The `GetDeviceInfo` dataset: strings and five `u16` arrays, each with a device-chosen count.
pub fn fuzz_device_info_parse(data: &[u8]) {
    let _ = DeviceInfo::from_bytes(data);
}

/// The `GetStorageInfo` dataset.
pub fn fuzz_storage_info_parse(data: &[u8]) {
    let _ = StorageInfo::from_bytes(data);
}

/// The `GetObjectInfo` dataset.
///
/// Property: a parsed `ObjectInfo` that encodes (its strings fit PTP's length byte) parses back to
/// an equal one. Uploads build their `SendObjectInfo` from these same fields, so a field that
/// doesn't survive the trip is a file uploaded with different metadata than the one listed.
pub fn fuzz_object_info_parse(data: &[u8]) {
    let Ok(info) = ObjectInfo::from_bytes(data) else {
        return;
    };
    let Ok(bytes) = info.to_bytes() else {
        return;
    };
    let reparsed = ObjectInfo::from_bytes(&bytes).expect("a re-encoded ObjectInfo did not parse");
    // `ObjectInfo` has no `PartialEq` (it's public and `non_exhaustive`, and equality on it would
    // be a semver promise), so compare the debug forms, which name every field.
    assert_eq!(
        format!("{reparsed:?}"),
        format!("{info:?}"),
        "an ObjectInfo did not survive to_bytes and from_bytes"
    );
}

/// The `GetObjectPropList` dataset: a device-chosen element count, then typed values, arrays,
/// and 128-bit integers that the parser steps over by size.
///
/// Property: every decoded value has the data type its element declared, since
/// `ObjectInfo::set_property` picks the field by property code and trusts the type.
pub fn fuzz_object_prop_list_parse(data: &[u8]) {
    let Ok(elements) = ObjectPropListElement::list_from_bytes(data) else {
        return;
    };
    for element in &elements {
        if let Some(value) = &element.value {
            assert_eq!(
                value.data_type(),
                element.data_type,
                "a prop-list value decoded as a different type than declared"
            );
        }
    }
}

/// The `GetDevicePropDesc` dataset, with its range and enumeration forms.
pub fn fuzz_device_prop_desc_parse(data: &[u8]) {
    let _ = DevicePropDesc::from_bytes(data);
}

/// One typed property value. The first two bytes pick the data type, the rest is the value.
///
/// Property: a value that parses re-encodes to bytes that parse to the same value, consuming
/// exactly those bytes.
pub fn fuzz_property_value_parse(data: &[u8]) {
    let Some((type_bytes, rest)) = data.split_first_chunk::<2>() else {
        return;
    };
    let data_type = PropertyDataType::from(u16::from_le_bytes(*type_bytes));
    let Ok((value, consumed)) = PropertyValue::from_bytes(rest, data_type) else {
        return;
    };
    assert!(consumed <= rest.len(), "consumed more bytes than it had");
    assert_eq!(value.data_type(), data_type, "decoded as a different type");
    let Ok(bytes) = value.to_bytes() else {
        return;
    };
    let reparsed = PropertyValue::from_bytes(&bytes, data_type);
    assert_eq!(
        reparsed.as_ref().ok(),
        Some(&(value, bytes.len())),
        "a property value did not survive to_bytes and from_bytes"
    );
}

/// The PTP string and the `u16` and `u32` arrays, read from the same bytes.
///
/// Property: a decoded string that fits PTP's length byte encodes and decodes back to itself. A
/// listed file name is what a rename or a copy back sends, so it must be sendable as it was read.
pub fn fuzz_string_parse(data: &[u8]) {
    if let Ok((s, consumed)) = unpack_string(data) {
        assert!(
            consumed <= data.len(),
            "a string consumed more bytes than it had"
        );
        if s.encode_utf16().count() <= MAX_STRING_UNITS {
            let packed = pack_string(&s).expect("a string within MAX_STRING_UNITS did not pack");
            let (again, again_consumed) =
                unpack_string(&packed).expect("a packed string did not unpack");
            assert_eq!(again, s, "a string did not survive pack and unpack");
            assert_eq!(
                again_consumed,
                packed.len(),
                "unpack did not consume what pack wrote"
            );
        }
    }
    if let Ok((values, consumed)) = unpack_u16_array(data) {
        assert_eq!(consumed, 4 + values.len() * 2, "u16 array size mismatch");
        assert!(
            consumed <= data.len(),
            "a u16 array consumed more bytes than it had"
        );
    }
    if let Ok((values, consumed)) = unpack_u32_array(data) {
        assert_eq!(consumed, 4 + values.len() * 4, "u32 array size mismatch");
        assert!(
            consumed <= data.len(),
            "a u32 array consumed more bytes than it had"
        );
    }
}

/// The MTP date string, both as text and as the length-prefixed string a dataset carries.
///
/// Property: a date that parses formats to a string that parses to the same date. Uploads write
/// dates with `format`, so a date that can be read but not written back changes on a copy.
pub fn fuzz_datetime_parse(data: &[u8]) {
    if let Ok(s) = std::str::from_utf8(data) {
        if let Some(dt) = DateTime::parse(s) {
            assert!(dt.is_valid(), "parse returned an invalid date from {s:?}");
            let formatted = dt.format().expect("a parsed date did not format");
            assert_eq!(
                DateTime::parse(&formatted),
                Some(dt),
                "{s:?} parsed, formatted as {formatted:?}, and parsed differently"
            );
        }
    }
    if let Ok((Some(dt), _)) = unpack_datetime(data) {
        assert!(dt.is_valid(), "unpack_datetime returned an invalid date");
    }
}

/// The SIC `GET_DEVICE_STATUS` response a device sends back during a cancel.
///
/// Property: it reports at most one stalled endpoint per four bytes after the status code.
pub fn fuzz_device_status_parse(data: &[u8]) {
    if let Some((_code, endpoints)) = crate::transport::nusb::parse_device_status(data) {
        assert!(
            endpoints.len() <= data.len().saturating_sub(4) / 4,
            "{} endpoints from {} bytes",
            endpoints.len(),
            data.len()
        );
    }
}

/// The committed seed corpus: a few well-formed inputs per target, so each run starts past the
/// header checks and every property above runs at least once (`every_seed_reaches_its_target`
/// checks that). `write_seed_corpus` writes them to `fuzz/corpus/<target>/seed_<name>.bin`.
#[cfg(test)]
mod seeds {
    use super::*;
    use crate::ptp::{
        pack_datetime, pack_string, pack_u16, pack_u16_array, pack_u32, pack_u32_array, pack_u64,
        ObjectFormatCode, ObjectHandle, OperationCode, StorageId, UtcOffset,
    };

    type Seed = (&'static str, &'static str, Vec<u8>);

    fn container(length: u32, kind: u16, code: u16, tail: &[u8]) -> Vec<u8> {
        let mut buf = pack_u32(length).to_vec();
        buf.extend_from_slice(&pack_u16(kind));
        buf.extend_from_slice(&pack_u16(code));
        buf.extend_from_slice(&pack_u32(7)); // transaction ID
        buf.extend_from_slice(tail);
        buf
    }

    fn device_info() -> Vec<u8> {
        let mut buf = pack_u16(100).to_vec(); // StandardVersion 1.00
        buf.extend_from_slice(&pack_u32(6)); // VendorExtensionID: Microsoft
        buf.extend_from_slice(&pack_u16(100));
        buf.extend_from_slice(&pack_string("microsoft.com: 1.0").unwrap());
        buf.extend_from_slice(&pack_u16(0)); // FunctionalMode
        buf.extend_from_slice(&pack_u16_array(&[0x1001, 0x1002, 0x1008, 0x9805]));
        buf.extend_from_slice(&pack_u16_array(&[0x4002, 0x4003]));
        buf.extend_from_slice(&pack_u16_array(&[0x5001, 0xD402]));
        buf.extend_from_slice(&pack_u16_array(&[0x3801]));
        buf.extend_from_slice(&pack_u16_array(&[0x3801, 0x3009]));
        for s in ["Google", "Pixel 9", "1.0", "ABC123"] {
            buf.extend_from_slice(&pack_string(s).unwrap());
        }
        buf
    }

    fn storage_info() -> Vec<u8> {
        let mut buf = pack_u16(3).to_vec(); // FixedRam
        buf.extend_from_slice(&pack_u16(2)); // GenericHierarchical
        buf.extend_from_slice(&pack_u16(0)); // ReadWrite
        buf.extend_from_slice(&pack_u64(128_000_000_000));
        buf.extend_from_slice(&pack_u64(64_000_000_000));
        buf.extend_from_slice(&pack_u32(0xFFFF_FFFF));
        buf.extend_from_slice(&pack_string("Internal shared storage").unwrap());
        buf.extend_from_slice(&pack_string("").unwrap());
        buf
    }

    fn object_info(name: &str, folder: bool) -> Vec<u8> {
        let date = DateTime::parse("20240315T143022").unwrap();
        ObjectInfo {
            storage_id: StorageId(0x0001_0001),
            format: if folder {
                ObjectFormatCode::Association
            } else {
                ObjectFormatCode::Jpeg
            },
            size: if folder { 0 } else { 2_345_678 },
            parent: ObjectHandle(5),
            filename: name.to_string(),
            created: Some(date),
            modified: Some(date.with_offset(UtcOffset::from_minutes(120).unwrap())),
            ..Default::default()
        }
        .to_bytes()
        .unwrap()
    }

    fn prop_list() -> Vec<u8> {
        let element = |handle: u32, property: u16, data_type: u16, value: &[u8]| {
            let mut buf = pack_u32(handle).to_vec();
            buf.extend_from_slice(&pack_u16(property));
            buf.extend_from_slice(&pack_u16(data_type));
            buf.extend_from_slice(value);
            buf
        };
        let modified = pack_datetime(&DateTime::parse("20240315T143022Z").unwrap()).unwrap();
        let elements = [
            element(1, 0xDC07, 0xFFFF, &pack_string("IMG_0001.JPG").unwrap()), // ObjectFileName
            element(1, 0xDC04, 0x0008, &pack_u64(2_345_678)),                  // ObjectSize
            element(1, 0xDC01, 0x0006, &pack_u32(0x0001_0001)),                // StorageID
            element(1, 0xDC09, 0xFFFF, &modified),                             // DateModified
            element(2, 0xDC02, 0x0004, &pack_u16(0x3001)),                     // ObjectFormat
            element(2, 0xDC41, 0x000A, &[0x11; 16]), // a UINT128, stepped over
            element(2, 0xDC8B, 0x4004, &pack_u16_array(&[1, 2, 3])), // an AUINT16, stepped over
            element(2, 0xDC0B, 0x0000, &[]),         // Undefined: no value bytes
        ];
        let mut buf = pack_u32(elements.len() as u32).to_vec();
        for e in elements {
            buf.extend_from_slice(&e);
        }
        buf
    }

    fn battery_prop_desc() -> Vec<u8> {
        let mut buf = pack_u16(0x5001).to_vec(); // BatteryLevel
        buf.extend_from_slice(&pack_u16(0x0002)); // UINT8
        buf.extend([0x00, 100, 75, 0x01, 0, 100, 1]); // read-only, default, current, Range
        buf
    }

    fn iso_prop_desc() -> Vec<u8> {
        let mut buf = pack_u16(0x500F).to_vec(); // ExposureIndex
        buf.extend_from_slice(&pack_u16(0x0004)); // UINT16
        buf.push(0x01); // read-write
        buf.extend_from_slice(&pack_u16(400));
        buf.extend_from_slice(&pack_u16(800));
        buf.push(0x02); // Enumeration
        buf.extend_from_slice(&pack_u16(4));
        for iso in [100u16, 200, 400, 800] {
            buf.extend_from_slice(&pack_u16(iso));
        }
        buf
    }

    fn name_prop_desc() -> Vec<u8> {
        let mut buf = pack_u16(0xD402).to_vec(); // DeviceFriendlyName
        buf.extend_from_slice(&pack_u16(0xFFFF)); // String
        buf.push(0x01);
        buf.extend_from_slice(&pack_string("Pixel").unwrap());
        buf.extend_from_slice(&pack_string("Sam's phone").unwrap());
        buf.push(0x00); // no form
        buf
    }

    fn typed(data_type: u16, value: &[u8]) -> Vec<u8> {
        let mut buf = pack_u16(data_type).to_vec();
        buf.extend_from_slice(value);
        buf
    }

    pub(super) fn all() -> Vec<Seed> {
        let data = DataContainer {
            code: OperationCode::GetObjectInfo,
            transaction_id: 7,
            payload: object_info("a.jpg", false),
        };
        let event_params = [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        // "A", then a lone high surrogate, which decodes to U+FFFD.
        let lone_surrogate = vec![3u8, 0x41, 0x00, 0x00, 0xD8, 0x00, 0x00];
        let stalled = vec![0x0C, 0x00, 0x19, 0x20, 0x81, 0, 0, 0, 0x02, 0, 0, 0];
        vec![
            ("fuzz_container_parse", "data", data.to_bytes()),
            (
                "fuzz_container_parse",
                "response",
                container(16, 3, 0x2001, &pack_u32(42)),
            ),
            (
                "fuzz_container_parse",
                "event",
                container(24, 4, 0x4002, &event_params),
            ),
            // A response whose length is below the header, which used to underflow.
            (
                "fuzz_container_parse",
                "response_short_length",
                container(5, 3, 0x2001, &[0; 8]),
            ),
            ("fuzz_device_info_parse", "pixel", device_info()),
            ("fuzz_storage_info_parse", "internal", storage_info()),
            (
                "fuzz_object_info_parse",
                "photo",
                object_info("IMG_0001.JPG", false),
            ),
            (
                "fuzz_object_info_parse",
                "folder",
                object_info("DCIM", true),
            ),
            (
                "fuzz_object_info_parse",
                "emoji",
                object_info("trip 🏔️.jpg", false),
            ),
            ("fuzz_object_prop_list_parse", "listing", prop_list()),
            (
                "fuzz_device_prop_desc_parse",
                "battery_range",
                battery_prop_desc(),
            ),
            (
                "fuzz_device_prop_desc_parse",
                "iso_enumeration",
                iso_prop_desc(),
            ),
            (
                "fuzz_device_prop_desc_parse",
                "friendly_name",
                name_prop_desc(),
            ),
            (
                "fuzz_property_value_parse",
                "uint16",
                typed(0x0004, &pack_u16(800)),
            ),
            (
                "fuzz_property_value_parse",
                "int64",
                typed(0x0007, &(-5i64).to_le_bytes()),
            ),
            (
                "fuzz_property_value_parse",
                "string",
                typed(0xFFFF, &pack_string("Pixel").unwrap()),
            ),
            (
                "fuzz_string_parse",
                "name",
                pack_string("IMG_0001.JPG").unwrap(),
            ),
            ("fuzz_string_parse", "empty", vec![0]),
            ("fuzz_string_parse", "lone_surrogate", lone_surrogate),
            ("fuzz_string_parse", "u32_array", pack_u32_array(&[1, 2, 3])),
            ("fuzz_datetime_parse", "plain", b"20240315T143022".to_vec()),
            (
                "fuzz_datetime_parse",
                "tenths_offset",
                b"20240229T235959.5-0800".to_vec(),
            ),
            ("fuzz_datetime_parse", "utc", b"20240315T143022Z".to_vec()),
            (
                "fuzz_datetime_parse",
                "packed",
                pack_string("20240315T143022+0530").unwrap(),
            ),
            ("fuzz_device_status_parse", "stalled", stalled),
            (
                "fuzz_device_status_parse",
                "ok",
                vec![0x04, 0x00, 0x01, 0x20],
            ),
        ]
    }

    /// Every seed gets past the parser it targets, so the property after it runs, and no seed
    /// fails its target.
    #[test]
    fn every_seed_reaches_its_target() {
        for (target, name, bytes) in all() {
            let parsed = match target {
                "fuzz_container_parse" => {
                    fuzz_container_parse(&bytes);
                    let short = name == "response_short_length";
                    short != ResponseContainer::from_bytes(&bytes).is_ok()
                        || DataContainer::from_bytes(&bytes).is_ok()
                        || EventContainer::from_bytes(&bytes).is_ok()
                }
                "fuzz_device_info_parse" => DeviceInfo::from_bytes(&bytes).is_ok(),
                "fuzz_storage_info_parse" => StorageInfo::from_bytes(&bytes).is_ok(),
                "fuzz_object_info_parse" => {
                    fuzz_object_info_parse(&bytes);
                    ObjectInfo::from_bytes(&bytes).is_ok()
                }
                "fuzz_object_prop_list_parse" => {
                    fuzz_object_prop_list_parse(&bytes);
                    ObjectPropListElement::list_from_bytes(&bytes).is_ok_and(|e| e.len() == 8)
                }
                "fuzz_device_prop_desc_parse" => DevicePropDesc::from_bytes(&bytes).is_ok(),
                "fuzz_property_value_parse" => {
                    fuzz_property_value_parse(&bytes);
                    let code = u16::from_le_bytes([bytes[0], bytes[1]]);
                    PropertyValue::from_bytes(&bytes[2..], PropertyDataType::from(code)).is_ok()
                }
                "fuzz_string_parse" => {
                    fuzz_string_parse(&bytes);
                    unpack_string(&bytes).is_ok() || unpack_u32_array(&bytes).is_ok()
                }
                "fuzz_datetime_parse" => {
                    fuzz_datetime_parse(&bytes);
                    let as_text = std::str::from_utf8(&bytes).ok().and_then(DateTime::parse);
                    as_text.is_some() || unpack_datetime(&bytes).is_ok_and(|(dt, _)| dt.is_some())
                }
                "fuzz_device_status_parse" => {
                    fuzz_device_status_parse(&bytes);
                    crate::transport::nusb::parse_device_status(&bytes).is_some()
                }
                other => panic!("seed for unknown target {other}"),
            };
            assert!(parsed, "seed {target}/{name} doesn't parse");
        }
    }

    /// Regenerates `fuzz/corpus/` (`just fuzz-seeds`).
    #[test]
    #[ignore = "writes files; run it with `just fuzz-seeds`"]
    fn write_seed_corpus() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus");
        for (target, name, bytes) in all() {
            let dir = root.join(target);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("seed_{name}.bin")), bytes).unwrap();
        }
    }
}
