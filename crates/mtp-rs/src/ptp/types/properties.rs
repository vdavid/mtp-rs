//! Property-related types for MTP/PTP device properties.
//!
//! This module contains types for working with device properties:
//! - [`PropertyValue`]: A property value with its associated type
//! - [`PropertyFormType`]: Form type for property value constraints
//! - [`PropertyRange`]: Range constraint for a property value
//! - [`DevicePropDesc`]: Device property descriptor

use crate::ptp::codes::{DevicePropertyCode, PropertyDataType};
use crate::ptp::pack::{
    pack_i16, pack_i32, pack_i64, pack_i8, pack_string, pack_u16, pack_u32, pack_u64, pack_u8,
    unpack_i16, unpack_i32, unpack_i64, unpack_i8, unpack_string, unpack_u16, unpack_u64,
    unpack_u8,
};

// --- PropertyValue Enum ---

/// A property value with its associated type.
///
/// Used for device property values in `DevicePropDesc`, as well as
/// for get/set device property operations.
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyValue {
    /// Signed 8-bit integer.
    Int8(i8),
    /// Unsigned 8-bit integer.
    Uint8(u8),
    /// Signed 16-bit integer.
    Int16(i16),
    /// Unsigned 16-bit integer.
    Uint16(u16),
    /// Signed 32-bit integer.
    Int32(i32),
    /// Unsigned 32-bit integer.
    Uint32(u32),
    /// Signed 64-bit integer.
    Int64(i64),
    /// Unsigned 64-bit integer.
    Uint64(u64),
    /// UTF-16LE encoded string.
    String(String),
}

impl PropertyValue {
    /// Serialize this property value to bytes.
    ///
    /// # Errors
    ///
    /// [`PtpError::StringTooLong`](crate::PtpError::StringTooLong) for a string longer than PTP
    /// can carry (see [`pack_string`]). Every other variant always serializes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, crate::PtpError> {
        Ok(match self {
            PropertyValue::Int8(v) => pack_i8(*v).to_vec(),
            PropertyValue::Uint8(v) => pack_u8(*v).to_vec(),
            PropertyValue::Int16(v) => pack_i16(*v).to_vec(),
            PropertyValue::Uint16(v) => pack_u16(*v).to_vec(),
            PropertyValue::Int32(v) => pack_i32(*v).to_vec(),
            PropertyValue::Uint32(v) => pack_u32(*v).to_vec(),
            PropertyValue::Int64(v) => pack_i64(*v).to_vec(),
            PropertyValue::Uint64(v) => pack_u64(*v).to_vec(),
            PropertyValue::String(v) => pack_string(v)?,
        })
    }

    /// Parse a property value from bytes given the expected data type.
    ///
    /// Returns the parsed value and the number of bytes consumed.
    pub fn from_bytes(
        buf: &[u8],
        data_type: PropertyDataType,
    ) -> Result<(Self, usize), crate::PtpError> {
        match data_type {
            PropertyDataType::Int8 => {
                let val = unpack_i8(buf)?;
                Ok((PropertyValue::Int8(val), 1))
            }
            PropertyDataType::Uint8 => {
                let val = unpack_u8(buf)?;
                Ok((PropertyValue::Uint8(val), 1))
            }
            PropertyDataType::Int16 => {
                let val = unpack_i16(buf)?;
                Ok((PropertyValue::Int16(val), 2))
            }
            PropertyDataType::Uint16 => {
                let val = unpack_u16(buf)?;
                Ok((PropertyValue::Uint16(val), 2))
            }
            PropertyDataType::Int32 => {
                let val = unpack_i32(buf)?;
                Ok((PropertyValue::Int32(val), 4))
            }
            PropertyDataType::Uint32 => {
                let val = crate::ptp::pack::unpack_u32(buf)?;
                Ok((PropertyValue::Uint32(val), 4))
            }
            PropertyDataType::Int64 => {
                let val = unpack_i64(buf)?;
                Ok((PropertyValue::Int64(val), 8))
            }
            PropertyDataType::Uint64 => {
                let val = unpack_u64(buf)?;
                Ok((PropertyValue::Uint64(val), 8))
            }
            PropertyDataType::String => {
                let (val, consumed) = unpack_string(buf)?;
                Ok((PropertyValue::String(val), consumed))
            }
            PropertyDataType::Undefined
            | PropertyDataType::Int128
            | PropertyDataType::Uint128
            | PropertyDataType::Unknown(_) => Err(crate::PtpError::invalid_data(format!(
                "unsupported property data type: {:?}",
                data_type
            ))),
        }
    }

    /// Get the data type of this property value.
    #[must_use]
    pub fn data_type(&self) -> PropertyDataType {
        match self {
            PropertyValue::Int8(_) => PropertyDataType::Int8,
            PropertyValue::Uint8(_) => PropertyDataType::Uint8,
            PropertyValue::Int16(_) => PropertyDataType::Int16,
            PropertyValue::Uint16(_) => PropertyDataType::Uint16,
            PropertyValue::Int32(_) => PropertyDataType::Int32,
            PropertyValue::Uint32(_) => PropertyDataType::Uint32,
            PropertyValue::Int64(_) => PropertyDataType::Int64,
            PropertyValue::Uint64(_) => PropertyDataType::Uint64,
            PropertyValue::String(_) => PropertyDataType::String,
        }
    }
}

// --- PropertyFormType Enum ---

/// Form type for property value constraints.
///
/// Describes how allowed values are specified for a property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PropertyFormType {
    /// No constraints (any value valid).
    #[default]
    None,
    /// Value must be within a range (min, max, step).
    Range,
    /// Value must be one of an enumerated set.
    Enumeration,
    /// Unknown form type.
    Unknown(u8),
}

impl PropertyFormType {
    /// Convert a raw u8 code to a PropertyFormType.
    #[must_use]
    pub fn from_code(code: u8) -> Self {
        match code {
            0x00 => PropertyFormType::None,
            0x01 => PropertyFormType::Range,
            0x02 => PropertyFormType::Enumeration,
            _ => PropertyFormType::Unknown(code),
        }
    }

    /// Convert a PropertyFormType to its raw u8 value.
    #[must_use]
    pub fn to_code(self) -> u8 {
        match self {
            PropertyFormType::None => 0x00,
            PropertyFormType::Range => 0x01,
            PropertyFormType::Enumeration => 0x02,
            PropertyFormType::Unknown(code) => code,
        }
    }
}

// --- PropertyRange Struct ---

/// Range constraint for a property value.
///
/// Used when `PropertyFormType::Range` is specified.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyRange {
    /// Minimum allowed value.
    pub min: PropertyValue,
    /// Maximum allowed value.
    pub max: PropertyValue,
    /// Step size between allowed values.
    pub step: PropertyValue,
}

impl PropertyRange {
    /// Parse a PropertyRange from bytes given the data type.
    ///
    /// Returns the parsed range and the number of bytes consumed.
    pub fn from_bytes(
        buf: &[u8],
        data_type: PropertyDataType,
    ) -> Result<(Self, usize), crate::PtpError> {
        let mut offset = 0;

        let (min, consumed) = PropertyValue::from_bytes(&buf[offset..], data_type)?;
        offset += consumed;

        let (max, consumed) = PropertyValue::from_bytes(&buf[offset..], data_type)?;
        offset += consumed;

        let (step, consumed) = PropertyValue::from_bytes(&buf[offset..], data_type)?;
        offset += consumed;

        Ok((PropertyRange { min, max, step }, offset))
    }

    /// Serialize this property range to bytes.
    ///
    /// # Errors
    ///
    /// As [`PropertyValue::to_bytes`]: only a too-long string bound fails.
    pub fn to_bytes(&self) -> Result<Vec<u8>, crate::PtpError> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&self.min.to_bytes()?);
        buf.extend_from_slice(&self.max.to_bytes()?);
        buf.extend_from_slice(&self.step.to_bytes()?);
        Ok(buf)
    }
}

// --- DevicePropDesc Structure ---

/// Device property descriptor.
///
/// Describes a device property including its type, current value,
/// default value, and allowed values/ranges.
///
/// Returned by the GetDevicePropDesc operation.
#[derive(Debug, Clone)]
pub struct DevicePropDesc {
    /// Property code identifying this property.
    pub property_code: DevicePropertyCode,
    /// Data type of the property value.
    pub data_type: PropertyDataType,
    /// Whether the property is writable (true) or read-only (false).
    pub writable: bool,
    /// Default/factory value.
    pub default_value: PropertyValue,
    /// Current value.
    pub current_value: PropertyValue,
    /// Form type (None, Range, or Enumeration).
    pub form_type: PropertyFormType,
    /// Allowed values (if form_type is Enumeration).
    pub enum_values: Option<Vec<PropertyValue>>,
    /// Value range (if form_type is Range).
    pub range: Option<PropertyRange>,
}

impl DevicePropDesc {
    /// Parse a DevicePropDesc from bytes.
    ///
    /// The buffer should contain the DevicePropDesc dataset as returned
    /// by GetDevicePropDesc.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, crate::PtpError> {
        let mut offset = 0;

        // 1. PropertyCode (u16)
        let property_code = DevicePropertyCode::from(unpack_u16(&buf[offset..])?);
        offset += 2;

        // 2. DataType (u16)
        let data_type = PropertyDataType::from(unpack_u16(&buf[offset..])?);
        offset += 2;

        // 3. GetSet (u8): 0x00 = read-only, 0x01 = read-write
        if buf.len() <= offset {
            return Err(crate::PtpError::invalid_data(
                "DevicePropDesc: insufficient bytes for GetSet",
            ));
        }
        let writable = buf[offset] != 0x00;
        offset += 1;

        // 4. DefaultValue (variable size based on data type)
        let (default_value, consumed) = PropertyValue::from_bytes(&buf[offset..], data_type)?;
        offset += consumed;

        // 5. CurrentValue (variable size based on data type)
        let (current_value, consumed) = PropertyValue::from_bytes(&buf[offset..], data_type)?;
        offset += consumed;

        // 6. FormFlag (u8)
        if buf.len() <= offset {
            return Err(crate::PtpError::invalid_data(
                "DevicePropDesc: insufficient bytes for FormFlag",
            ));
        }
        let form_type = PropertyFormType::from_code(buf[offset]);
        offset += 1;

        // 7. Form data (depends on form_type)
        let (enum_values, range) = match form_type {
            PropertyFormType::None | PropertyFormType::Unknown(_) => (None, None),
            PropertyFormType::Range => {
                let (range, _consumed) = PropertyRange::from_bytes(&buf[offset..], data_type)?;
                (None, Some(range))
            }
            PropertyFormType::Enumeration => {
                // Number of values (u16)
                let count = unpack_u16(&buf[offset..])? as usize;
                offset += 2;

                let mut values = Vec::with_capacity(count);
                for _ in 0..count {
                    let (val, consumed) = PropertyValue::from_bytes(&buf[offset..], data_type)?;
                    values.push(val);
                    offset += consumed;
                }
                (Some(values), None)
            }
        };

        Ok(DevicePropDesc {
            property_code,
            data_type,
            writable,
            default_value,
            current_value,
            form_type,
            enum_values,
            range,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::pack::{pack_i16, pack_u16};
    use proptest::prelude::*;

    // --- PropertyValue tests ---

    #[test]
    fn property_value_roundtrip() {
        let values = [
            PropertyValue::Int8(-42),
            PropertyValue::Uint8(100),
            PropertyValue::Int16(-1000),
            PropertyValue::Uint16(5000),
            PropertyValue::Int32(-100000),
            PropertyValue::Uint32(100000),
            PropertyValue::Int64(-1_000_000_000),
            PropertyValue::Uint64(1_000_000_000),
            PropertyValue::String("Test".to_string()),
        ];
        for val in &values {
            let bytes = val.to_bytes().unwrap();
            let (parsed, _) = PropertyValue::from_bytes(&bytes, val.data_type()).unwrap();
            assert_eq!(&parsed, val);
        }
    }

    #[test]
    fn property_value_data_type() {
        let cases: &[(PropertyValue, PropertyDataType)] = &[
            (PropertyValue::Int8(0), PropertyDataType::Int8),
            (PropertyValue::Uint8(0), PropertyDataType::Uint8),
            (PropertyValue::Int16(0), PropertyDataType::Int16),
            (PropertyValue::Uint16(0), PropertyDataType::Uint16),
            (PropertyValue::Int32(0), PropertyDataType::Int32),
            (PropertyValue::Uint32(0), PropertyDataType::Uint32),
            (PropertyValue::Int64(0), PropertyDataType::Int64),
            (PropertyValue::Uint64(0), PropertyDataType::Uint64),
            (PropertyValue::String("".into()), PropertyDataType::String),
        ];
        for (val, expected) in cases {
            assert_eq!(val.data_type(), *expected);
        }
    }

    #[test]
    fn property_value_from_bytes_unsupported_type() {
        for dt in [
            PropertyDataType::Undefined,
            PropertyDataType::Int128,
            PropertyDataType::Uint128,
            PropertyDataType::Unknown(0x99),
        ] {
            assert!(PropertyValue::from_bytes(&[0x00], dt).is_err());
        }
    }

    #[test]
    fn property_value_from_bytes_insufficient_bytes() {
        assert!(PropertyValue::from_bytes(&[], PropertyDataType::Int8).is_err());
        assert!(PropertyValue::from_bytes(&[0x00], PropertyDataType::Int16).is_err());
        assert!(PropertyValue::from_bytes(&[0x00, 0x00], PropertyDataType::Int32).is_err());
        assert!(PropertyValue::from_bytes(&[0x00; 7], PropertyDataType::Int64).is_err());
    }

    // --- PropertyFormType tests ---

    #[test]
    fn property_form_type_from_code() {
        for (code, expected) in [
            (0x00, PropertyFormType::None),
            (0x01, PropertyFormType::Range),
            (0x02, PropertyFormType::Enumeration),
        ] {
            assert_eq!(PropertyFormType::from_code(code), expected);
            assert_eq!(expected.to_code(), code);
        }
        assert_eq!(
            PropertyFormType::from_code(0x99),
            PropertyFormType::Unknown(0x99)
        );
    }

    #[test]
    fn property_form_type_to_code() {
        assert_eq!(PropertyFormType::None.to_code(), 0x00);
        assert_eq!(PropertyFormType::Range.to_code(), 0x01);
        assert_eq!(PropertyFormType::Enumeration.to_code(), 0x02);
        assert_eq!(PropertyFormType::Unknown(0x99).to_code(), 0x99);
    }

    #[test]
    fn property_form_type_roundtrip() {
        for f in [
            PropertyFormType::None,
            PropertyFormType::Range,
            PropertyFormType::Enumeration,
        ] {
            assert_eq!(PropertyFormType::from_code(f.to_code()), f);
        }
    }

    // --- PropertyRange tests ---

    #[test]
    fn property_range_from_bytes_uint8() {
        let buf = vec![0x00, 0x64, 0x01]; // min=0, max=100, step=1
        let (range, consumed) = PropertyRange::from_bytes(&buf, PropertyDataType::Uint8).unwrap();
        assert_eq!(range.min, PropertyValue::Uint8(0));
        assert_eq!(range.max, PropertyValue::Uint8(100));
        assert_eq!(range.step, PropertyValue::Uint8(1));
        assert_eq!(consumed, 3);
    }

    #[test]
    fn property_range_from_bytes_uint16() {
        let buf = vec![0x64, 0x00, 0x00, 0x19, 0x64, 0x00]; // min=100, max=6400, step=100
        let (range, consumed) = PropertyRange::from_bytes(&buf, PropertyDataType::Uint16).unwrap();
        assert_eq!(range.min, PropertyValue::Uint16(100));
        assert_eq!(range.max, PropertyValue::Uint16(6400));
        assert_eq!(range.step, PropertyValue::Uint16(100));
        assert_eq!(consumed, 6);
    }

    #[test]
    fn property_range_to_bytes() {
        let range = PropertyRange {
            min: PropertyValue::Uint8(0),
            max: PropertyValue::Uint8(100),
            step: PropertyValue::Uint8(1),
        };
        assert_eq!(range.to_bytes().unwrap(), vec![0x00, 0x64, 0x01]);
    }

    #[test]
    fn property_range_roundtrip() {
        let range = PropertyRange {
            min: PropertyValue::Uint16(100),
            max: PropertyValue::Uint16(6400),
            step: PropertyValue::Uint16(100),
        };
        let bytes = range.to_bytes().unwrap();
        let (parsed, _) = PropertyRange::from_bytes(&bytes, PropertyDataType::Uint16).unwrap();
        assert_eq!(parsed.min, range.min);
        assert_eq!(parsed.max, range.max);
        assert_eq!(parsed.step, range.step);
    }

    // --- DevicePropDesc tests ---

    fn build_battery_level_prop_desc(current: u8) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&pack_u16(0x5001)); // BatteryLevel
        buf.extend_from_slice(&pack_u16(0x0002)); // UINT8
        buf.push(0x00); // read-only
        buf.push(100); // default
        buf.push(current); // current
        buf.push(0x01); // Range
        buf.extend([0, 100, 1]); // min, max, step
        buf
    }

    #[test]
    fn device_prop_desc_parse_battery_level() {
        let desc = DevicePropDesc::from_bytes(&build_battery_level_prop_desc(75)).unwrap();
        assert_eq!(desc.property_code, DevicePropertyCode::BatteryLevel);
        assert_eq!(desc.data_type, PropertyDataType::Uint8);
        assert!(!desc.writable);
        assert_eq!(desc.current_value, PropertyValue::Uint8(75));
        assert_eq!(desc.form_type, PropertyFormType::Range);
        let range = desc.range.unwrap();
        assert_eq!(range.min, PropertyValue::Uint8(0));
        assert_eq!(range.max, PropertyValue::Uint8(100));
    }

    fn build_iso_prop_desc() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&pack_u16(0x500F)); // ExposureIndex
        buf.extend_from_slice(&pack_u16(0x0004)); // UINT16
        buf.push(0x01); // read-write
        buf.extend_from_slice(&pack_u16(400)); // default
        buf.extend_from_slice(&pack_u16(800)); // current
        buf.push(0x02); // Enumeration
        buf.extend_from_slice(&pack_u16(4)); // count
        for iso in [100u16, 200, 400, 800] {
            buf.extend_from_slice(&pack_u16(iso));
        }
        buf
    }

    #[test]
    fn device_prop_desc_parse_iso_enumeration() {
        let desc = DevicePropDesc::from_bytes(&build_iso_prop_desc()).unwrap();
        assert_eq!(desc.property_code, DevicePropertyCode::ExposureIndex);
        assert!(desc.writable);
        assert_eq!(desc.current_value, PropertyValue::Uint16(800));
        assert_eq!(desc.form_type, PropertyFormType::Enumeration);
        let values = desc.enum_values.unwrap();
        assert_eq!(
            values,
            vec![
                PropertyValue::Uint16(100),
                PropertyValue::Uint16(200),
                PropertyValue::Uint16(400),
                PropertyValue::Uint16(800),
            ]
        );
    }

    fn build_datetime_prop_desc() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&pack_u16(0x5011)); // DateTime
        buf.extend_from_slice(&pack_u16(0xFFFF)); // String
        buf.push(0x01); // read-write
        buf.push(0x00); // default: empty
        buf.extend_from_slice(&pack_string("20240315T120000").unwrap());
        buf.push(0x00); // None
        buf
    }

    #[test]
    fn device_prop_desc_parse_datetime_no_form() {
        let desc = DevicePropDesc::from_bytes(&build_datetime_prop_desc()).unwrap();
        assert_eq!(desc.property_code, DevicePropertyCode::DateTime);
        assert_eq!(desc.data_type, PropertyDataType::String);
        assert_eq!(
            desc.current_value,
            PropertyValue::String("20240315T120000".into())
        );
        assert_eq!(desc.form_type, PropertyFormType::None);
    }

    fn build_exposure_bias_prop_desc() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&pack_u16(0x5010)); // ExposureBiasCompensation
        buf.extend_from_slice(&pack_u16(0x0003)); // INT16
        buf.push(0x01);
        buf.extend_from_slice(&pack_i16(0)); // default
        buf.extend_from_slice(&pack_i16(-1000)); // current
        buf.push(0x01); // Range
        buf.extend_from_slice(&pack_i16(-3000));
        buf.extend_from_slice(&pack_i16(3000));
        buf.extend_from_slice(&pack_i16(333));
        buf
    }

    #[test]
    fn device_prop_desc_parse_exposure_bias_signed() {
        let desc = DevicePropDesc::from_bytes(&build_exposure_bias_prop_desc()).unwrap();
        assert_eq!(
            desc.property_code,
            DevicePropertyCode::ExposureBiasCompensation
        );
        assert_eq!(desc.current_value, PropertyValue::Int16(-1000));
        let range = desc.range.unwrap();
        assert_eq!(range.min, PropertyValue::Int16(-3000));
        assert_eq!(range.max, PropertyValue::Int16(3000));
    }

    #[test]
    fn device_prop_desc_parse_insufficient_bytes() {
        assert!(DevicePropDesc::from_bytes(&[0x01]).is_err());
        assert!(DevicePropDesc::from_bytes(&[0x01, 0x50]).is_err());
    }

    #[test]
    fn device_prop_desc_minimum_valid() {
        assert!(DevicePropDesc::from_bytes(&[]).is_err());
        assert!(DevicePropDesc::from_bytes(&[0; 4]).is_err());
    }

    // --- Property-based tests ---

    proptest! {
        #[test]
        fn prop_property_form_type_known_roundtrip(code in 0u8..=2u8) {
            let form = PropertyFormType::from_code(code);
            prop_assert_eq!(form.to_code(), code);
        }

        #[test]
        fn prop_property_form_type_unknown_preserves_code(code in 3u8..=u8::MAX) {
            let form = PropertyFormType::from_code(code);
            prop_assert_eq!(form, PropertyFormType::Unknown(code));
            prop_assert_eq!(form.to_code(), code);
        }

        #[test]
        fn prop_property_value_int8_roundtrip(val: i8) {
            let pv = PropertyValue::Int8(val);
            let (parsed, _) = PropertyValue::from_bytes(&pv.to_bytes().unwrap(), PropertyDataType::Int8).unwrap();
            prop_assert_eq!(parsed, pv);
        }

        #[test]
        fn prop_property_value_int16_roundtrip(val: i16) {
            let pv = PropertyValue::Int16(val);
            let (parsed, _) = PropertyValue::from_bytes(&pv.to_bytes().unwrap(), PropertyDataType::Int16).unwrap();
            prop_assert_eq!(parsed, pv);
        }

        #[test]
        fn prop_property_value_int32_roundtrip(val: i32) {
            let pv = PropertyValue::Int32(val);
            let (parsed, _) = PropertyValue::from_bytes(&pv.to_bytes().unwrap(), PropertyDataType::Int32).unwrap();
            prop_assert_eq!(parsed, pv);
        }

        #[test]
        fn prop_property_value_int64_roundtrip(val: i64) {
            let pv = PropertyValue::Int64(val);
            let (parsed, _) = PropertyValue::from_bytes(&pv.to_bytes().unwrap(), PropertyDataType::Int64).unwrap();
            prop_assert_eq!(parsed, pv);
        }

        #[test]
        fn prop_property_value_uint16_roundtrip(val: u16) {
            let pv = PropertyValue::Uint16(val);
            let (parsed, _) = PropertyValue::from_bytes(&pv.to_bytes().unwrap(), PropertyDataType::Uint16).unwrap();
            prop_assert_eq!(parsed, pv);
        }

        #[test]
        fn prop_property_value_uint32_roundtrip(val: u32) {
            let pv = PropertyValue::Uint32(val);
            let (parsed, _) = PropertyValue::from_bytes(&pv.to_bytes().unwrap(), PropertyDataType::Uint32).unwrap();
            prop_assert_eq!(parsed, pv);
        }

        #[test]
        fn prop_property_value_uint64_roundtrip(val: u64) {
            let pv = PropertyValue::Uint64(val);
            let (parsed, _) = PropertyValue::from_bytes(&pv.to_bytes().unwrap(), PropertyDataType::Uint64).unwrap();
            prop_assert_eq!(parsed, pv);
        }

        #[test]
        fn prop_property_range_uint8_roundtrip(min: u8, max: u8, step: u8) {
            let range = PropertyRange {
                min: PropertyValue::Uint8(min),
                max: PropertyValue::Uint8(max),
                step: PropertyValue::Uint8(step),
            };
            let (parsed, _) = PropertyRange::from_bytes(&range.to_bytes().unwrap(), PropertyDataType::Uint8).unwrap();
            prop_assert_eq!(parsed.min, range.min);
            prop_assert_eq!(parsed.max, range.max);
            prop_assert_eq!(parsed.step, range.step);
        }

        #[test]
        fn prop_property_range_uint16_roundtrip(min: u16, max: u16, step: u16) {
            let range = PropertyRange {
                min: PropertyValue::Uint16(min),
                max: PropertyValue::Uint16(max),
                step: PropertyValue::Uint16(step),
            };
            let (parsed, _) = PropertyRange::from_bytes(&range.to_bytes().unwrap(), PropertyDataType::Uint16).unwrap();
            prop_assert_eq!(parsed.min, range.min);
            prop_assert_eq!(parsed.max, range.max);
        }

        #[test]
        fn prop_property_range_int16_roundtrip(min: i16, max: i16, step: i16) {
            let range = PropertyRange {
                min: PropertyValue::Int16(min),
                max: PropertyValue::Int16(max),
                step: PropertyValue::Int16(step),
            };
            let (parsed, _) = PropertyRange::from_bytes(&range.to_bytes().unwrap(), PropertyDataType::Int16).unwrap();
            prop_assert_eq!(parsed.min, range.min);
            prop_assert_eq!(parsed.max, range.max);
        }

        #[test]
        fn prop_property_range_uint32_roundtrip(min: u32, max: u32, step: u32) {
            let range = PropertyRange {
                min: PropertyValue::Uint32(min),
                max: PropertyValue::Uint32(max),
                step: PropertyValue::Uint32(step),
            };
            let (parsed, _) = PropertyRange::from_bytes(&range.to_bytes().unwrap(), PropertyDataType::Uint32).unwrap();
            prop_assert_eq!(parsed.min, range.min);
            prop_assert_eq!(parsed.max, range.max);
        }
    }

    // --- Fuzz tests ---

    fn valid_property_string() -> impl Strategy<Value = String> {
        // NUL is excluded: PTP strings are null-terminated, so a NUL can
        // never appear inside one (unpack_string truncates at the first NUL).
        prop::collection::vec(
            prop::char::range('\u{0001}', '\u{D7FF}')
                .prop_union(prop::char::range('\u{E000}', '\u{FFFF}')),
            0..50,
        )
        .prop_map(|chars| chars.into_iter().collect::<String>())
    }

    proptest! {
        #[test]
        fn prop_property_value_string_roundtrip(s in valid_property_string()) {
            let s = if s.chars().count() > 254 { s.chars().take(254).collect() } else { s };
            let pv = PropertyValue::String(s.clone());
            let (parsed, _) = PropertyValue::from_bytes(&pv.to_bytes().unwrap(), PropertyDataType::String).unwrap();
            prop_assert_eq!(parsed, PropertyValue::String(s));
        }

        #[test]
        fn fuzz_property_value_truncated(data_type_code in 1u16..=8u16, bytes in prop::collection::vec(any::<u8>(), 0..20)) {
            let _ = PropertyValue::from_bytes(&bytes, PropertyDataType::from(data_type_code));
        }

        #[test]
        fn fuzz_property_value_empty(data_type_code in 1u16..=8u16) {
            prop_assert!(PropertyValue::from_bytes(&[], PropertyDataType::from(data_type_code)).is_err());
        }

        #[test]
        fn fuzz_property_value_string_garbage(bytes in prop::collection::vec(any::<u8>(), 0..50)) {
            let _ = PropertyValue::from_bytes(&bytes, PropertyDataType::String);
        }

        #[test]
        fn fuzz_property_value_unsupported(bytes in prop::collection::vec(any::<u8>(), 0..20)) {
            prop_assert!(PropertyValue::from_bytes(&bytes, PropertyDataType::Undefined).is_err());
            prop_assert!(PropertyValue::from_bytes(&bytes, PropertyDataType::Int128).is_err());
            prop_assert!(PropertyValue::from_bytes(&bytes, PropertyDataType::Uint128).is_err());
        }

        #[test]
        fn fuzz_property_value_unknown_type(unknown_code in 11u16..=0xFFFEu16, bytes in prop::collection::vec(any::<u8>(), 0..20)) {
            prop_assert!(PropertyValue::from_bytes(&bytes, PropertyDataType::Unknown(unknown_code)).is_err());
        }

        #[test]
        fn fuzz_property_range_truncated(data_type_code in 1u16..=8u16, bytes in prop::collection::vec(any::<u8>(), 0..20)) {
            let _ = PropertyRange::from_bytes(&bytes, PropertyDataType::from(data_type_code));
        }

        #[test]
        fn fuzz_property_range_wrong_type(bytes in prop::collection::vec(any::<u8>(), 0..20)) {
            prop_assert!(PropertyRange::from_bytes(&bytes, PropertyDataType::Undefined).is_err());
            prop_assert!(PropertyRange::from_bytes(&bytes, PropertyDataType::Unknown(0x1234)).is_err());
        }
    }

    crate::fuzz_bytes!(fuzz_device_prop_desc, DevicePropDesc, 200);
}
