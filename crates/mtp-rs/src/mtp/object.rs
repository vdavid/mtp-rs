//! Object-related types for MTP.

use crate::mtp::{DateTime, ObjectFormat};
use crate::ptp::{AssociationType, ObjectFormatCode, ObjectInfo as PtpObjectInfo};

/// Information needed to create a new object.
#[derive(Debug, Clone)]
pub struct NewObjectInfo {
    /// Filename (max 254 characters, no /, \, or null bytes)
    pub filename: String,
    /// File size in bytes (must match actual data sent)
    pub size: u64,
    /// Object format (auto-detected from extension if None)
    pub format: Option<ObjectFormat>,
    /// Modification time
    pub modified: Option<DateTime>,
}

impl NewObjectInfo {
    /// Create info for a file. Format auto-detected from extension.
    #[must_use]
    pub fn file(filename: impl Into<String>, size: u64) -> Self {
        let filename = filename.into();
        let format = detect_format_from_filename(&filename);
        Self {
            filename,
            size,
            format: Some(format),
            modified: None,
        }
    }

    /// Create info for a folder.
    #[must_use]
    pub fn folder(name: impl Into<String>) -> Self {
        Self {
            filename: name.into(),
            size: 0,
            format: Some(ObjectFormat::ASSOCIATION),
            modified: None,
        }
    }

    /// Create info with explicit format.
    #[must_use]
    pub fn with_format(filename: impl Into<String>, size: u64, format: ObjectFormat) -> Self {
        Self {
            filename: filename.into(),
            size,
            format: Some(format),
            modified: None,
        }
    }

    /// Set modification time.
    #[must_use]
    pub fn with_modified(mut self, modified: DateTime) -> Self {
        self.modified = Some(modified);
        self
    }

    /// Convert to PTP ObjectInfo for sending.
    pub(crate) fn to_object_info(&self) -> PtpObjectInfo {
        let format = self.format.unwrap_or(ObjectFormat::UNDEFINED);
        let is_folder = format.is_association();

        PtpObjectInfo {
            format: ObjectFormatCode::from(format.code()),
            size: self.size,
            filename: self.filename.clone(),
            modified: self.modified.map(DateTime::to_ptp),
            association_type: if is_folder {
                AssociationType::GenericFolder
            } else {
                AssociationType::None
            },
            ..Default::default()
        }
    }
}

/// Detect format from filename extension.
fn detect_format_from_filename(filename: &str) -> ObjectFormat {
    if let Some(ext) = filename.rsplit('.').next() {
        ObjectFormat::from(ObjectFormatCode::from_extension(ext))
    } else {
        ObjectFormat::UNDEFINED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(code: ObjectFormatCode) -> ObjectFormat {
        ObjectFormat::from(code)
    }

    #[test]
    fn test_new_object_info_file() {
        let info = NewObjectInfo::file("test.mp3", 1000);
        assert_eq!(info.filename, "test.mp3");
        assert_eq!(info.size, 1000);
        assert_eq!(info.format, Some(fmt(ObjectFormatCode::Mp3)));
    }

    #[test]
    fn test_new_object_info_folder() {
        let info = NewObjectInfo::folder("Music");
        assert_eq!(info.filename, "Music");
        assert_eq!(info.size, 0);
        assert_eq!(info.format, Some(ObjectFormat::ASSOCIATION));
    }

    #[test]
    fn test_format_detection() {
        assert_eq!(
            detect_format_from_filename("song.mp3"),
            fmt(ObjectFormatCode::Mp3)
        );
        assert_eq!(
            detect_format_from_filename("photo.jpg"),
            fmt(ObjectFormatCode::Jpeg)
        );
        assert_eq!(
            detect_format_from_filename("video.mp4"),
            fmt(ObjectFormatCode::Mp4Container)
        );
        assert_eq!(
            detect_format_from_filename("unknown.xyz"),
            ObjectFormat::UNDEFINED
        );
    }

    #[test]
    fn test_with_format() {
        let info =
            NewObjectInfo::with_format("document.bin", 500, fmt(ObjectFormatCode::Executable));
        assert_eq!(info.filename, "document.bin");
        assert_eq!(info.size, 500);
        assert_eq!(info.format, Some(fmt(ObjectFormatCode::Executable)));
    }

    #[test]
    fn test_with_modified() {
        let dt = DateTime {
            year: 2024,
            month: 6,
            day: 15,
            hour: 10,
            minute: 30,
            second: 0,
            offset: None,
        };
        let info = NewObjectInfo::file("test.txt", 100).with_modified(dt);
        assert_eq!(info.modified, Some(dt));
    }

    #[test]
    fn test_to_object_info_file() {
        let info = NewObjectInfo::file("test.mp3", 1000);
        let ptp_info = info.to_object_info();

        assert_eq!(ptp_info.format, ObjectFormatCode::Mp3);
        assert_eq!(ptp_info.size, 1000);
        assert_eq!(ptp_info.filename, "test.mp3");
        assert_eq!(ptp_info.association_type, AssociationType::None);
    }

    #[test]
    fn test_to_object_info_folder() {
        let info = NewObjectInfo::folder("Music");
        let ptp_info = info.to_object_info();

        assert_eq!(ptp_info.format, ObjectFormatCode::Association);
        assert_eq!(ptp_info.size, 0);
        assert_eq!(ptp_info.filename, "Music");
        assert_eq!(ptp_info.association_type, AssociationType::GenericFolder);
    }

    #[test]
    fn test_format_detection_case_insensitive() {
        assert_eq!(
            detect_format_from_filename("SONG.MP3"),
            fmt(ObjectFormatCode::Mp3)
        );
        assert_eq!(
            detect_format_from_filename("Photo.JPG"),
            fmt(ObjectFormatCode::Jpeg)
        );
    }

    #[test]
    fn test_format_detection_no_extension() {
        assert_eq!(
            detect_format_from_filename("noextension"),
            ObjectFormat::UNDEFINED
        );
    }
}
