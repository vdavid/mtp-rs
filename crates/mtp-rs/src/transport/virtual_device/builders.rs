//! Binary payload builders for virtual device responses.
//!
//! These functions produce the binary payloads that get wrapped in data containers
//! before being returned to the PTP session.

use super::state::VirtualDeviceState;
use crate::ptp::{
    pack_string, pack_u16, pack_u16_array, pack_u32, pack_u64, DateTime, EventCode,
    ObjectFormatCode, ObjectHandle, OperationCode, StorageId, MAX_STRING_UNITS,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Pack a string the device sends, truncating one PTP can't carry the way a real device does.
///
/// The host side refuses an over-long string ([`pack_string`] errors), but a device has no one to
/// refuse to: a backing file can have a 255-unit name (Linux allows 255 bytes) and still has to be
/// listed. AOSP's `MtpStringBuffer::writeToPacket` cuts at [`MAX_STRING_UNITS`] units; this does
/// too, but backs off rather than split a surrogate pair, which would reach the host as a lone
/// surrogate.
pub(super) fn pack_device_string(s: &str) -> Vec<u8> {
    let mut units = 0;
    let end = s
        .char_indices()
        .find(|&(_, c)| {
            units += c.len_utf16();
            units > MAX_STRING_UNITS
        })
        .map_or(s.len(), |(i, _)| i);
    pack_string(&s[..end]).expect("truncated to MAX_STRING_UNITS, so it always fits")
}

/// Build a DeviceInfo binary payload from the virtual device config.
pub(super) fn build_device_info(state: &VirtualDeviceState) -> Vec<u8> {
    let mut buf = Vec::with_capacity(512);

    // StandardVersion: 100 (v1.00)
    buf.extend_from_slice(&pack_u16(100));
    // VendorExtensionID: 6 (Microsoft / MTP)
    buf.extend_from_slice(&pack_u32(6));
    // VendorExtensionVersion: 100
    buf.extend_from_slice(&pack_u16(100));
    // VendorExtensionDesc: "microsoft.com: 1.0"
    buf.extend_from_slice(&pack_device_string("microsoft.com: 1.0"));
    // FunctionalMode: 0 (standard)
    buf.extend_from_slice(&pack_u16(0));

    // OperationsSupported
    let mut ops: Vec<u16> = vec![
        OperationCode::GetDeviceInfo.into(),
        OperationCode::OpenSession.into(),
        OperationCode::CloseSession.into(),
        OperationCode::GetStorageIds.into(),
        OperationCode::GetStorageInfo.into(),
        OperationCode::GetObjectHandles.into(),
        OperationCode::GetObjectInfo.into(),
        OperationCode::GetObject.into(),
        OperationCode::GetThumb.into(),
        OperationCode::SendObjectInfo.into(),
        OperationCode::SendObject.into(),
        OperationCode::DeleteObject.into(),
        OperationCode::MoveObject.into(),
        OperationCode::CopyObject.into(),
        OperationCode::GetObjectPropValue.into(),
    ];
    if state.config.supports_partial_object {
        ops.push(OperationCode::GetPartialObject.into());
    }
    if state.config.supports_partial_object_64 {
        ops.push(OperationCode::GetPartialObject64.into());
    }
    if state.config.supports_rename {
        ops.push(OperationCode::SetObjectPropValue.into());
    }
    if state.config.supports_object_prop_list {
        ops.push(OperationCode::GetObjectPropList.into());
    }
    buf.extend_from_slice(&pack_u16_array(&ops));

    // EventsSupported
    let events: Vec<u16> = vec![
        EventCode::ObjectAdded.into(),
        EventCode::ObjectRemoved.into(),
        EventCode::StorageInfoChanged.into(),
    ];
    buf.extend_from_slice(&pack_u16_array(&events));

    // DevicePropertiesSupported: empty
    buf.extend_from_slice(&pack_u16_array(&[]));
    // CaptureFormats: empty
    buf.extend_from_slice(&pack_u16_array(&[]));
    // PlaybackFormats: common formats
    let playback: Vec<u16> = vec![
        ObjectFormatCode::Undefined.into(),
        ObjectFormatCode::Association.into(),
        ObjectFormatCode::Text.into(),
        ObjectFormatCode::Jpeg.into(),
        ObjectFormatCode::Png.into(),
        ObjectFormatCode::Mp3.into(),
        ObjectFormatCode::Mp4Container.into(),
    ];
    buf.extend_from_slice(&pack_u16_array(&playback));

    // Manufacturer
    buf.extend_from_slice(&pack_device_string(&state.config.manufacturer));
    // Model
    buf.extend_from_slice(&pack_device_string(&state.config.model));
    // DeviceVersion
    buf.extend_from_slice(&pack_device_string("1.0.0"));
    // SerialNumber
    buf.extend_from_slice(&pack_device_string(&state.config.serial));

    buf
}

/// Build a StorageInfo binary payload for a given storage.
pub(super) fn build_storage_info(
    state: &VirtualDeviceState,
    storage_id: StorageId,
) -> Option<Vec<u8>> {
    let storage = state.find_storage(storage_id)?;
    let mut buf = Vec::with_capacity(128);

    // StorageType: FixedRam (3)
    buf.extend_from_slice(&pack_u16(3));
    // FilesystemType: GenericHierarchical (2)
    buf.extend_from_slice(&pack_u16(2));
    // AccessCapability
    let access = if storage.config.read_only { 1u16 } else { 0u16 };
    buf.extend_from_slice(&pack_u16(access));
    // MaxCapacity
    buf.extend_from_slice(&pack_u64(storage.config.capacity));
    // FreeSpaceInBytes - compute from backing dir
    let free = compute_free_space(&storage.config);
    buf.extend_from_slice(&pack_u64(free));
    // FreeSpaceInObjects: 0xFFFFFFFF (unknown)
    buf.extend_from_slice(&pack_u32(0xFFFF_FFFF));
    // StorageDescription
    buf.extend_from_slice(&pack_device_string(&storage.config.description));
    // VolumeIdentifier
    buf.extend_from_slice(&pack_device_string(""));

    Some(buf)
}

/// Compute free space for a storage by subtracting used space from capacity.
fn compute_free_space(config: &super::config::VirtualStorageConfig) -> u64 {
    let used = dir_size(&config.backing_dir);
    config.capacity.saturating_sub(used)
}

/// Recursively compute total size of all files in a directory.
fn dir_size(path: &std::path::Path) -> u64 {
    if !path.is_dir() {
        return 0;
    }
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
            } else if ft.is_dir() {
                total += dir_size(&entry.path());
            }
        }
    }
    total
}

/// Build a container header + params for a response container.
pub(super) fn build_response(code: u16, tx_id: u32, params: &[u32]) -> Vec<u8> {
    let len = 12 + params.len() * 4;
    let mut buf = Vec::with_capacity(len);
    buf.extend_from_slice(&pack_u32(len as u32));
    buf.extend_from_slice(&pack_u16(3)); // Response type
    buf.extend_from_slice(&pack_u16(code));
    buf.extend_from_slice(&pack_u32(tx_id));
    for &p in params {
        buf.extend_from_slice(&pack_u32(p));
    }
    buf
}

/// Build a data container wrapping a payload.
pub(super) fn build_data_container(op_code: u16, tx_id: u32, payload: &[u8]) -> Vec<u8> {
    let len = 12 + payload.len();
    let mut buf = Vec::with_capacity(len);
    buf.extend_from_slice(&pack_u32(len as u32));
    buf.extend_from_slice(&pack_u16(2)); // Data type
    buf.extend_from_slice(&pack_u16(op_code));
    buf.extend_from_slice(&pack_u32(tx_id));
    buf.extend_from_slice(payload);
    buf
}

/// Build an event container.
pub(super) fn build_event(code: EventCode, params: &[u32]) -> Vec<u8> {
    let param_count = params.len().min(3);
    let len = 12 + param_count * 4;
    let mut buf = Vec::with_capacity(len);
    buf.extend_from_slice(&pack_u32(len as u32));
    buf.extend_from_slice(&pack_u16(4)); // Event type
    buf.extend_from_slice(&pack_u16(code.into()));
    buf.extend_from_slice(&pack_u32(0)); // transaction_id = 0 for events
    for &p in params.iter().take(param_count) {
        buf.extend_from_slice(&pack_u32(p));
    }
    buf
}

/// Pack a file timestamp as an ObjectInfo datetime on the device's clock, or as
/// the empty "no date" string when there's none or MTP can't write it.
pub(super) fn pack_file_time(time: Option<SystemTime>, state: &VirtualDeviceState) -> Vec<u8> {
    let config = &state.config;
    let formatted = time
        .and_then(unix_seconds)
        .and_then(|secs| DateTime::from_unix_seconds(secs, config.utc_offset))
        .map(|dt| DateTime {
            offset: dt.offset.filter(|_| config.dates_include_offset),
            ..dt
        })
        .and_then(|dt| dt.format())
        .unwrap_or_default();
    pack_device_string(&formatted)
}

/// Whole seconds since the Unix epoch, rounded down (so before the epoch too).
fn unix_seconds(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).ok(),
        Err(before) => {
            let before = before.duration();
            let secs = i64::try_from(before.as_secs()).ok()?;
            Some(if before.subsec_nanos() > 0 {
                -secs - 1
            } else {
                -secs
            })
        }
    }
}

/// The `SystemTime` at whole seconds since the Unix epoch.
pub(super) fn system_time_from_unix(secs: i64) -> SystemTime {
    let magnitude = Duration::from_secs(secs.unsigned_abs());
    if secs >= 0 {
        UNIX_EPOCH + magnitude
    } else {
        UNIX_EPOCH - magnitude
    }
}

/// What the device reports about one object, read from its backing file.
/// `GetObjectInfo` and `GetObjectPropList` both answer from this, so they agree.
pub(super) struct ObjectFacts {
    pub storage_id: StorageId,
    pub parent: ObjectHandle,
    pub filename: String,
    pub is_dir: bool,
    /// The full size: `GetObjectInfo` saturates it at `u32::MAX`, `ObjectSize` doesn't.
    pub size: u64,
    pub format: u16,
    pub created: Option<SystemTime>,
    pub modified: Option<SystemTime>,
}

/// Read [`ObjectFacts`] for `handle`, or `None` for an unknown handle or a vanished file.
pub(super) fn object_facts(
    handle: ObjectHandle,
    state: &VirtualDeviceState,
) -> Option<ObjectFacts> {
    let obj = state.objects.get(&handle.0)?;
    let storage = state.find_storage(obj.storage_id)?;
    let metadata = std::fs::metadata(storage.config.backing_dir.join(&obj.rel_path)).ok()?;
    let is_dir = metadata.is_dir();
    let format = if is_dir {
        ObjectFormatCode::Association
    } else {
        let ext = obj
            .rel_path
            .extension()
            .unwrap_or_default()
            .to_string_lossy();
        ObjectFormatCode::from_extension(&ext)
    };
    Some(ObjectFacts {
        storage_id: obj.storage_id,
        parent: obj.parent,
        filename: obj
            .rel_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
        is_dir,
        size: if is_dir { 0 } else { metadata.len() },
        format: format.into(),
        created: metadata.created().ok(),
        modified: metadata.modified().ok(),
    })
}

/// Build an ObjectInfo payload from filesystem metadata.
pub(super) fn build_object_info(
    handle: ObjectHandle,
    state: &VirtualDeviceState,
) -> Option<Vec<u8>> {
    let facts = object_facts(handle, state)?;
    let size = facts.size.min(u64::from(u32::MAX)) as u32;

    let mut buf = Vec::with_capacity(128);

    // StorageID
    buf.extend_from_slice(&pack_u32(facts.storage_id.0));
    // ObjectFormat
    buf.extend_from_slice(&pack_u16(facts.format));
    // ProtectionStatus: None (0)
    buf.extend_from_slice(&pack_u16(0));
    // ObjectCompressedSize
    buf.extend_from_slice(&pack_u32(size));
    // ThumbFormat: Undefined
    buf.extend_from_slice(&pack_u16(ObjectFormatCode::Undefined.into()));
    // ThumbCompressedSize, ThumbPixWidth, ThumbPixHeight
    buf.extend_from_slice(&pack_u32(0));
    buf.extend_from_slice(&pack_u32(0));
    buf.extend_from_slice(&pack_u32(0));
    // ImagePixWidth, ImagePixHeight, ImageBitDepth
    buf.extend_from_slice(&pack_u32(0));
    buf.extend_from_slice(&pack_u32(0));
    buf.extend_from_slice(&pack_u32(0));
    // ParentObject
    buf.extend_from_slice(&pack_u32(facts.parent.0));
    // AssociationType
    let assoc_type: u16 = if facts.is_dir { 1 } else { 0 };
    buf.extend_from_slice(&pack_u16(assoc_type));
    // AssociationDesc
    buf.extend_from_slice(&pack_u32(0));
    // SequenceNumber
    buf.extend_from_slice(&pack_u32(0));
    // Filename
    buf.extend_from_slice(&pack_device_string(&facts.filename));
    // DateCreated, DateModified
    buf.extend_from_slice(&pack_file_time(facts.created, state));
    buf.extend_from_slice(&pack_file_time(facts.modified, state));
    // Keywords: empty
    buf.push(0x00);

    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ptp::unpack_string;

    #[test]
    fn device_strings_that_fit_go_out_unchanged() {
        let s = "a".repeat(MAX_STRING_UNITS);
        assert_eq!(pack_device_string(&s), pack_string(&s).unwrap());
    }

    #[test]
    fn device_strings_too_long_for_ptp_are_cut_like_aosp_does() {
        let (decoded, _) = unpack_string(&pack_device_string(&"a".repeat(300))).unwrap();
        assert_eq!(decoded, "a".repeat(MAX_STRING_UNITS));
    }

    #[test]
    fn device_string_cut_never_splits_a_surrogate_pair() {
        // 253 units of ASCII, then an emoji whose second unit would be the 255th.
        let s = format!("{}\u{1F600}", "a".repeat(MAX_STRING_UNITS - 1));
        let (decoded, _) = unpack_string(&pack_device_string(&s)).unwrap();
        assert_eq!(decoded, "a".repeat(MAX_STRING_UNITS - 1));
    }
}
