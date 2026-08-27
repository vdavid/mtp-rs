//! Binary payload builders for virtual device responses.
//!
//! These functions produce the binary payloads that get wrapped in data containers
//! before being returned to the PTP session.

use super::state::VirtualDeviceState;
use crate::ptp::{
    pack_string, pack_u16, pack_u16_array, pack_u32, pack_u64, EventCode, ObjectFormatCode,
    ObjectHandle, OperationCode, StorageId,
};

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
    buf.extend_from_slice(&pack_string("microsoft.com: 1.0"));
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
    buf.extend_from_slice(&pack_string(&state.config.manufacturer));
    // Model
    buf.extend_from_slice(&pack_string(&state.config.model));
    // DeviceVersion
    buf.extend_from_slice(&pack_string("1.0.0"));
    // SerialNumber
    buf.extend_from_slice(&pack_string(&state.config.serial));

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
    buf.extend_from_slice(&pack_string(&storage.config.description));
    // VolumeIdentifier
    buf.extend_from_slice(&pack_string(""));

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

/// Build an ObjectInfo payload from filesystem metadata.
pub(super) fn build_object_info(
    handle: ObjectHandle,
    state: &VirtualDeviceState,
) -> Option<Vec<u8>> {
    let obj = state.objects.get(&handle.0)?;
    let storage = state.find_storage(obj.storage_id)?;
    let full_path = storage.config.backing_dir.join(&obj.rel_path);

    let metadata = std::fs::metadata(full_path).ok()?;
    let filename = obj
        .rel_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let is_dir = metadata.is_dir();
    let size = if is_dir {
        0u32
    } else {
        metadata.len().min(u32::MAX as u64) as u32
    };
    let format: u16 = if is_dir {
        ObjectFormatCode::Association.into()
    } else {
        let ext = obj
            .rel_path
            .extension()
            .unwrap_or_default()
            .to_string_lossy();
        ObjectFormatCode::from_extension(&ext).into()
    };

    let mut buf = Vec::with_capacity(128);

    // StorageID
    buf.extend_from_slice(&pack_u32(obj.storage_id.0));
    // ObjectFormat
    buf.extend_from_slice(&pack_u16(format));
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
    buf.extend_from_slice(&pack_u32(obj.parent.0));
    // AssociationType
    let assoc_type: u16 = if is_dir { 1 } else { 0 };
    buf.extend_from_slice(&pack_u16(assoc_type));
    // AssociationDesc
    buf.extend_from_slice(&pack_u32(0));
    // SequenceNumber
    buf.extend_from_slice(&pack_u32(0));
    // Filename
    buf.extend_from_slice(&pack_string(&filename));
    // DateCreated: empty
    buf.push(0x00);
    // DateModified: empty
    buf.push(0x00);
    // Keywords: empty
    buf.push(0x00);

    Some(buf)
}
