//! Device and storage information types for MTP/PTP.
//!
//! This module contains:
//! - [`DeviceInfo`]: Device capabilities and identification
//! - [`StorageInfo`]: Storage characteristics and capacity

use super::storage::{AccessCapability, FilesystemType, StorageType};
use crate::ptp::pack::{unpack_string, unpack_u16, unpack_u16_array, unpack_u32, unpack_u64};
use crate::ptp::{EventCode, ObjectFormatCode, OperationCode};

// --- DeviceInfo Structure ---

/// Device information returned by GetDeviceInfo.
///
/// Contains device capabilities, manufacturer info, and supported operations.
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    /// PTP standard version (e.g., 100 = v1.00).
    pub standard_version: u16,
    /// Vendor extension ID (0 = no extension).
    pub vendor_extension_id: u32,
    /// Vendor extension version.
    pub vendor_extension_version: u16,
    /// Vendor extension description.
    pub vendor_extension_desc: String,
    /// Functional mode (0 = standard).
    pub functional_mode: u16,
    /// Operations supported by the device.
    pub operations_supported: Vec<OperationCode>,
    /// Events supported by the device.
    pub events_supported: Vec<EventCode>,
    /// Device properties supported.
    pub device_properties_supported: Vec<u16>,
    /// Object formats the device can capture/create.
    pub capture_formats: Vec<ObjectFormatCode>,
    /// Object formats the device can play/display.
    pub playback_formats: Vec<ObjectFormatCode>,
    /// Manufacturer name.
    pub manufacturer: String,
    /// Device model name.
    pub model: String,
    /// Device version string.
    pub device_version: String,
    /// Device serial number.
    pub serial_number: String,
}

impl DeviceInfo {
    /// Parse DeviceInfo from a byte buffer.
    ///
    /// The buffer should contain the DeviceInfo dataset as returned by GetDeviceInfo.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, crate::PtpError> {
        let mut offset = 0;

        // 1. StandardVersion (u16)
        let standard_version = unpack_u16(&buf[offset..])?;
        offset += 2;

        // 2. VendorExtensionID (u32)
        let vendor_extension_id = unpack_u32(&buf[offset..])?;
        offset += 4;

        // 3. VendorExtensionVersion (u16)
        let vendor_extension_version = unpack_u16(&buf[offset..])?;
        offset += 2;

        // 4. VendorExtensionDesc (string)
        let (vendor_extension_desc, consumed) = unpack_string(&buf[offset..])?;
        offset += consumed;

        // 5. FunctionalMode (u16)
        let functional_mode = unpack_u16(&buf[offset..])?;
        offset += 2;

        // 6. OperationsSupported (u16 array)
        let (ops_raw, consumed) = unpack_u16_array(&buf[offset..])?;
        let operations_supported: Vec<OperationCode> =
            ops_raw.into_iter().map(OperationCode::from).collect();
        offset += consumed;

        // 7. EventsSupported (u16 array)
        let (events_raw, consumed) = unpack_u16_array(&buf[offset..])?;
        let events_supported: Vec<EventCode> =
            events_raw.into_iter().map(EventCode::from).collect();
        offset += consumed;

        // 8. DevicePropertiesSupported (u16 array)
        let (device_properties_supported, consumed) = unpack_u16_array(&buf[offset..])?;
        offset += consumed;

        // 9. CaptureFormats (u16 array)
        let (capture_raw, consumed) = unpack_u16_array(&buf[offset..])?;
        let capture_formats: Vec<ObjectFormatCode> = capture_raw
            .into_iter()
            .map(ObjectFormatCode::from)
            .collect();
        offset += consumed;

        // 10. PlaybackFormats (u16 array)
        let (playback_raw, consumed) = unpack_u16_array(&buf[offset..])?;
        let playback_formats: Vec<ObjectFormatCode> = playback_raw
            .into_iter()
            .map(ObjectFormatCode::from)
            .collect();
        offset += consumed;

        // 11. Manufacturer (string)
        let (manufacturer, consumed) = unpack_string(&buf[offset..])?;
        offset += consumed;

        // 12. Model (string)
        let (model, consumed) = unpack_string(&buf[offset..])?;
        offset += consumed;

        // 13. DeviceVersion (string)
        let (device_version, consumed) = unpack_string(&buf[offset..])?;
        offset += consumed;

        // 14. SerialNumber (string)
        let (serial_number, _) = unpack_string(&buf[offset..])?;

        Ok(DeviceInfo {
            standard_version,
            vendor_extension_id,
            vendor_extension_version,
            vendor_extension_desc,
            functional_mode,
            operations_supported,
            events_supported,
            device_properties_supported,
            capture_formats,
            playback_formats,
            manufacturer,
            model,
            device_version,
            serial_number,
        })
    }

    /// Check if the device supports a specific operation.
    ///
    /// # Arguments
    ///
    /// * `operation` - The operation code to check
    ///
    /// # Returns
    ///
    /// Returns true if the operation is in the device's supported operations list.
    #[must_use]
    pub fn supports_operation(&self, operation: OperationCode) -> bool {
        self.operations_supported.contains(&operation)
    }

    /// Check if the device supports renaming objects.
    ///
    /// This checks for support of the SetObjectPropValue operation (0x9804),
    /// which is required to rename files and folders via the ObjectFileName property.
    ///
    /// # Returns
    ///
    /// Returns true if the device advertises SetObjectPropValue support.
    #[must_use]
    pub fn supports_rename(&self) -> bool {
        self.supports_operation(OperationCode::SetObjectPropValue)
    }

    /// Check if the device supports creating objects (uploads and folders).
    ///
    /// This checks for support of both SendObjectInfo (0x100C) and
    /// SendObject (0x100D), which together form the two-phase object
    /// creation flow. Read-only devices (for example, PTP cameras) typically
    /// don't advertise these.
    ///
    /// Note: some devices advertise these operations but still reject writes
    /// per-storage or per-format (Fuji cameras report ReadWrite capability yet
    /// return StoreReadOnly). A `true` here means uploads are worth attempting,
    /// not that they're guaranteed to succeed.
    ///
    /// # Returns
    ///
    /// Returns true if the device advertises both SendObjectInfo and SendObject.
    #[must_use]
    pub fn supports_upload(&self) -> bool {
        self.supports_operation(OperationCode::SendObjectInfo)
            && self.supports_operation(OperationCode::SendObject)
    }
}

// --- StorageInfo Structure ---

/// Storage information returned by GetStorageInfo.
///
/// Contains storage capacity, type, and access information.
#[derive(Debug, Clone, Default)]
pub struct StorageInfo {
    /// Type of storage medium.
    pub storage_type: StorageType,
    /// Type of filesystem.
    pub filesystem_type: FilesystemType,
    /// Access capability.
    pub access_capability: AccessCapability,
    /// Maximum storage capacity in bytes.
    pub max_capacity: u64,
    /// Free space in bytes.
    pub free_space_bytes: u64,
    /// Free space in number of objects (0xFFFFFFFF if unknown).
    pub free_space_objects: u32,
    /// Storage description string.
    pub description: String,
    /// Volume identifier/label.
    pub volume_identifier: String,
}

impl StorageInfo {
    /// Parse StorageInfo from a byte buffer.
    ///
    /// The buffer should contain the StorageInfo dataset as returned by GetStorageInfo.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, crate::PtpError> {
        let mut offset = 0;

        // 1. StorageType (u16)
        let storage_type = StorageType::from(unpack_u16(&buf[offset..])?);
        offset += 2;

        // 2. FilesystemType (u16)
        let filesystem_type = FilesystemType::from(unpack_u16(&buf[offset..])?);
        offset += 2;

        // 3. AccessCapability (u16)
        let access_capability = AccessCapability::from(unpack_u16(&buf[offset..])?);
        offset += 2;

        // 4. MaxCapacity (u64)
        let max_capacity = unpack_u64(&buf[offset..])?;
        offset += 8;

        // 5. FreeSpaceInBytes (u64)
        let free_space_bytes = unpack_u64(&buf[offset..])?;
        offset += 8;

        // 6. FreeSpaceInObjects (u32)
        let free_space_objects = unpack_u32(&buf[offset..])?;
        offset += 4;

        // 7. StorageDescription (string)
        let (description, consumed) = unpack_string(&buf[offset..])?;
        offset += consumed;

        // 8. VolumeIdentifier (string)
        let (volume_identifier, _) = unpack_string(&buf[offset..])?;

        Ok(StorageInfo {
            storage_type,
            filesystem_type,
            access_capability,
            max_capacity,
            free_space_bytes,
            free_space_objects,
            description,
            volume_identifier,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::pack::{pack_string, pack_u16, pack_u16_array, pack_u32};

    // --- DeviceInfo Tests ---

    fn build_minimal_device_info_bytes() -> Vec<u8> {
        let mut buf = Vec::new();

        // StandardVersion: 100 (v1.00)
        buf.extend_from_slice(&pack_u16(100));
        // VendorExtensionID: 0
        buf.extend_from_slice(&pack_u32(0));
        // VendorExtensionVersion: 0
        buf.extend_from_slice(&pack_u16(0));
        // VendorExtensionDesc: empty string
        buf.push(0x00);
        // FunctionalMode: 0
        buf.extend_from_slice(&pack_u16(0));
        // OperationsSupported: empty array
        buf.extend_from_slice(&pack_u16_array(&[]));
        // EventsSupported: empty array
        buf.extend_from_slice(&pack_u16_array(&[]));
        // DevicePropertiesSupported: empty array
        buf.extend_from_slice(&pack_u16_array(&[]));
        // CaptureFormats: empty array
        buf.extend_from_slice(&pack_u16_array(&[]));
        // PlaybackFormats: empty array
        buf.extend_from_slice(&pack_u16_array(&[]));
        // Manufacturer: empty string
        buf.push(0x00);
        // Model: empty string
        buf.push(0x00);
        // DeviceVersion: empty string
        buf.push(0x00);
        // SerialNumber: empty string
        buf.push(0x00);

        buf
    }

    #[test]
    fn device_info_parse_minimal() {
        let buf = build_minimal_device_info_bytes();
        let info = DeviceInfo::from_bytes(&buf).unwrap();

        assert_eq!(info.standard_version, 100);
        assert_eq!(info.vendor_extension_id, 0);
        assert_eq!(info.vendor_extension_version, 0);
        assert_eq!(info.vendor_extension_desc, "");
        assert_eq!(info.functional_mode, 0);
        assert!(info.operations_supported.is_empty());
        assert!(info.events_supported.is_empty());
        assert!(info.device_properties_supported.is_empty());
        assert!(info.capture_formats.is_empty());
        assert!(info.playback_formats.is_empty());
        assert_eq!(info.manufacturer, "");
        assert_eq!(info.model, "");
        assert_eq!(info.device_version, "");
        assert_eq!(info.serial_number, "");
    }

    fn build_full_device_info_bytes() -> Vec<u8> {
        let mut buf = Vec::new();

        // StandardVersion: 100 (v1.00)
        buf.extend_from_slice(&pack_u16(100));
        // VendorExtensionID: 0x00000006 (Microsoft)
        buf.extend_from_slice(&pack_u32(6));
        // VendorExtensionVersion: 100
        buf.extend_from_slice(&pack_u16(100));
        // VendorExtensionDesc: "microsoft.com: 1.0"
        buf.extend_from_slice(&pack_string("microsoft.com: 1.0").unwrap());
        // FunctionalMode: 0
        buf.extend_from_slice(&pack_u16(0));
        // OperationsSupported: [GetDeviceInfo, OpenSession, CloseSession]
        buf.extend_from_slice(&pack_u16_array(&[0x1001, 0x1002, 0x1003]));
        // EventsSupported: [ObjectAdded, ObjectRemoved]
        buf.extend_from_slice(&pack_u16_array(&[0x4002, 0x4003]));
        // DevicePropertiesSupported: [0x5001, 0x5002]
        buf.extend_from_slice(&pack_u16_array(&[0x5001, 0x5002]));
        // CaptureFormats: [JPEG]
        buf.extend_from_slice(&pack_u16_array(&[0x3801]));
        // PlaybackFormats: [JPEG, MP3]
        buf.extend_from_slice(&pack_u16_array(&[0x3801, 0x3009]));
        // Manufacturer: "Test Manufacturer"
        buf.extend_from_slice(&pack_string("Test Manufacturer").unwrap());
        // Model: "Test Model"
        buf.extend_from_slice(&pack_string("Test Model").unwrap());
        // DeviceVersion: "1.0.0"
        buf.extend_from_slice(&pack_string("1.0.0").unwrap());
        // SerialNumber: "ABC123"
        buf.extend_from_slice(&pack_string("ABC123").unwrap());

        buf
    }

    #[test]
    fn device_info_parse_full() {
        let buf = build_full_device_info_bytes();
        let info = DeviceInfo::from_bytes(&buf).unwrap();

        assert_eq!(info.standard_version, 100);
        assert_eq!(info.vendor_extension_id, 6);
        assert_eq!(info.vendor_extension_version, 100);
        assert_eq!(info.vendor_extension_desc, "microsoft.com: 1.0");
        assert_eq!(info.functional_mode, 0);

        assert_eq!(info.operations_supported.len(), 3);
        assert_eq!(info.operations_supported[0], OperationCode::GetDeviceInfo);
        assert_eq!(info.operations_supported[1], OperationCode::OpenSession);
        assert_eq!(info.operations_supported[2], OperationCode::CloseSession);

        assert_eq!(info.events_supported.len(), 2);
        assert_eq!(info.events_supported[0], EventCode::ObjectAdded);
        assert_eq!(info.events_supported[1], EventCode::ObjectRemoved);

        assert_eq!(info.device_properties_supported, vec![0x5001, 0x5002]);

        assert_eq!(info.capture_formats.len(), 1);
        assert_eq!(info.capture_formats[0], ObjectFormatCode::Jpeg);

        assert_eq!(info.playback_formats.len(), 2);
        assert_eq!(info.playback_formats[0], ObjectFormatCode::Jpeg);
        assert_eq!(info.playback_formats[1], ObjectFormatCode::Mp3);

        assert_eq!(info.manufacturer, "Test Manufacturer");
        assert_eq!(info.model, "Test Model");
        assert_eq!(info.device_version, "1.0.0");
        assert_eq!(info.serial_number, "ABC123");
    }

    #[test]
    fn device_info_parse_insufficient_bytes() {
        let buf = vec![0x00, 0x01]; // Only 2 bytes
        assert!(DeviceInfo::from_bytes(&buf).is_err());
    }

    // --- StorageInfo Tests ---

    fn build_storage_info_bytes() -> Vec<u8> {
        let mut buf = Vec::new();

        // StorageType: RemovableRam (4)
        buf.extend_from_slice(&pack_u16(4));
        // FilesystemType: GenericHierarchical (2)
        buf.extend_from_slice(&pack_u16(2));
        // AccessCapability: ReadWrite (0)
        buf.extend_from_slice(&pack_u16(0));
        // MaxCapacity: 32GB
        buf.extend_from_slice(&32_000_000_000u64.to_le_bytes());
        // FreeSpaceInBytes: 16GB
        buf.extend_from_slice(&16_000_000_000u64.to_le_bytes());
        // FreeSpaceInObjects: 0xFFFFFFFF (unknown)
        buf.extend_from_slice(&pack_u32(0xFFFFFFFF));
        // StorageDescription: "SD Card"
        buf.extend_from_slice(&pack_string("SD Card").unwrap());
        // VolumeIdentifier: "VOL001"
        buf.extend_from_slice(&pack_string("VOL001").unwrap());

        buf
    }

    #[test]
    fn storage_info_parse() {
        let buf = build_storage_info_bytes();
        let info = StorageInfo::from_bytes(&buf).unwrap();

        assert_eq!(info.storage_type, StorageType::RemovableRam);
        assert_eq!(info.filesystem_type, FilesystemType::GenericHierarchical);
        assert_eq!(info.access_capability, AccessCapability::ReadWrite);
        assert_eq!(info.max_capacity, 32_000_000_000);
        assert_eq!(info.free_space_bytes, 16_000_000_000);
        assert_eq!(info.free_space_objects, 0xFFFFFFFF);
        assert_eq!(info.description, "SD Card");
        assert_eq!(info.volume_identifier, "VOL001");
    }

    #[test]
    fn storage_info_parse_insufficient_bytes() {
        let buf = vec![0x00; 10]; // Not enough bytes
        assert!(StorageInfo::from_bytes(&buf).is_err());
    }

    // --- DeviceInfo capability tests ---

    #[test]
    fn device_info_supports_operation() {
        let info = DeviceInfo {
            operations_supported: vec![
                OperationCode::GetDeviceInfo,
                OperationCode::OpenSession,
                OperationCode::SetObjectPropValue,
            ],
            ..Default::default()
        };

        assert!(info.supports_operation(OperationCode::GetDeviceInfo));
        assert!(info.supports_operation(OperationCode::OpenSession));
        assert!(info.supports_operation(OperationCode::SetObjectPropValue));
        assert!(!info.supports_operation(OperationCode::DeleteObject));
        assert!(!info.supports_operation(OperationCode::GetObjectPropValue));
    }

    #[test]
    fn device_info_supports_rename_true() {
        let info = DeviceInfo {
            operations_supported: vec![
                OperationCode::GetDeviceInfo,
                OperationCode::SetObjectPropValue, // Required for rename
            ],
            ..Default::default()
        };

        assert!(info.supports_rename());
    }

    #[test]
    fn device_info_supports_rename_false() {
        let info = DeviceInfo {
            operations_supported: vec![
                OperationCode::GetDeviceInfo,
                OperationCode::GetObjectPropValue, // Has Get but not Set
            ],
            ..Default::default()
        };

        assert!(!info.supports_rename());
    }

    #[test]
    fn device_info_supports_rename_empty() {
        let info = DeviceInfo::default();
        assert!(!info.supports_rename());
    }

    #[test]
    fn device_info_supports_upload_true() {
        let info = DeviceInfo {
            operations_supported: vec![
                OperationCode::GetDeviceInfo,
                OperationCode::SendObjectInfo,
                OperationCode::SendObject,
            ],
            ..Default::default()
        };

        assert!(info.supports_upload());
    }

    #[test]
    fn device_info_supports_upload_false_missing_send_object() {
        let info = DeviceInfo {
            operations_supported: vec![OperationCode::SendObjectInfo],
            ..Default::default()
        };

        assert!(!info.supports_upload());
    }

    #[test]
    fn device_info_supports_upload_false_missing_send_object_info() {
        let info = DeviceInfo {
            operations_supported: vec![OperationCode::SendObject],
            ..Default::default()
        };

        assert!(!info.supports_upload());
    }

    #[test]
    fn device_info_supports_upload_empty() {
        let info = DeviceInfo::default();
        assert!(!info.supports_upload());
    }

    // Fuzz tests using shared macros - verify parsers don't panic on arbitrary input
    crate::fuzz_bytes!(fuzz_device_info, DeviceInfo, 200);
    crate::fuzz_bytes!(fuzz_storage_info, StorageInfo, 100);

    #[test]
    fn device_info_minimum_valid() {
        // DeviceInfo needs at minimum: u16 + u32 + u16 + string + u16 + 5 arrays + 4 strings
        // This is a lot of bytes. Test that small buffers fail gracefully.
        assert!(DeviceInfo::from_bytes(&[]).is_err());
        assert!(DeviceInfo::from_bytes(&[0; 1]).is_err());
        assert!(DeviceInfo::from_bytes(&[0; 7]).is_err());
        assert!(DeviceInfo::from_bytes(&[0; 8]).is_err()); // Need at least string data after first fields
    }

    #[test]
    fn storage_info_minimum_valid() {
        // StorageInfo needs: 3 * u16 + 2 * u64 + u32 + 2 strings = 26 bytes minimum + string data
        assert!(StorageInfo::from_bytes(&[]).is_err());
        assert!(StorageInfo::from_bytes(&[0; 25]).is_err());
        assert!(StorageInfo::from_bytes(&[0; 26]).is_err()); // Still need string data
    }

    #[test]
    fn storage_info_max_capacity() {
        let mut buf = build_storage_info_bytes();
        // Replace MaxCapacity field (bytes 6-13, after 3 u16s = 6 bytes)
        let max_bytes = u64::MAX.to_le_bytes();
        buf[6..14].copy_from_slice(&max_bytes);

        let info = StorageInfo::from_bytes(&buf).unwrap();
        assert_eq!(info.max_capacity, u64::MAX);
    }
}
