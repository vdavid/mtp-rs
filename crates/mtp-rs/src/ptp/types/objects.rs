//! Object-related types for MTP/PTP.
//!
//! This module contains the [`ObjectInfo`] structure for file/folder metadata.

use super::storage::{AssociationType, ProtectionStatus};
use crate::ptp::pack::{
    pack_datetime, pack_string, pack_u16, pack_u32, unpack_datetime, unpack_string, unpack_u16,
    unpack_u32, DateTime,
};
use crate::ptp::{ObjectFormatCode, ObjectHandle, StorageId};

// --- ObjectInfo Structure ---

/// Object information returned by GetObjectInfo.
///
/// Contains file/folder metadata including name, size, timestamps, and hierarchy info.
#[derive(Debug, Clone, Default)]
pub struct ObjectInfo {
    /// Object handle (set after parsing, not part of protocol data).
    pub handle: ObjectHandle,
    /// Storage containing this object.
    pub storage_id: StorageId,
    /// Object format code.
    pub format: ObjectFormatCode,
    /// Protection status.
    pub protection_status: ProtectionStatus,
    /// Object size in bytes.
    ///
    /// The standard `ObjectInfo` dataset encodes size as u32 which saturates at
    /// `u32::MAX` for files larger than 4 GB. When this happens and the dataset
    /// is obtained via high-level APIs like [`Storage::get_object_info`],
    /// [`Storage::list_objects`], or [`PtpSession::get_object_info_full`], the
    /// real u64 size is auto-resolved via `GetObjectPropValue(ObjectSize)`.
    ///
    /// [`Storage::get_object_info`]: crate::mtp::Storage::get_object_info
    /// [`Storage::list_objects`]: crate::mtp::Storage::list_objects
    /// [`PtpSession::get_object_info_full`]: crate::ptp::PtpSession::get_object_info_full
    pub size: u64,
    /// Thumbnail format.
    pub thumb_format: ObjectFormatCode,
    /// Thumbnail size in bytes.
    pub thumb_size: u32,
    /// Thumbnail width in pixels.
    pub thumb_width: u32,
    /// Thumbnail height in pixels.
    pub thumb_height: u32,
    /// Image width in pixels.
    pub image_width: u32,
    /// Image height in pixels.
    pub image_height: u32,
    /// Image bit depth.
    pub image_bit_depth: u32,
    /// Parent object handle (ROOT for root-level objects).
    pub parent: ObjectHandle,
    /// Association type (folder type).
    pub association_type: AssociationType,
    /// Association description.
    pub association_desc: u32,
    /// Sequence number.
    pub sequence_number: u32,
    /// Filename.
    pub filename: String,
    /// Creation timestamp.
    pub created: Option<DateTime>,
    /// Modification timestamp.
    pub modified: Option<DateTime>,
    /// Keywords string.
    pub keywords: String,
}

impl ObjectInfo {
    /// Parse ObjectInfo from a byte buffer.
    ///
    /// The buffer should contain the ObjectInfo dataset as returned by GetObjectInfo.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, crate::PtpError> {
        let mut offset = 0;

        // 1. StorageID (u32)
        let storage_id = StorageId(unpack_u32(&buf[offset..])?);
        offset += 4;

        // 2. ObjectFormat (u16)
        let format = ObjectFormatCode::from(unpack_u16(&buf[offset..])?);
        offset += 2;

        // 3. ProtectionStatus (u16)
        let protection_status = ProtectionStatus::from(unpack_u16(&buf[offset..])?);
        offset += 2;

        // 4. ObjectCompressedSize (u32) - stored as u64, but protocol uses u32
        let size = unpack_u32(&buf[offset..])? as u64;
        offset += 4;

        // 5. ThumbFormat (u16)
        let thumb_format = ObjectFormatCode::from(unpack_u16(&buf[offset..])?);
        offset += 2;

        // 6. ThumbCompressedSize (u32)
        let thumb_size = unpack_u32(&buf[offset..])?;
        offset += 4;

        // 7. ThumbPixWidth (u32)
        let thumb_width = unpack_u32(&buf[offset..])?;
        offset += 4;

        // 8. ThumbPixHeight (u32)
        let thumb_height = unpack_u32(&buf[offset..])?;
        offset += 4;

        // 9. ImagePixWidth (u32)
        let image_width = unpack_u32(&buf[offset..])?;
        offset += 4;

        // 10. ImagePixHeight (u32)
        let image_height = unpack_u32(&buf[offset..])?;
        offset += 4;

        // 11. ImageBitDepth (u32)
        let image_bit_depth = unpack_u32(&buf[offset..])?;
        offset += 4;

        // 12. ParentObject (u32)
        let parent = ObjectHandle(unpack_u32(&buf[offset..])?);
        offset += 4;

        // 13. AssociationType (u16)
        let association_type = AssociationType::from(unpack_u16(&buf[offset..])?);
        offset += 2;

        // 14. AssociationDesc (u32)
        let association_desc = unpack_u32(&buf[offset..])?;
        offset += 4;

        // 15. SequenceNumber (u32)
        let sequence_number = unpack_u32(&buf[offset..])?;
        offset += 4;

        // 16. Filename (string)
        let (filename, consumed) = unpack_string(&buf[offset..])?;
        offset += consumed;

        // 17. DateCreated (datetime string)
        let (created, consumed) = unpack_datetime(&buf[offset..])?;
        offset += consumed;

        // 18. DateModified (datetime string)
        let (modified, consumed) = unpack_datetime(&buf[offset..])?;
        offset += consumed;

        // 19. Keywords (string)
        let (keywords, _consumed) = unpack_string(&buf[offset..])?;

        Ok(ObjectInfo {
            handle: ObjectHandle::default(), // Set by caller after parsing
            storage_id,
            format,
            protection_status,
            size,
            thumb_format,
            thumb_size,
            thumb_width,
            thumb_height,
            image_width,
            image_height,
            image_bit_depth,
            parent,
            association_type,
            association_desc,
            sequence_number,
            filename,
            created,
            modified,
            keywords,
        })
    }

    /// Serialize ObjectInfo to a byte buffer.
    ///
    /// Used for SendObjectInfo operation.
    ///
    /// Returns an error if the created or modified DateTime contains invalid values.
    pub fn to_bytes(&self) -> Result<Vec<u8>, crate::PtpError> {
        let mut buf = Vec::new();

        // 1. StorageID (u32)
        buf.extend_from_slice(&pack_u32(self.storage_id.0));

        // 2. ObjectFormat (u16)
        buf.extend_from_slice(&pack_u16(self.format.into()));

        // 3. ProtectionStatus (u16)
        buf.extend_from_slice(&pack_u16(self.protection_status.into()));

        // 4. ObjectCompressedSize (u32) - cap at u32::MAX for >4GB files
        let size_u32 = if self.size > u32::MAX as u64 {
            u32::MAX
        } else {
            self.size as u32
        };
        buf.extend_from_slice(&pack_u32(size_u32));

        // 5. ThumbFormat (u16)
        buf.extend_from_slice(&pack_u16(self.thumb_format.into()));

        // 6. ThumbCompressedSize (u32)
        buf.extend_from_slice(&pack_u32(self.thumb_size));

        // 7. ThumbPixWidth (u32)
        buf.extend_from_slice(&pack_u32(self.thumb_width));

        // 8. ThumbPixHeight (u32)
        buf.extend_from_slice(&pack_u32(self.thumb_height));

        // 9. ImagePixWidth (u32)
        buf.extend_from_slice(&pack_u32(self.image_width));

        // 10. ImagePixHeight (u32)
        buf.extend_from_slice(&pack_u32(self.image_height));

        // 11. ImageBitDepth (u32)
        buf.extend_from_slice(&pack_u32(self.image_bit_depth));

        // 12. ParentObject (u32)
        buf.extend_from_slice(&pack_u32(self.parent.0));

        // 13. AssociationType (u16)
        buf.extend_from_slice(&pack_u16(self.association_type.into()));

        // 14. AssociationDesc (u32)
        buf.extend_from_slice(&pack_u32(self.association_desc));

        // 15. SequenceNumber (u32)
        buf.extend_from_slice(&pack_u32(self.sequence_number));

        // 16. Filename (string)
        buf.extend_from_slice(&pack_string(&self.filename));

        // 17. DateCreated (datetime string)
        if let Some(dt) = &self.created {
            buf.extend_from_slice(&pack_datetime(dt)?);
        } else {
            buf.push(0x00); // Empty string
        }

        // 18. DateModified (datetime string)
        if let Some(dt) = &self.modified {
            buf.extend_from_slice(&pack_datetime(dt)?);
        } else {
            buf.push(0x00); // Empty string
        }

        // 19. Keywords (string)
        buf.extend_from_slice(&pack_string(&self.keywords));

        Ok(buf)
    }

    /// Check if this object is a folder.
    ///
    /// Returns true if the format is Association or the association type is GenericFolder.
    #[must_use]
    pub fn is_folder(&self) -> bool {
        self.format == ObjectFormatCode::Association
            || self.association_type == AssociationType::GenericFolder
    }

    /// Check if this object is a file.
    ///
    /// Returns true if this is not a folder.
    #[must_use]
    pub fn is_file(&self) -> bool {
        !self.is_folder()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::pack::{pack_datetime, pack_string, pack_u16, pack_u32, DateTime};

    // --- ObjectInfo Tests ---

    fn build_file_object_info_bytes() -> Vec<u8> {
        let mut buf = Vec::new();

        // StorageID: 0x00010001
        buf.extend_from_slice(&pack_u32(0x00010001));
        // ObjectFormat: JPEG (0x3801)
        buf.extend_from_slice(&pack_u16(0x3801));
        // ProtectionStatus: None (0)
        buf.extend_from_slice(&pack_u16(0));
        // ObjectCompressedSize: 1024 bytes
        buf.extend_from_slice(&pack_u32(1024));
        // ThumbFormat: JPEG (0x3801)
        buf.extend_from_slice(&pack_u16(0x3801));
        // ThumbCompressedSize: 512
        buf.extend_from_slice(&pack_u32(512));
        // ThumbPixWidth: 160
        buf.extend_from_slice(&pack_u32(160));
        // ThumbPixHeight: 120
        buf.extend_from_slice(&pack_u32(120));
        // ImagePixWidth: 1920
        buf.extend_from_slice(&pack_u32(1920));
        // ImagePixHeight: 1080
        buf.extend_from_slice(&pack_u32(1080));
        // ImageBitDepth: 24
        buf.extend_from_slice(&pack_u32(24));
        // ParentObject: 0x00000005
        buf.extend_from_slice(&pack_u32(5));
        // AssociationType: None (0)
        buf.extend_from_slice(&pack_u16(0));
        // AssociationDesc: 0
        buf.extend_from_slice(&pack_u32(0));
        // SequenceNumber: 1
        buf.extend_from_slice(&pack_u32(1));
        // Filename: "photo.jpg"
        buf.extend_from_slice(&pack_string("photo.jpg"));
        // DateCreated: "20240315T143022"
        buf.extend_from_slice(
            &pack_datetime(&DateTime {
                year: 2024,
                month: 3,
                day: 15,
                hour: 14,
                minute: 30,
                second: 22,
                offset: None,
            })
            .unwrap(),
        );
        // DateModified: "20240316T090000"
        buf.extend_from_slice(
            &pack_datetime(&DateTime {
                year: 2024,
                month: 3,
                day: 16,
                hour: 9,
                minute: 0,
                second: 0,
                offset: None,
            })
            .unwrap(),
        );
        // Keywords: ""
        buf.push(0x00);

        buf
    }

    #[test]
    fn object_info_parse_file() {
        let buf = build_file_object_info_bytes();
        let info = ObjectInfo::from_bytes(&buf).unwrap();

        assert_eq!(info.storage_id, StorageId(0x00010001));
        assert_eq!(info.format, ObjectFormatCode::Jpeg);
        assert_eq!(info.protection_status, ProtectionStatus::None);
        assert_eq!(info.size, 1024);
        assert_eq!(info.thumb_format, ObjectFormatCode::Jpeg);
        assert_eq!(info.thumb_size, 512);
        assert_eq!(info.thumb_width, 160);
        assert_eq!(info.thumb_height, 120);
        assert_eq!(info.image_width, 1920);
        assert_eq!(info.image_height, 1080);
        assert_eq!(info.image_bit_depth, 24);
        assert_eq!(info.parent, ObjectHandle(5));
        assert_eq!(info.association_type, AssociationType::None);
        assert_eq!(info.association_desc, 0);
        assert_eq!(info.sequence_number, 1);
        assert_eq!(info.filename, "photo.jpg");
        assert!(info.created.is_some());
        let created = info.created.unwrap();
        assert_eq!(created.year, 2024);
        assert_eq!(created.month, 3);
        assert_eq!(created.day, 15);
        assert!(info.modified.is_some());
        assert_eq!(info.keywords, "");

        assert!(info.is_file());
        assert!(!info.is_folder());
    }

    #[test]
    fn object_info_parse_with_unparseable_datetime() {
        // Panasonic Lumix DMC-TZ61 (issue #12) reports "20480000T000000"
        // (month 0, day 0) as its "no date" sentinel in DateCreated and
        // DateModified. The whole ObjectInfo must still parse; the dates
        // become None and the fields after them (Keywords) stay aligned.
        let mut buf = Vec::new();
        buf.extend_from_slice(&pack_u32(0x00010001)); // StorageID
        buf.extend_from_slice(&pack_u16(0x3801)); // ObjectFormat: JPEG
        buf.extend_from_slice(&pack_u16(0)); // ProtectionStatus
        buf.extend_from_slice(&pack_u32(5_000_000)); // ObjectCompressedSize
        buf.extend_from_slice(&pack_u16(0x3801)); // ThumbFormat
        buf.extend_from_slice(&pack_u32(512)); // ThumbCompressedSize
        buf.extend_from_slice(&pack_u32(160)); // ThumbPixWidth
        buf.extend_from_slice(&pack_u32(120)); // ThumbPixHeight
        buf.extend_from_slice(&pack_u32(4896)); // ImagePixWidth
        buf.extend_from_slice(&pack_u32(3672)); // ImagePixHeight
        buf.extend_from_slice(&pack_u32(24)); // ImageBitDepth
        buf.extend_from_slice(&pack_u32(5)); // ParentObject
        buf.extend_from_slice(&pack_u16(0)); // AssociationType
        buf.extend_from_slice(&pack_u32(0)); // AssociationDesc
        buf.extend_from_slice(&pack_u32(1)); // SequenceNumber
        buf.extend_from_slice(&pack_string("P1040001.JPG")); // Filename
        buf.extend_from_slice(&pack_string("20480000T000000")); // DateCreated (sentinel)
        buf.extend_from_slice(&pack_string("20480000T000000")); // DateModified (sentinel)
        buf.extend_from_slice(&pack_string("kw")); // Keywords

        let info = ObjectInfo::from_bytes(&buf).unwrap();
        assert_eq!(info.filename, "P1040001.JPG");
        assert_eq!(info.created, None);
        assert_eq!(info.modified, None);
        assert_eq!(info.keywords, "kw");
        assert!(info.is_file());
    }

    fn build_folder_object_info_bytes() -> Vec<u8> {
        let mut buf = Vec::new();

        // StorageID: 0x00010001
        buf.extend_from_slice(&pack_u32(0x00010001));
        // ObjectFormat: Association (0x3001)
        buf.extend_from_slice(&pack_u16(0x3001));
        // ProtectionStatus: None (0)
        buf.extend_from_slice(&pack_u16(0));
        // ObjectCompressedSize: 0
        buf.extend_from_slice(&pack_u32(0));
        // ThumbFormat: Undefined (0x3000)
        buf.extend_from_slice(&pack_u16(0x3000));
        // ThumbCompressedSize: 0
        buf.extend_from_slice(&pack_u32(0));
        // ThumbPixWidth: 0
        buf.extend_from_slice(&pack_u32(0));
        // ThumbPixHeight: 0
        buf.extend_from_slice(&pack_u32(0));
        // ImagePixWidth: 0
        buf.extend_from_slice(&pack_u32(0));
        // ImagePixHeight: 0
        buf.extend_from_slice(&pack_u32(0));
        // ImageBitDepth: 0
        buf.extend_from_slice(&pack_u32(0));
        // ParentObject: ROOT (0)
        buf.extend_from_slice(&pack_u32(0));
        // AssociationType: GenericFolder (1)
        buf.extend_from_slice(&pack_u16(1));
        // AssociationDesc: 0
        buf.extend_from_slice(&pack_u32(0));
        // SequenceNumber: 0
        buf.extend_from_slice(&pack_u32(0));
        // Filename: "DCIM"
        buf.extend_from_slice(&pack_string("DCIM"));
        // DateCreated: empty
        buf.push(0x00);
        // DateModified: empty
        buf.push(0x00);
        // Keywords: ""
        buf.push(0x00);

        buf
    }

    #[test]
    fn object_info_parse_folder() {
        let buf = build_folder_object_info_bytes();
        let info = ObjectInfo::from_bytes(&buf).unwrap();

        assert_eq!(info.format, ObjectFormatCode::Association);
        assert_eq!(info.association_type, AssociationType::GenericFolder);
        assert_eq!(info.filename, "DCIM");
        assert_eq!(info.parent, ObjectHandle::ROOT);
        assert!(info.created.is_none());
        assert!(info.modified.is_none());

        assert!(info.is_folder());
        assert!(!info.is_file());
    }

    #[test]
    fn object_info_to_bytes_roundtrip() {
        let original = ObjectInfo {
            handle: ObjectHandle(42),
            storage_id: StorageId(0x00010001),
            format: ObjectFormatCode::Jpeg,
            protection_status: ProtectionStatus::None,
            size: 2048,
            thumb_format: ObjectFormatCode::Jpeg,
            thumb_size: 256,
            thumb_width: 80,
            thumb_height: 60,
            image_width: 800,
            image_height: 600,
            image_bit_depth: 24,
            parent: ObjectHandle(10),
            association_type: AssociationType::None,
            association_desc: 0,
            sequence_number: 5,
            filename: "test.jpg".to_string(),
            created: Some(DateTime {
                year: 2024,
                month: 6,
                day: 15,
                hour: 10,
                minute: 30,
                second: 0,
                offset: None,
            }),
            modified: Some(DateTime {
                year: 2024,
                month: 6,
                day: 16,
                hour: 11,
                minute: 45,
                second: 30,
                offset: None,
            }),
            keywords: "test,photo".to_string(),
        };

        let bytes = original.to_bytes().unwrap();
        let parsed = ObjectInfo::from_bytes(&bytes).unwrap();

        assert_eq!(parsed.storage_id, original.storage_id);
        assert_eq!(parsed.format, original.format);
        assert_eq!(parsed.protection_status, original.protection_status);
        assert_eq!(parsed.size, original.size);
        assert_eq!(parsed.thumb_format, original.thumb_format);
        assert_eq!(parsed.thumb_size, original.thumb_size);
        assert_eq!(parsed.thumb_width, original.thumb_width);
        assert_eq!(parsed.thumb_height, original.thumb_height);
        assert_eq!(parsed.image_width, original.image_width);
        assert_eq!(parsed.image_height, original.image_height);
        assert_eq!(parsed.image_bit_depth, original.image_bit_depth);
        assert_eq!(parsed.parent, original.parent);
        assert_eq!(parsed.association_type, original.association_type);
        assert_eq!(parsed.association_desc, original.association_desc);
        assert_eq!(parsed.sequence_number, original.sequence_number);
        assert_eq!(parsed.filename, original.filename);
        assert_eq!(parsed.created, original.created);
        assert_eq!(parsed.modified, original.modified);
        assert_eq!(parsed.keywords, original.keywords);
    }

    #[test]
    fn object_info_to_bytes_large_size() {
        let info = ObjectInfo {
            size: 5_000_000_000, // 5GB, larger than u32::MAX
            ..Default::default()
        };

        let bytes = info.to_bytes().unwrap();
        let parsed = ObjectInfo::from_bytes(&bytes).unwrap();

        // Should be capped at u32::MAX when serializing
        assert_eq!(parsed.size, u32::MAX as u64);
    }

    #[test]
    fn object_info_is_folder_by_format() {
        let info = ObjectInfo {
            format: ObjectFormatCode::Association,
            association_type: AssociationType::None,
            ..Default::default()
        };
        assert!(info.is_folder());
    }

    #[test]
    fn object_info_is_folder_by_association() {
        let info = ObjectInfo {
            format: ObjectFormatCode::Undefined,
            association_type: AssociationType::GenericFolder,
            ..Default::default()
        };
        assert!(info.is_folder());
    }

    #[test]
    fn object_info_is_file() {
        let info = ObjectInfo {
            format: ObjectFormatCode::Jpeg,
            association_type: AssociationType::None,
            ..Default::default()
        };
        assert!(info.is_file());
        assert!(!info.is_folder());
    }

    #[test]
    fn object_info_parse_insufficient_bytes() {
        let buf = vec![0x00; 10]; // Not enough bytes
        assert!(ObjectInfo::from_bytes(&buf).is_err());
    }

    #[test]
    fn object_info_default() {
        let info = ObjectInfo::default();
        assert_eq!(info.storage_id, StorageId::default());
        assert_eq!(info.format, ObjectFormatCode::Undefined);
        assert_eq!(info.protection_status, ProtectionStatus::None);
        assert_eq!(info.size, 0);
        assert_eq!(info.filename, "");
        assert!(info.created.is_none());
        assert!(info.modified.is_none());
    }

    // Fuzz tests using shared macros
    crate::fuzz_bytes!(fuzz_object_info, ObjectInfo, 200);

    #[test]
    fn object_info_minimum_valid() {
        // ObjectInfo has many fixed fields before strings
        // StorageID(4) + Format(2) + Protection(2) + Size(4) + ThumbFormat(2) + ThumbSize(4) +
        // ThumbW(4) + ThumbH(4) + ImgW(4) + ImgH(4) + BitDepth(4) + Parent(4) + AssocType(2) +
        // AssocDesc(4) + SeqNum(4) = 52 bytes + 4 strings
        assert!(ObjectInfo::from_bytes(&[]).is_err());
        assert!(ObjectInfo::from_bytes(&[0; 51]).is_err());
        assert!(ObjectInfo::from_bytes(&[0; 52]).is_err()); // Still need string data
    }

    #[test]
    fn object_info_size_u32_max() {
        // When size is u32::MAX (0xFFFFFFFF), it indicates >4GB file
        let mut buf = build_file_object_info_bytes();
        // Replace size field (bytes 8-11, after StorageID(4) + Format(2) + Protection(2))
        buf[8] = 0xFF;
        buf[9] = 0xFF;
        buf[10] = 0xFF;
        buf[11] = 0xFF;

        let info = ObjectInfo::from_bytes(&buf).unwrap();
        assert_eq!(info.size, u32::MAX as u64);
    }
}
