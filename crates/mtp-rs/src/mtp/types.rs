//! Backend-neutral types for the high-level [`crate::mtp`] API.
//!
//! These types are deliberately independent of any single backend's wire format. The PTP-over-USB
//! backend ([`crate::ptp`]) and the Windows WPD-over-COM backend both produce and consume them,
//! converting from their own representations at the boundary.
//!
//! Where a value originates in the PTP layer, a `From` impl (and a `pub(crate)` `to_ptp` helper for
//! the reverse) bridges the two so the `UsbBackend` converts only at its edge.

use crate::ptp::{
    AccessCapability, DateTime as PtpDateTime, DeviceInfo as PtpDeviceInfo,
    FilesystemType as PtpFs, ObjectFormatCode, ObjectHandle as PtpObjectHandle,
    ObjectInfo as PtpObjectInfo, OperationCode, StorageId as PtpStorageId,
    StorageInfo as PtpStorageInfo, StorageType as PtpStorageType,
};

/// Opaque handle for an object on a device.
///
/// The inner value is a backend-defined token, meaningful only within a single open device session
/// — **not** a stable, cross-session, or wire-level identifier. The USB/PTP backend uses the raw PTP
/// object handle; the WPD backend maps the token to an opaque WPD object-ID string. Treat it as
/// opaque: pass it back to the same open device, don't persist it across sessions, and don't derive
/// meaning from its numeric value. (Some devices, notably Android, re-key handles even within a
/// session — see [`crate::Error::StaleHandle`].)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ObjectHandle(pub u64);

impl ObjectHandle {
    /// The storage root (an object's parent is `ROOT` when it sits at the top of a storage).
    pub const ROOT: Self = ObjectHandle(0x0000_0000);
    /// Sentinel meaning "all objects" for recursive listing.
    pub const ALL: Self = ObjectHandle(0xFFFF_FFFF);

    /// Narrow to the PTP wire handle. Sound for the USB backend because its tokens originate as
    /// widened `u32` PTP handles; `ROOT`/`ALL` round-trip exactly.
    #[must_use]
    pub(crate) fn to_ptp(self) -> PtpObjectHandle {
        PtpObjectHandle(self.0 as u32)
    }
}

impl From<PtpObjectHandle> for ObjectHandle {
    fn from(h: PtpObjectHandle) -> Self {
        ObjectHandle(u64::from(h.0))
    }
}

/// Opaque identifier for a storage on a device.
///
/// Like [`ObjectHandle`], this is a backend-defined token. The USB/PTP backend uses the raw PTP
/// storage ID; the WPD backend derives a stable token from the WPD storage object-ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct StorageId(pub u64);

impl StorageId {
    /// Sentinel meaning "all storages".
    pub const ALL: Self = StorageId(0xFFFF_FFFF);

    /// Narrow to the PTP wire storage ID. See [`ObjectHandle::to_ptp`].
    #[must_use]
    pub(crate) fn to_ptp(self) -> PtpStorageId {
        PtpStorageId(self.0 as u32)
    }
}

impl From<PtpStorageId> for StorageId {
    fn from(s: PtpStorageId) -> Self {
        StorageId(u64::from(s.0))
    }
}

/// Object format, as the MTP format code the device reports.
///
/// Kept as the raw 16-bit MTP format code (WPD exposes the same codes), with category helpers so
/// callers don't switch on a backend-specific enum. Use [`ObjectFormat::is_association`] for the
/// folder marker, or [`ObjectInfo::is_folder`] for the full folder test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectFormat(pub u16);

impl ObjectFormat {
    /// Undefined / unknown format.
    pub const UNDEFINED: Self = ObjectFormat(0x3000);
    /// Association (a folder).
    pub const ASSOCIATION: Self = ObjectFormat(0x3001);

    /// The raw 16-bit MTP format code.
    #[must_use]
    pub fn code(self) -> u16 {
        self.0
    }

    /// Whether this is the association (folder) format.
    #[must_use]
    pub fn is_association(self) -> bool {
        self == Self::ASSOCIATION
    }

    /// Whether this is an audio format.
    #[must_use]
    pub fn is_audio(self) -> bool {
        ObjectFormatCode::from(self.0).is_audio()
    }

    /// Whether this is a video format.
    #[must_use]
    pub fn is_video(self) -> bool {
        ObjectFormatCode::from(self.0).is_video()
    }

    /// Whether this is an image format.
    #[must_use]
    pub fn is_image(self) -> bool {
        ObjectFormatCode::from(self.0).is_image()
    }
}

impl Default for ObjectFormat {
    fn default() -> Self {
        Self::UNDEFINED
    }
}

impl From<ObjectFormatCode> for ObjectFormat {
    fn from(c: ObjectFormatCode) -> Self {
        ObjectFormat(c.into())
    }
}

/// A calendar date and time as reported by a device (no timezone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateTime {
    /// Year (e.g. 2026).
    pub year: u16,
    /// Month, 1-12.
    pub month: u8,
    /// Day, 1-31.
    pub day: u8,
    /// Hour, 0-23.
    pub hour: u8,
    /// Minute, 0-59.
    pub minute: u8,
    /// Second, 0-59.
    pub second: u8,
}

impl From<PtpDateTime> for DateTime {
    fn from(d: PtpDateTime) -> Self {
        DateTime {
            year: d.year,
            month: d.month,
            day: d.day,
            hour: d.hour,
            minute: d.minute,
            second: d.second,
        }
    }
}

impl DateTime {
    /// Convert to the PTP wire datetime (used when packing an upload's `ObjectInfo`).
    #[must_use]
    pub(crate) fn to_ptp(self) -> PtpDateTime {
        PtpDateTime {
            year: self.year,
            month: self.month,
            day: self.day,
            hour: self.hour,
            minute: self.minute,
            second: self.second,
        }
    }
}

/// Type of storage medium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageType {
    /// Undefined storage type.
    #[default]
    Undefined,
    /// Fixed read-only memory (e.g. internal flash exposed read-only).
    FixedRom,
    /// Removable read-only memory.
    RemovableRom,
    /// Fixed read-write memory (e.g. internal storage).
    FixedRam,
    /// Removable read-write memory (e.g. an SD card).
    RemovableRam,
    /// A code this library doesn't model.
    Other,
}

impl From<PtpStorageType> for StorageType {
    fn from(t: PtpStorageType) -> Self {
        match t {
            PtpStorageType::Undefined => StorageType::Undefined,
            PtpStorageType::FixedRom => StorageType::FixedRom,
            PtpStorageType::RemovableRom => StorageType::RemovableRom,
            PtpStorageType::FixedRam => StorageType::FixedRam,
            PtpStorageType::RemovableRam => StorageType::RemovableRam,
            PtpStorageType::Unknown(_) => StorageType::Other,
        }
    }
}

/// Type of filesystem on a storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilesystemType {
    /// Undefined filesystem type.
    #[default]
    Undefined,
    /// Flat (no folders).
    Flat,
    /// Hierarchical (folders).
    Hierarchical,
    /// DCF (camera file system).
    Dcf,
    /// A code this library doesn't model.
    Other,
}

impl From<PtpFs> for FilesystemType {
    fn from(t: PtpFs) -> Self {
        match t {
            PtpFs::Undefined => FilesystemType::Undefined,
            PtpFs::GenericFlat => FilesystemType::Flat,
            PtpFs::GenericHierarchical => FilesystemType::Hierarchical,
            PtpFs::Dcf => FilesystemType::Dcf,
            PtpFs::Unknown(_) => FilesystemType::Other,
        }
    }
}

/// Metadata for one object (file or folder) on a device.
#[derive(Debug, Clone, Default)]
pub struct ObjectInfo {
    /// Opaque handle for this object (see [`ObjectHandle`]).
    pub handle: ObjectHandle,
    /// The storage this object lives on.
    pub storage_id: StorageId,
    /// Parent object, or [`ObjectHandle::ROOT`] for a top-level object.
    pub parent: ObjectHandle,
    /// File or folder name.
    pub filename: String,
    /// Size in bytes (0 for folders).
    pub size: u64,
    /// MTP format code.
    pub format: ObjectFormat,
    /// Creation time, if the device reported a valid one.
    pub created: Option<DateTime>,
    /// Modification time, if the device reported a valid one.
    pub modified: Option<DateTime>,
    /// Image width in pixels (0 if not an image or unknown).
    pub image_width: u32,
    /// Image height in pixels (0 if not an image or unknown).
    pub image_height: u32,
    /// Pre-resolved folder flag (see [`ObjectInfo::is_folder`]).
    pub(crate) folder: bool,
}

impl ObjectInfo {
    /// Whether this object is a folder.
    #[must_use]
    pub fn is_folder(&self) -> bool {
        self.folder
    }

    /// Whether this object is a file (not a folder).
    #[must_use]
    pub fn is_file(&self) -> bool {
        !self.folder
    }

    /// Build the neutral form from a PTP `ObjectInfo`, preserving the folder test (which on PTP
    /// depends on both the format and the association type).
    pub(crate) fn from_ptp(o: PtpObjectInfo) -> Self {
        let folder = o.is_folder();
        ObjectInfo {
            handle: o.handle.into(),
            storage_id: o.storage_id.into(),
            parent: o.parent.into(),
            filename: o.filename,
            size: o.size,
            format: o.format.into(),
            created: o.created.map(Into::into),
            modified: o.modified.map(Into::into),
            image_width: o.image_width,
            image_height: o.image_height,
            folder,
        }
    }
}

/// Identity of a connected device, backend-neutral.
///
/// Protocol- and backend-specific detail (PTP operation lists, vendor extensions) lives on the
/// low-level [`crate::ptp`] types, not here. What a device can *do* is reported separately via
/// [`Capabilities`].
#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    /// Manufacturer name, if reported.
    pub manufacturer: String,
    /// Model name.
    pub model: String,
    /// Serial number, if reported.
    pub serial_number: String,
    /// Device firmware/software version, if reported.
    pub device_version: String,
}

impl DeviceInfo {
    pub(crate) fn from_ptp(d: &PtpDeviceInfo) -> Self {
        DeviceInfo {
            manufacturer: d.manufacturer.clone(),
            model: d.model.clone(),
            serial_number: d.serial_number.clone(),
            device_version: d.device_version.clone(),
        }
    }
}

/// Description of a single storage on a device, backend-neutral.
#[derive(Debug, Clone, Default)]
pub struct StorageInfo {
    /// Opaque identifier for this storage (see [`StorageId`]).
    pub id: StorageId,
    /// Human-readable description (e.g. "Internal shared storage").
    pub description: String,
    /// Volume identifier, if any.
    pub volume_identifier: String,
    /// Total capacity in bytes.
    pub total_capacity: u64,
    /// Free space in bytes.
    pub free_space: u64,
    /// Whether the storage accepts writes (uploads, deletes, renames).
    pub is_writable: bool,
    /// Storage medium type.
    pub storage_type: StorageType,
    /// Filesystem type.
    pub filesystem_type: FilesystemType,
}

impl StorageInfo {
    pub(crate) fn from_ptp(s: &PtpStorageInfo) -> Self {
        StorageInfo {
            // The PTP StorageInfo dataset doesn't carry its own id; the backend fills it in after
            // the GetStorageInfo call that already knows the id.
            id: StorageId::default(),
            description: s.description.clone(),
            volume_identifier: s.volume_identifier.clone(),
            total_capacity: s.max_capacity,
            free_space: s.free_space_bytes,
            is_writable: s.access_capability == AccessCapability::ReadWrite,
            storage_type: s.storage_type.into(),
            filesystem_type: s.filesystem_type.into(),
        }
    }
}

/// What a connected device supports, derived per backend.
///
/// Replaces switching on backend-specific operation codes. The USB/PTP backend computes these from
/// the device's advertised operations; the WPD backend computes them from WPD command support.
/// Advertised support can still be wrong on some devices (see the Fujifilm quirk in `AGENTS.md`),
/// so treat these as a strong hint, not a guarantee.
#[derive(Debug, Clone, Copy, Default)]
pub struct Capabilities {
    /// Can create new files (upload).
    pub can_upload: bool,
    /// Can delete objects.
    pub can_delete: bool,
    /// Can rename objects.
    pub can_rename: bool,
    /// Can move objects between folders/storages.
    pub can_move: bool,
    /// Can copy objects.
    pub can_copy: bool,
    /// Can create folders.
    pub can_create_folder: bool,
    /// Can download a byte range / resume (not just whole files).
    pub supports_partial_download: bool,
    /// Can fetch thumbnails.
    pub supports_thumbnails: bool,
    /// Emits device events.
    pub supports_events: bool,
}

impl Capabilities {
    pub(crate) fn from_ptp_device_info(d: &PtpDeviceInfo) -> Self {
        let op = |o: OperationCode| d.supports_operation(o);
        Capabilities {
            can_upload: op(OperationCode::SendObjectInfo) && op(OperationCode::SendObject),
            can_delete: op(OperationCode::DeleteObject),
            can_rename: d.supports_rename(),
            can_move: op(OperationCode::MoveObject),
            can_copy: op(OperationCode::CopyObject),
            can_create_folder: op(OperationCode::SendObjectInfo),
            supports_partial_download: op(OperationCode::GetPartialObject64)
                || op(OperationCode::GetPartialObject),
            supports_thumbnails: op(OperationCode::GetThumb),
            supports_events: !d.events_supported.is_empty(),
        }
    }
}
