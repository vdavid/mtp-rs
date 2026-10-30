//! MTP/PTP protocol operation, response, event, and format codes.
//!
//! This module defines the standard codes used in MTP/PTP communication:
//! - [`OperationCode`]: Commands sent to the device
//! - [`ResponseCode`]: Status codes returned by the device
//! - [`EventCode`]: Asynchronous events from the device
//! - [`ObjectFormatCode`]: File format identifiers

use num_enum::{FromPrimitive, IntoPrimitive};

/// PTP operation codes (commands sent to device).
///
/// These codes identify the operation being requested in a PTP command container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromPrimitive, IntoPrimitive)]
#[repr(u16)]
#[non_exhaustive]
pub enum OperationCode {
    /// Get device information (capabilities, manufacturer, etc.).
    GetDeviceInfo = 0x1001,
    /// Open a session with the device.
    OpenSession = 0x1002,
    /// Close the current session.
    CloseSession = 0x1003,
    /// Get list of storage IDs.
    GetStorageIds = 0x1004,
    /// Get information about a storage.
    GetStorageInfo = 0x1005,
    /// Get the number of objects in a storage/folder.
    GetNumObjects = 0x1006,
    /// Get list of object handles.
    GetObjectHandles = 0x1007,
    /// Get information about an object.
    GetObjectInfo = 0x1008,
    /// Download an object's data.
    GetObject = 0x1009,
    /// Get thumbnail for an object.
    GetThumb = 0x100A,
    /// Delete an object.
    DeleteObject = 0x100B,
    /// Send object metadata (before sending object data).
    SendObjectInfo = 0x100C,
    /// Send object data (after SendObjectInfo).
    SendObject = 0x100D,
    /// Initiate image capture on a camera.
    InitiateCapture = 0x100E,
    /// Format (erase) a storage.
    FormatStore = 0x100F,
    /// Reset the device to its default state, closing all sessions.
    ResetDevice = 0x1010,
    /// Run a device self-test.
    SelfTest = 0x1011,
    /// Set the write/delete protection status of an object.
    SetObjectProtection = 0x1012,
    /// Power down the device.
    PowerDown = 0x1013,
    /// Get device property descriptor.
    GetDevicePropDesc = 0x1014,
    /// Get current device property value.
    GetDevicePropValue = 0x1015,
    /// Set device property value.
    SetDevicePropValue = 0x1016,
    /// Reset device property to default value.
    ResetDevicePropValue = 0x1017,
    /// Terminate an open capture session.
    TerminateOpenCapture = 0x1018,
    /// Move an object to a different location.
    MoveObject = 0x1019,
    /// Copy an object.
    CopyObject = 0x101A,
    /// Get partial object data (range request). Offset is u32, so capped at 4 GB.
    GetPartialObject = 0x101B,
    /// Initiate an open-ended capture session.
    InitiateOpenCapture = 0x101C,
    /// Get the object properties the device supports for a format (MTP extension).
    GetObjectPropsSupported = 0x9801,
    /// Get an object property descriptor (MTP extension).
    GetObjectPropDesc = 0x9802,
    /// Get the value of an object property (MTP extension).
    GetObjectPropValue = 0x9803,
    /// Set the value of an object property (MTP extension).
    SetObjectPropValue = 0x9804,
    /// Get a list of object properties in one call (MTP extension).
    GetObjectPropList = 0x9805,
    /// Set a list of object properties in one call (MTP extension).
    SetObjectPropList = 0x9806,
    /// Get the references (associations) of an object (MTP extension).
    GetObjectReferences = 0x9810,
    /// Set the references (associations) of an object (MTP extension).
    SetObjectReferences = 0x9811,
    /// Get partial object data with 64-bit offset (Android/MTP extension).
    /// Supports files larger than 4 GB. Parameters: handle, offset_lo, offset_hi, max_bytes.
    GetPartialObject64 = 0x95C1,
    /// Unknown or vendor-specific operation code.
    #[num_enum(catch_all)]
    Unknown(u16),
}

/// PTP response codes (status returned by device).
///
/// These codes indicate the result of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromPrimitive, IntoPrimitive)]
#[repr(u16)]
#[non_exhaustive]
pub enum ResponseCode {
    /// Operation completed successfully.
    Ok = 0x2001,
    /// General unspecified error.
    GeneralError = 0x2002,
    /// Session is not open.
    SessionNotOpen = 0x2003,
    /// Invalid transaction ID.
    InvalidTransactionId = 0x2004,
    /// Operation is not supported.
    OperationNotSupported = 0x2005,
    /// Parameter is not supported.
    ParameterNotSupported = 0x2006,
    /// Transfer was incomplete.
    IncompleteTransfer = 0x2007,
    /// Invalid storage ID.
    InvalidStorageId = 0x2008,
    /// Invalid object handle.
    InvalidObjectHandle = 0x2009,
    /// Device property not supported.
    DevicePropNotSupported = 0x200A,
    /// Invalid object format code.
    InvalidObjectFormatCode = 0x200B,
    /// Storage is full.
    StoreFull = 0x200C,
    /// Object is write-protected.
    ObjectWriteProtected = 0x200D,
    /// Storage is read-only.
    StoreReadOnly = 0x200E,
    /// Access denied.
    AccessDenied = 0x200F,
    /// Object has no thumbnail.
    NoThumbnailPresent = 0x2010,
    /// Device is busy.
    DeviceBusy = 0x2019,
    /// Invalid parent object.
    InvalidParentObject = 0x201A,
    /// Invalid device property format.
    InvalidDevicePropFormat = 0x201B,
    /// Invalid device property value.
    InvalidDevicePropValue = 0x201C,
    /// Invalid parameter value.
    InvalidParameter = 0x201D,
    /// Session is already open.
    SessionAlreadyOpen = 0x201E,
    /// Transaction was cancelled.
    TransactionCancelled = 0x201F,
    /// Object is too large for the storage.
    ObjectTooLarge = 0xA809,
    /// Unknown or vendor-specific response code.
    #[num_enum(catch_all)]
    Unknown(u16),
}

/// PTP event codes (asynchronous notifications from device).
///
/// These codes identify events that the device sends asynchronously.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromPrimitive, IntoPrimitive)]
#[repr(u16)]
#[non_exhaustive]
pub enum EventCode {
    /// Cancel an in-progress transaction.
    CancelTransaction = 0x4001,
    /// A new object was added.
    ObjectAdded = 0x4002,
    /// An object was removed.
    ObjectRemoved = 0x4003,
    /// A new storage was added.
    StoreAdded = 0x4004,
    /// A storage was removed.
    StoreRemoved = 0x4005,
    /// A device property changed.
    DevicePropChanged = 0x4006,
    /// Object information changed.
    ObjectInfoChanged = 0x4007,
    /// Device information changed.
    DeviceInfoChanged = 0x4008,
    /// Storage information changed.
    StorageInfoChanged = 0x400C,
    /// Capture operation completed.
    CaptureComplete = 0x400D,
    /// Unknown or vendor-specific event code.
    #[num_enum(catch_all)]
    Unknown(u16),
}

/// PTP/MTP object format codes.
///
/// These codes identify the format/type of objects stored on the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromPrimitive, IntoPrimitive)]
#[repr(u16)]
#[non_exhaustive]
pub enum ObjectFormatCode {
    /// Undefined/unknown format.
    Undefined = 0x3000,
    /// Association (folder/directory).
    Association = 0x3001,
    /// Script file.
    Script = 0x3002,
    /// Executable file.
    Executable = 0x3003,
    /// Plain text file.
    Text = 0x3004,
    /// HTML file.
    Html = 0x3005,
    /// DPOF (Digital Print Order Format).
    Dpof = 0x3006,
    /// AIFF audio.
    Aiff = 0x3007,
    /// WAV audio.
    Wav = 0x3008,
    /// MP3 audio.
    Mp3 = 0x3009,
    /// AVI video.
    Avi = 0x300A,
    /// MPEG video.
    Mpeg = 0x300B,
    /// ASF (Advanced Systems Format).
    Asf = 0x300C,
    /// JPEG image.
    Jpeg = 0x3801,
    /// TIFF image.
    Tiff = 0x3804,
    /// GIF image.
    Gif = 0x3807,
    /// BMP image.
    Bmp = 0x3808,
    /// PICT image.
    Pict = 0x380A,
    /// PNG image.
    Png = 0x380B,
    /// WMA audio.
    WmaAudio = 0xB901,
    /// OGG audio.
    OggAudio = 0xB902,
    /// AAC audio.
    AacAudio = 0xB903,
    /// FLAC audio.
    FlacAudio = 0xB906,
    /// WMV video.
    WmvVideo = 0xB981,
    /// MP4 container.
    Mp4Container = 0xB982,
    /// M4A audio.
    M4aAudio = 0xB984,
    /// Unknown or vendor-specific format code.
    #[num_enum(catch_all)]
    Unknown(u16),
}

impl ObjectFormatCode {
    /// Detect object format from file extension (case insensitive).
    ///
    /// Returns `Undefined` for unrecognized extensions.
    #[must_use]
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            // Text and documents
            "txt" => ObjectFormatCode::Text,
            "html" | "htm" => ObjectFormatCode::Html,
            "dpof" => ObjectFormatCode::Dpof,

            // Audio formats
            "aiff" | "aif" => ObjectFormatCode::Aiff,
            "wav" => ObjectFormatCode::Wav,
            "mp3" => ObjectFormatCode::Mp3,
            "wma" => ObjectFormatCode::WmaAudio,
            "ogg" | "oga" => ObjectFormatCode::OggAudio,
            "aac" => ObjectFormatCode::AacAudio,
            "flac" => ObjectFormatCode::FlacAudio,
            "m4a" => ObjectFormatCode::M4aAudio,

            // Video formats
            "avi" => ObjectFormatCode::Avi,
            "mpg" | "mpeg" => ObjectFormatCode::Mpeg,
            "asf" => ObjectFormatCode::Asf,
            "wmv" => ObjectFormatCode::WmvVideo,
            "mp4" | "m4v" => ObjectFormatCode::Mp4Container,

            // Image formats
            "jpg" | "jpeg" => ObjectFormatCode::Jpeg,
            "tif" | "tiff" => ObjectFormatCode::Tiff,
            "gif" => ObjectFormatCode::Gif,
            "bmp" => ObjectFormatCode::Bmp,
            "pict" | "pct" => ObjectFormatCode::Pict,
            "png" => ObjectFormatCode::Png,

            // Executables and scripts
            "exe" | "dll" | "bin" => ObjectFormatCode::Executable,
            "sh" | "bat" | "cmd" | "ps1" => ObjectFormatCode::Script,

            _ => ObjectFormatCode::Undefined,
        }
    }

    /// Check if this format is an audio format.
    #[must_use]
    pub fn is_audio(&self) -> bool {
        matches!(
            self,
            ObjectFormatCode::Aiff
                | ObjectFormatCode::Wav
                | ObjectFormatCode::Mp3
                | ObjectFormatCode::WmaAudio
                | ObjectFormatCode::OggAudio
                | ObjectFormatCode::AacAudio
                | ObjectFormatCode::FlacAudio
                | ObjectFormatCode::M4aAudio
        )
    }

    /// Check if this format is a video format.
    #[must_use]
    pub fn is_video(&self) -> bool {
        matches!(
            self,
            ObjectFormatCode::Avi
                | ObjectFormatCode::Mpeg
                | ObjectFormatCode::Asf
                | ObjectFormatCode::WmvVideo
                | ObjectFormatCode::Mp4Container
        )
    }

    /// Check if this format is an image format.
    #[must_use]
    pub fn is_image(&self) -> bool {
        matches!(
            self,
            ObjectFormatCode::Jpeg
                | ObjectFormatCode::Tiff
                | ObjectFormatCode::Gif
                | ObjectFormatCode::Bmp
                | ObjectFormatCode::Pict
                | ObjectFormatCode::Png
        )
    }
}

// Manual impl required because #[default] attribute conflicts with num_enum's #[num_enum(catch_all)]
#[allow(clippy::derivable_impls)]
impl Default for ObjectFormatCode {
    fn default() -> Self {
        ObjectFormatCode::Undefined
    }
}

/// MTP object property codes.
///
/// These codes identify object properties that can be get/set via MTP operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromPrimitive, IntoPrimitive)]
#[repr(u16)]
#[non_exhaustive]
pub enum ObjectPropertyCode {
    /// Storage ID containing the object.
    StorageId = 0xDC01,
    /// Object format code.
    ObjectFormat = 0xDC02,
    /// Protection status (read-only, etc.).
    ProtectionStatus = 0xDC03,
    /// Object size in bytes.
    ObjectSize = 0xDC04,
    /// Object filename (key property for renaming).
    ObjectFileName = 0xDC07,
    /// Date the object was created.
    DateCreated = 0xDC08,
    /// Date the object was last modified.
    DateModified = 0xDC09,
    /// Parent object handle.
    ParentObject = 0xDC0B,
    /// Display name of the object.
    Name = 0xDC44,
    /// Unknown or vendor-specific property code.
    #[num_enum(catch_all)]
    Unknown(u16),
}

/// PTP property data type codes.
///
/// These codes identify the data type of property values in property descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromPrimitive, IntoPrimitive)]
#[repr(u16)]
#[non_exhaustive]
pub enum PropertyDataType {
    /// Undefined type (0x0000).
    Undefined = 0x0000,
    /// Signed 8-bit integer (0x0001).
    Int8 = 0x0001,
    /// Unsigned 8-bit integer (0x0002).
    Uint8 = 0x0002,
    /// Signed 16-bit integer (0x0003).
    Int16 = 0x0003,
    /// Unsigned 16-bit integer (0x0004).
    Uint16 = 0x0004,
    /// Signed 32-bit integer (0x0005).
    Int32 = 0x0005,
    /// Unsigned 32-bit integer (0x0006).
    Uint32 = 0x0006,
    /// Signed 64-bit integer (0x0007).
    Int64 = 0x0007,
    /// Unsigned 64-bit integer (0x0008).
    Uint64 = 0x0008,
    /// Signed 128-bit integer (0x0009, rarely used).
    Int128 = 0x0009,
    /// Unsigned 128-bit integer (0x000A, rarely used).
    Uint128 = 0x000A,
    /// Unknown type code.
    #[num_enum(catch_all)]
    Unknown(u16),
    /// UTF-16LE string (0xFFFF).
    String = 0xFFFF,
}

impl PropertyDataType {
    /// Returns the byte size of this data type.
    ///
    /// Returns `None` for variable-length types (String) and unsupported types
    /// (Undefined, Int128, Uint128, Unknown).
    #[must_use]
    pub fn byte_size(&self) -> Option<usize> {
        match self {
            PropertyDataType::Int8 | PropertyDataType::Uint8 => Some(1),
            PropertyDataType::Int16 | PropertyDataType::Uint16 => Some(2),
            PropertyDataType::Int32 | PropertyDataType::Uint32 => Some(4),
            PropertyDataType::Int64 | PropertyDataType::Uint64 => Some(8),
            PropertyDataType::Int128 | PropertyDataType::Uint128 => Some(16),
            PropertyDataType::String
            | PropertyDataType::Undefined
            | PropertyDataType::Unknown(_) => None,
        }
    }
}

/// Standard PTP device property codes (0x5000 range).
///
/// These codes identify device-level properties that can be read or modified
/// using the GetDevicePropDesc, GetDevicePropValue, SetDevicePropValue, and
/// ResetDevicePropValue operations.
///
/// Device properties are primarily used with digital cameras for settings
/// like ISO, aperture, shutter speed, etc. Most Android MTP devices do not
/// support device properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, FromPrimitive, IntoPrimitive)]
#[repr(u16)]
#[non_exhaustive]
pub enum DevicePropertyCode {
    /// Undefined property.
    Undefined = 0x5000,
    /// Battery level (UINT8, 0-100 percent).
    BatteryLevel = 0x5001,
    /// Device functional mode (UINT16).
    FunctionalMode = 0x5002,
    /// Image size setting (String, e.g., "1920x1080").
    ImageSize = 0x5003,
    /// Compression setting (UINT8).
    CompressionSetting = 0x5004,
    /// White balance (UINT16).
    WhiteBalance = 0x5005,
    /// RGB gain (String).
    RgbGain = 0x5006,
    /// F-Number/Aperture (UINT16, value/100 = f-stop).
    FNumber = 0x5007,
    /// Focal length (UINT32, units of 0.01mm).
    FocalLength = 0x5008,
    /// Focus distance (UINT16, mm).
    FocusDistance = 0x5009,
    /// Focus mode (UINT16).
    FocusMode = 0x500A,
    /// Exposure metering mode (UINT16).
    ExposureMeteringMode = 0x500B,
    /// Flash mode (UINT16).
    FlashMode = 0x500C,
    /// Exposure time/shutter speed (UINT32, units of 0.0001s).
    ExposureTime = 0x500D,
    /// Exposure program mode (UINT16).
    ExposureProgramMode = 0x500E,
    /// Exposure index/ISO (UINT16).
    ExposureIndex = 0x500F,
    /// Exposure bias compensation (INT16, units of 0.001 EV).
    ExposureBiasCompensation = 0x5010,
    /// Date and time (String, "YYYYMMDDThhmmss").
    DateTime = 0x5011,
    /// Capture delay (UINT32, ms).
    CaptureDelay = 0x5012,
    /// Still capture mode (UINT16).
    StillCaptureMode = 0x5013,
    /// Contrast (UINT8).
    Contrast = 0x5014,
    /// Sharpness (UINT8).
    Sharpness = 0x5015,
    /// Digital zoom (UINT8).
    DigitalZoom = 0x5016,
    /// Effect mode (UINT16).
    EffectMode = 0x5017,
    /// Burst number (UINT16).
    BurstNumber = 0x5018,
    /// Burst interval (UINT16, ms).
    BurstInterval = 0x5019,
    /// Timelapse number (UINT16).
    TimelapseNumber = 0x501A,
    /// Timelapse interval (UINT32, ms).
    TimelapseInterval = 0x501B,
    /// Focus metering mode (UINT16).
    FocusMeteringMode = 0x501C,
    /// Upload URL (String).
    UploadUrl = 0x501D,
    /// Artist name (String).
    Artist = 0x501E,
    /// Copyright info (String).
    CopyrightInfo = 0x501F,
    /// Unknown/vendor-specific property code.
    #[num_enum(catch_all)]
    Unknown(u16),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_code_standard_ptp_and_mtp_codes() {
        // The exact codes the Panasonic Lumix DMC-TZ61 advertised as
        // Unknown(...) in issue #12, plus the rest of the standard sets.
        let cases: [(u16, OperationCode); 13] = [
            (0x100F, OperationCode::FormatStore),
            (0x1010, OperationCode::ResetDevice),
            (0x1011, OperationCode::SelfTest),
            (0x1012, OperationCode::SetObjectProtection),
            (0x1013, OperationCode::PowerDown),
            (0x1018, OperationCode::TerminateOpenCapture),
            (0x101C, OperationCode::InitiateOpenCapture),
            (0x9801, OperationCode::GetObjectPropsSupported),
            (0x9802, OperationCode::GetObjectPropDesc),
            (0x9805, OperationCode::GetObjectPropList),
            (0x9806, OperationCode::SetObjectPropList),
            (0x9810, OperationCode::GetObjectReferences),
            (0x9811, OperationCode::SetObjectReferences),
        ];
        for (raw, expected) in cases {
            assert_eq!(OperationCode::from(raw), expected);
            assert_eq!(u16::from(expected), raw);
        }
    }

    #[test]
    fn from_extension_detection() {
        // Audio (representative samples)
        assert_eq!(
            ObjectFormatCode::from_extension("mp3"),
            ObjectFormatCode::Mp3
        );
        assert_eq!(
            ObjectFormatCode::from_extension("flac"),
            ObjectFormatCode::FlacAudio
        );
        assert_eq!(
            ObjectFormatCode::from_extension("aif"),
            ObjectFormatCode::Aiff
        ); // alternate ext

        // Video
        assert_eq!(
            ObjectFormatCode::from_extension("mp4"),
            ObjectFormatCode::Mp4Container
        );
        assert_eq!(
            ObjectFormatCode::from_extension("avi"),
            ObjectFormatCode::Avi
        );
        assert_eq!(
            ObjectFormatCode::from_extension("mpg"),
            ObjectFormatCode::Mpeg
        ); // alternate ext

        // Image
        assert_eq!(
            ObjectFormatCode::from_extension("jpg"),
            ObjectFormatCode::Jpeg
        );
        assert_eq!(
            ObjectFormatCode::from_extension("png"),
            ObjectFormatCode::Png
        );
        assert_eq!(
            ObjectFormatCode::from_extension("tif"),
            ObjectFormatCode::Tiff
        ); // alternate ext

        // Text/Documents
        assert_eq!(
            ObjectFormatCode::from_extension("txt"),
            ObjectFormatCode::Text
        );
        assert_eq!(
            ObjectFormatCode::from_extension("htm"),
            ObjectFormatCode::Html
        ); // alternate ext

        // Executables/Scripts
        assert_eq!(
            ObjectFormatCode::from_extension("exe"),
            ObjectFormatCode::Executable
        );
        assert_eq!(
            ObjectFormatCode::from_extension("sh"),
            ObjectFormatCode::Script
        );

        // Case insensitivity (one example suffices since .to_lowercase() is used)
        assert_eq!(
            ObjectFormatCode::from_extension("MP3"),
            ObjectFormatCode::Mp3
        );

        // Unknown extensions
        assert_eq!(
            ObjectFormatCode::from_extension("xyz"),
            ObjectFormatCode::Undefined
        );
        assert_eq!(
            ObjectFormatCode::from_extension(""),
            ObjectFormatCode::Undefined
        );
    }

    // ==================== Format Category Tests ====================

    #[test]
    fn is_audio() {
        assert!(ObjectFormatCode::Mp3.is_audio());
        assert!(ObjectFormatCode::FlacAudio.is_audio());
        assert!(!ObjectFormatCode::Jpeg.is_audio());
        assert!(!ObjectFormatCode::Mp4Container.is_audio());
    }

    #[test]
    fn is_video() {
        assert!(ObjectFormatCode::Mp4Container.is_video());
        assert!(ObjectFormatCode::Avi.is_video());
        assert!(!ObjectFormatCode::Mp3.is_video());
        assert!(!ObjectFormatCode::Jpeg.is_video());
    }

    #[test]
    fn is_image() {
        assert!(ObjectFormatCode::Jpeg.is_image());
        assert!(ObjectFormatCode::Png.is_image());
        assert!(!ObjectFormatCode::Mp3.is_image());
        assert!(!ObjectFormatCode::Mp4Container.is_image());
    }

    #[test]
    fn format_categories_are_mutually_exclusive() {
        let all_formats = [
            ObjectFormatCode::Undefined,
            ObjectFormatCode::Association,
            ObjectFormatCode::Script,
            ObjectFormatCode::Executable,
            ObjectFormatCode::Text,
            ObjectFormatCode::Html,
            ObjectFormatCode::Dpof,
            ObjectFormatCode::Aiff,
            ObjectFormatCode::Wav,
            ObjectFormatCode::Mp3,
            ObjectFormatCode::Avi,
            ObjectFormatCode::Mpeg,
            ObjectFormatCode::Asf,
            ObjectFormatCode::Jpeg,
            ObjectFormatCode::Tiff,
            ObjectFormatCode::Gif,
            ObjectFormatCode::Bmp,
            ObjectFormatCode::Pict,
            ObjectFormatCode::Png,
            ObjectFormatCode::WmaAudio,
            ObjectFormatCode::OggAudio,
            ObjectFormatCode::AacAudio,
            ObjectFormatCode::FlacAudio,
            ObjectFormatCode::WmvVideo,
            ObjectFormatCode::Mp4Container,
            ObjectFormatCode::M4aAudio,
        ];

        for format in all_formats {
            let categories = [format.is_audio(), format.is_video(), format.is_image()];
            let true_count = categories.iter().filter(|&&b| b).count();
            assert!(
                true_count <= 1,
                "{:?} belongs to multiple categories",
                format
            );
        }
    }

    // ==================== PropertyDataType Tests ====================

    #[test]
    fn property_data_type_byte_size() {
        // Fixed-size types
        assert_eq!(PropertyDataType::Int8.byte_size(), Some(1));
        assert_eq!(PropertyDataType::Uint8.byte_size(), Some(1));
        assert_eq!(PropertyDataType::Int16.byte_size(), Some(2));
        assert_eq!(PropertyDataType::Uint16.byte_size(), Some(2));
        assert_eq!(PropertyDataType::Int32.byte_size(), Some(4));
        assert_eq!(PropertyDataType::Uint32.byte_size(), Some(4));
        assert_eq!(PropertyDataType::Int64.byte_size(), Some(8));
        assert_eq!(PropertyDataType::Uint64.byte_size(), Some(8));
        assert_eq!(PropertyDataType::Int128.byte_size(), Some(16));
        assert_eq!(PropertyDataType::Uint128.byte_size(), Some(16));

        // Variable/undefined types
        assert_eq!(PropertyDataType::String.byte_size(), None);
        assert_eq!(PropertyDataType::Undefined.byte_size(), None);
        assert_eq!(PropertyDataType::Unknown(0x1234).byte_size(), None);
    }
}
