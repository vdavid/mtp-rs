//! Protocol operation handlers for the virtual device.
//!
//! Each handler processes an incoming MTP command and queues response (and optionally
//! data) containers into the state's response queue.

use super::builders::{
    build_data_container, build_device_info, build_event, build_object_info, build_response,
    build_storage_info,
};
use super::state::{PendingSendInfo, VirtualDeviceState};
use crate::ptp::{
    pack_string, pack_u32_array, unpack_string, EventCode, ObjectFormatCode, ObjectHandle,
    ObjectPropertyCode, OperationCode, StorageId,
};
use std::path::{Path, PathBuf};

/// Response code constants as u16.
const OK: u16 = 0x2001;
const GENERAL_ERROR: u16 = 0x2002;
const SESSION_NOT_OPEN: u16 = 0x2003;
const OPERATION_NOT_SUPPORTED: u16 = 0x2005;
const INVALID_STORAGE_ID: u16 = 0x2008;
const INVALID_OBJECT_HANDLE: u16 = 0x2009;
const STORE_READ_ONLY: u16 = 0x200E;
const NO_THUMBNAIL: u16 = 0x2010;
const SESSION_ALREADY_OPEN: u16 = 0x201E;
const INVALID_PARENT: u16 = 0x201A;

/// Dispatch a command to the appropriate handler.
///
/// `op_code` is the raw operation code, `tx_id` the transaction ID,
/// `params` are the u32 command parameters, and `data_payload` is the
/// optional data phase payload (for operations like SendObjectInfo/SendObject).
pub(super) fn dispatch(
    state: &mut VirtualDeviceState,
    op_code: u16,
    tx_id: u32,
    params: &[u32],
    data_payload: Option<&[u8]>,
) {
    let op = OperationCode::from(op_code);

    match op {
        OperationCode::GetDeviceInfo => handle_get_device_info(state, op_code, tx_id),
        OperationCode::OpenSession => handle_open_session(state, tx_id, params),
        OperationCode::CloseSession => handle_close_session(state, tx_id),
        _ => {
            // All other operations require an open session
            if !state.session_open {
                state
                    .response_queue
                    .push_back(build_response(SESSION_NOT_OPEN, tx_id, &[]));
                return;
            }
            match op {
                OperationCode::GetStorageIds => handle_get_storage_ids(state, op_code, tx_id),
                OperationCode::GetStorageInfo => {
                    handle_get_storage_info(state, op_code, tx_id, params)
                }
                OperationCode::GetObjectHandles => {
                    handle_get_object_handles(state, op_code, tx_id, params)
                }
                OperationCode::GetObjectInfo => {
                    handle_get_object_info(state, op_code, tx_id, params)
                }
                OperationCode::GetObject => handle_get_object(state, op_code, tx_id, params),
                OperationCode::GetPartialObject => {
                    handle_get_partial_object(state, op_code, tx_id, params)
                }
                OperationCode::GetPartialObject64 => {
                    handle_get_partial_object_64(state, op_code, tx_id, params)
                }
                OperationCode::GetThumb => handle_get_thumb(state, tx_id, params),
                OperationCode::SendObjectInfo => {
                    handle_send_object_info(state, tx_id, params, data_payload)
                }
                OperationCode::SendObject => handle_send_object(state, tx_id, data_payload),
                OperationCode::DeleteObject => handle_delete_object(state, tx_id, params),
                OperationCode::MoveObject => handle_move_object(state, tx_id, params),
                OperationCode::CopyObject => handle_copy_object(state, tx_id, params),
                OperationCode::GetObjectPropValue => {
                    handle_get_object_prop_value(state, op_code, tx_id, params)
                }
                OperationCode::SetObjectPropValue => {
                    handle_set_object_prop_value(state, tx_id, params, data_payload)
                }
                _ => {
                    state.response_queue.push_back(build_response(
                        OPERATION_NOT_SUPPORTED,
                        tx_id,
                        &[],
                    ));
                }
            }
        }
    }
}

fn handle_get_device_info(state: &mut VirtualDeviceState, op_code: u16, tx_id: u32) {
    let payload = build_device_info(state);
    state
        .response_queue
        .push_back(build_data_container(op_code, tx_id, &payload));
    state
        .response_queue
        .push_back(build_response(OK, tx_id, &[]));
}

fn handle_open_session(state: &mut VirtualDeviceState, tx_id: u32, _params: &[u32]) {
    if state.session_open {
        state
            .response_queue
            .push_back(build_response(SESSION_ALREADY_OPEN, tx_id, &[]));
    } else {
        state.session_open = true;
        state
            .response_queue
            .push_back(build_response(OK, tx_id, &[]));
    }
}

fn handle_close_session(state: &mut VirtualDeviceState, tx_id: u32) {
    state.session_open = false;
    state.objects.clear();
    state.pending_send = None;
    state
        .response_queue
        .push_back(build_response(OK, tx_id, &[]));
}

fn handle_get_storage_ids(state: &mut VirtualDeviceState, op_code: u16, tx_id: u32) {
    let ids: Vec<u32> = state.storages.iter().map(|s| s.storage_id.0).collect();
    let payload = pack_u32_array(&ids);
    state
        .response_queue
        .push_back(build_data_container(op_code, tx_id, &payload));
    state
        .response_queue
        .push_back(build_response(OK, tx_id, &[]));
}

fn handle_get_storage_info(
    state: &mut VirtualDeviceState,
    op_code: u16,
    tx_id: u32,
    params: &[u32],
) {
    let storage_id = StorageId(params.first().copied().unwrap_or(0));
    match build_storage_info(state, storage_id) {
        Some(payload) => {
            state
                .response_queue
                .push_back(build_data_container(op_code, tx_id, &payload));
            state
                .response_queue
                .push_back(build_response(OK, tx_id, &[]));
        }
        None => {
            state
                .response_queue
                .push_back(build_response(INVALID_STORAGE_ID, tx_id, &[]));
        }
    }
}

fn handle_get_object_handles(
    state: &mut VirtualDeviceState,
    op_code: u16,
    tx_id: u32,
    params: &[u32],
) {
    let storage_id = StorageId(params.first().copied().unwrap_or(0));
    // params[1] = format filter (ignored), params[2] = parent handle
    let parent_raw = params.get(2).copied().unwrap_or(0);
    let parent = ObjectHandle(parent_raw);

    // Validate storage
    if state.find_storage(storage_id).is_none() {
        state
            .response_queue
            .push_back(build_response(INVALID_STORAGE_ID, tx_id, &[]));
        return;
    }

    let handles_result = if parent == ObjectHandle::ALL {
        state.scan_all(storage_id)
    } else {
        state.scan_dir(storage_id, parent)
    };

    match handles_result {
        Ok(handles) => {
            let raw: Vec<u32> = handles.iter().map(|h| h.0).collect();
            let payload = pack_u32_array(&raw);
            state
                .response_queue
                .push_back(build_data_container(op_code, tx_id, &payload));
            state
                .response_queue
                .push_back(build_response(OK, tx_id, &[]));
        }
        Err(_) => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
        }
    }
}

fn handle_get_object_info(
    state: &mut VirtualDeviceState,
    op_code: u16,
    tx_id: u32,
    params: &[u32],
) {
    let handle = ObjectHandle(params.first().copied().unwrap_or(0));
    match build_object_info(handle, state) {
        Some(payload) => {
            state
                .response_queue
                .push_back(build_data_container(op_code, tx_id, &payload));
            state
                .response_queue
                .push_back(build_response(OK, tx_id, &[]));
        }
        None => {
            state
                .response_queue
                .push_back(build_response(INVALID_OBJECT_HANDLE, tx_id, &[]));
        }
    }
}

fn handle_get_object(state: &mut VirtualDeviceState, op_code: u16, tx_id: u32, params: &[u32]) {
    let handle = ObjectHandle(params.first().copied().unwrap_or(0));
    let path = match state.resolve_path(handle) {
        Some(p) => p,
        None => {
            state
                .response_queue
                .push_back(build_response(INVALID_OBJECT_HANDLE, tx_id, &[]));
            return;
        }
    };

    match std::fs::read(path) {
        Ok(data) => {
            state
                .response_queue
                .push_back(build_data_container(op_code, tx_id, &data));
            state
                .response_queue
                .push_back(build_response(OK, tx_id, &[]));
        }
        Err(_) => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
        }
    }
}

fn handle_get_partial_object(
    state: &mut VirtualDeviceState,
    op_code: u16,
    tx_id: u32,
    params: &[u32],
) {
    let handle = ObjectHandle(params.first().copied().unwrap_or(0));
    let offset = params.get(1).copied().unwrap_or(0) as u64;
    let max_bytes = params.get(2).copied().unwrap_or(0) as usize;
    read_partial(state, op_code, tx_id, handle, offset, max_bytes);
}

fn handle_get_partial_object_64(
    state: &mut VirtualDeviceState,
    op_code: u16,
    tx_id: u32,
    params: &[u32],
) {
    let handle = ObjectHandle(params.first().copied().unwrap_or(0));
    let offset_lo = params.get(1).copied().unwrap_or(0) as u64;
    let offset_hi = params.get(2).copied().unwrap_or(0) as u64;
    let offset = (offset_hi << 32) | offset_lo;
    let max_bytes = params.get(3).copied().unwrap_or(0) as usize;
    read_partial(state, op_code, tx_id, handle, offset, max_bytes);
}

fn read_partial(
    state: &mut VirtualDeviceState,
    op_code: u16,
    tx_id: u32,
    handle: ObjectHandle,
    offset: u64,
    max_bytes: usize,
) {
    use std::io::{Read, Seek, SeekFrom};

    // Test hook: clamp this read's length below what was requested. A cap of 0
    // returns an empty data container (a device stalling mid-file); a cap of n>0
    // returns a short read of n real bytes (a legal partial read). See
    // `VirtualDeviceState::forced_partial_read_caps`.
    let max_bytes = match state.forced_partial_read_caps.pop_front() {
        Some(cap) => cap.min(max_bytes),
        None => max_bytes,
    };

    let path = match state.resolve_path(handle) {
        Some(p) => p,
        None => {
            state
                .response_queue
                .push_back(build_response(INVALID_OBJECT_HANDLE, tx_id, &[]));
            return;
        }
    };

    match std::fs::File::open(path) {
        Ok(mut file) => {
            if offset > 0 && file.seek(SeekFrom::Start(offset)).is_err() {
                state
                    .response_queue
                    .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
                return;
            }
            let mut buf = vec![0u8; max_bytes];
            match file.read(&mut buf) {
                Ok(n) => {
                    buf.truncate(n);
                    state
                        .response_queue
                        .push_back(build_data_container(op_code, tx_id, &buf));
                    state
                        .response_queue
                        .push_back(build_response(OK, tx_id, &[]));
                }
                Err(_) => {
                    state
                        .response_queue
                        .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
                }
            }
        }
        Err(_) => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
        }
    }
}

fn handle_get_thumb(state: &mut VirtualDeviceState, tx_id: u32, params: &[u32]) {
    let handle = ObjectHandle(params.first().copied().unwrap_or(0));
    if !state.objects.contains_key(&handle.0) {
        state
            .response_queue
            .push_back(build_response(INVALID_OBJECT_HANDLE, tx_id, &[]));
    } else {
        // Virtual device never has thumbnails
        state
            .response_queue
            .push_back(build_response(NO_THUMBNAIL, tx_id, &[]));
    }
}

fn handle_send_object_info(
    state: &mut VirtualDeviceState,
    tx_id: u32,
    params: &[u32],
    data_payload: Option<&[u8]>,
) {
    let storage_id = StorageId(params.first().copied().unwrap_or(0));
    // `SendObjectInfo`'s root destination is `SEND_ROOT` (0xFFFFFFFF) per MTP 1.1
    // D.2.12, not the `ROOT` (0) that every other parent field uses. Normalize it
    // here so path resolution and the stored object's parent stay on `ROOT`, the
    // way a real responder maps it to its storage object. `0` is accepted too:
    // the device is the lenient side, and low-level `ptp::` callers may send it.
    let parent = match ObjectHandle(params.get(1).copied().unwrap_or(0)) {
        ObjectHandle::SEND_ROOT => ObjectHandle::ROOT,
        other => other,
    };

    // Validate storage
    if state.find_storage(storage_id).is_none() {
        state
            .response_queue
            .push_back(build_response(INVALID_STORAGE_ID, tx_id, &[]));
        return;
    }

    // Check read-only
    if state.is_read_only(storage_id) {
        state
            .response_queue
            .push_back(build_response(STORE_READ_ONLY, tx_id, &[]));
        return;
    }

    // Validate parent (must be ROOT or a known directory handle in this storage)
    if parent != ObjectHandle::ROOT && parent.0 != 0 {
        match state.objects.get(&parent.0) {
            Some(obj) if obj.storage_id == storage_id => {
                let path = state.resolve_path(parent).unwrap();
                if !path.is_dir() {
                    state
                        .response_queue
                        .push_back(build_response(INVALID_PARENT, tx_id, &[]));
                    return;
                }
            }
            _ => {
                state
                    .response_queue
                    .push_back(build_response(INVALID_PARENT, tx_id, &[]));
                return;
            }
        }
    }

    // Parse ObjectInfo from data payload
    let data = match data_payload {
        Some(d) => d,
        None => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
            return;
        }
    };

    // Parse just what we need: skip to filename at offset 52
    let info = match crate::ptp::ObjectInfo::from_bytes(data) {
        Ok(i) => i,
        Err(_) => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
            return;
        }
    };

    let is_folder = info.format == ObjectFormatCode::Association;
    let new_handle = state.alloc_handle();

    state.pending_send = Some(PendingSendInfo {
        storage_id,
        parent,
        filename: info.filename.clone(),
        size: info.size,
        is_folder,
        assigned_handle: new_handle,
    });

    // Create the object on disk immediately, matching real devices: PTP's
    // SendObjectInfo creates the object (a real, addressable handle) before the
    // SendObject data phase. Folders need no data phase, so they're complete
    // here. Files get an empty placeholder that the SendObject phase overwrites;
    // if that phase never arrives (mid-stream error, cancellation), the empty
    // object remains under `new_handle` so callers can delete it or resume,
    // exactly as `UploadError::partial` promises.
    let storage = state.find_storage(storage_id).unwrap();
    let parent_path = if parent == ObjectHandle::ROOT || parent.0 == 0 {
        PathBuf::new()
    } else {
        state.objects.get(&parent.0).unwrap().rel_path.clone()
    };
    let rel_path = parent_path.join(&info.filename);
    let full_path = storage.config.backing_dir.join(&rel_path);

    if validate_path_within(&storage.config.backing_dir, &full_path).is_err() {
        state.pending_send = None;
        state
            .response_queue
            .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
        return;
    }

    let create_result = if is_folder {
        std::fs::create_dir_all(&full_path)
    } else {
        // Ensure the parent directory exists, then write an empty placeholder.
        full_path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&full_path, []))
    };

    if create_result.is_err() {
        state.pending_send = None;
        state
            .response_queue
            .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
        return;
    }

    state.objects.insert(
        new_handle.0,
        super::state::VirtualObject {
            rel_path,
            storage_id,
            parent,
        },
    );

    // Queue events
    state
        .event_queue
        .push_back(build_event(EventCode::ObjectAdded, &[new_handle.0]));
    state
        .event_queue
        .push_back(build_event(EventCode::StorageInfoChanged, &[storage_id.0]));

    // Folders are fully created; files still expect a SendObject data phase to
    // fill in their contents, so keep `pending_send` set for them.
    if is_folder {
        state.pending_send = None;
    }

    // Response params: storage_id, parent_handle, new_object_handle
    state.response_queue.push_back(build_response(
        OK,
        tx_id,
        &[storage_id.0, parent.0, new_handle.0],
    ));
}

fn handle_send_object(state: &mut VirtualDeviceState, tx_id: u32, data_payload: Option<&[u8]>) {
    let pending = match state.pending_send.take() {
        Some(p) => p,
        None => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
            return;
        }
    };

    if pending.is_folder {
        // Folder was already created in SendObjectInfo; SendObject is a no-op
        state
            .response_queue
            .push_back(build_response(OK, tx_id, &[]));
        return;
    }

    let data = data_payload.unwrap_or(&[]);

    let storage = match state.find_storage(pending.storage_id) {
        Some(s) => s,
        None => {
            state
                .response_queue
                .push_back(build_response(INVALID_STORAGE_ID, tx_id, &[]));
            return;
        }
    };

    let parent_path = if pending.parent == ObjectHandle::ROOT || pending.parent.0 == 0 {
        PathBuf::new()
    } else {
        match state.objects.get(&pending.parent.0) {
            Some(obj) => obj.rel_path.clone(),
            None => {
                state
                    .response_queue
                    .push_back(build_response(INVALID_PARENT, tx_id, &[]));
                return;
            }
        }
    };

    let rel_path = parent_path.join(&pending.filename);
    let full_path = storage.config.backing_dir.join(&rel_path);

    if validate_path_within(&storage.config.backing_dir, &full_path).is_err() {
        state
            .response_queue
            .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
        return;
    }

    // Ensure parent directory exists
    if let Some(parent_dir) = full_path.parent() {
        if std::fs::create_dir_all(parent_dir).is_err() {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
            return;
        }
    }

    if std::fs::write(&full_path, data).is_err() {
        state
            .response_queue
            .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
        return;
    }

    let handle = pending.assigned_handle;
    // The object was already registered (as an empty placeholder) during
    // SendObjectInfo; re-insert defensively in case its rel_path changed, then
    // signal that its data changed rather than re-announcing it as added.
    state.objects.insert(
        handle.0,
        super::state::VirtualObject {
            rel_path,
            storage_id: pending.storage_id,
            parent: pending.parent,
        },
    );

    // Queue events. The ObjectAdded fired at SendObjectInfo time, so the data
    // phase emits ObjectInfoChanged (the object's size/content changed).
    state
        .event_queue
        .push_back(build_event(EventCode::ObjectInfoChanged, &[handle.0]));
    state.event_queue.push_back(build_event(
        EventCode::StorageInfoChanged,
        &[pending.storage_id.0],
    ));

    state
        .response_queue
        .push_back(build_response(OK, tx_id, &[]));
}

fn handle_delete_object(state: &mut VirtualDeviceState, tx_id: u32, params: &[u32]) {
    let handle = ObjectHandle(params.first().copied().unwrap_or(0));

    let obj = match state.objects.get(&handle.0) {
        Some(o) => o.clone(),
        None => {
            state
                .response_queue
                .push_back(build_response(INVALID_OBJECT_HANDLE, tx_id, &[]));
            return;
        }
    };

    if state.is_read_only(obj.storage_id) {
        state
            .response_queue
            .push_back(build_response(STORE_READ_ONLY, tx_id, &[]));
        return;
    }

    let path = match state.resolve_path(handle) {
        Some(p) => p,
        None => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
            return;
        }
    };

    let result = if path.is_dir() {
        std::fs::remove_dir_all(&path)
    } else {
        std::fs::remove_file(&path)
    };

    match result {
        Ok(()) => {
            let storage_id = obj.storage_id;

            // Remove this handle and any child handles
            let to_remove: Vec<u32> = state
                .objects
                .iter()
                .filter(|(_, o)| {
                    o.storage_id == storage_id && (o.rel_path.starts_with(&obj.rel_path))
                })
                .map(|(&h, _)| h)
                .collect();

            for h in &to_remove {
                state.objects.remove(h);
            }

            state
                .event_queue
                .push_back(build_event(EventCode::ObjectRemoved, &[handle.0]));
            state
                .event_queue
                .push_back(build_event(EventCode::StorageInfoChanged, &[storage_id.0]));

            state
                .response_queue
                .push_back(build_response(OK, tx_id, &[]));
        }
        Err(_) => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
        }
    }
}

fn handle_move_object(state: &mut VirtualDeviceState, tx_id: u32, params: &[u32]) {
    let handle = ObjectHandle(params.first().copied().unwrap_or(0));
    let dest_storage = StorageId(params.get(1).copied().unwrap_or(0));
    let dest_parent = ObjectHandle(params.get(2).copied().unwrap_or(0));

    let obj = match state.objects.get(&handle.0) {
        Some(o) => o.clone(),
        None => {
            state
                .response_queue
                .push_back(build_response(INVALID_OBJECT_HANDLE, tx_id, &[]));
            return;
        }
    };

    if state.is_read_only(obj.storage_id) || state.is_read_only(dest_storage) {
        state
            .response_queue
            .push_back(build_response(STORE_READ_ONLY, tx_id, &[]));
        return;
    }

    let src_path = match state.resolve_path(handle) {
        Some(p) => p,
        None => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
            return;
        }
    };

    let dest_storage_state = match state.find_storage(dest_storage) {
        Some(s) => s,
        None => {
            state
                .response_queue
                .push_back(build_response(INVALID_STORAGE_ID, tx_id, &[]));
            return;
        }
    };

    let filename = obj.rel_path.file_name().unwrap_or_default().to_os_string();

    let dest_parent_rel = if dest_parent == ObjectHandle::ROOT || dest_parent.0 == 0 {
        PathBuf::new()
    } else {
        match state.objects.get(&dest_parent.0) {
            Some(o) => o.rel_path.clone(),
            None => {
                state
                    .response_queue
                    .push_back(build_response(INVALID_PARENT, tx_id, &[]));
                return;
            }
        }
    };

    let new_rel = dest_parent_rel.join(filename);
    let dest_path = dest_storage_state.config.backing_dir.join(&new_rel);

    if validate_path_within(&dest_storage_state.config.backing_dir, &dest_path).is_err() {
        state
            .response_queue
            .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
        return;
    }

    if std::fs::rename(&src_path, &dest_path).is_err() {
        // Cross-device move: copy + delete
        let copy_result = if src_path.is_dir() {
            copy_dir_all(&src_path, &dest_path)
        } else {
            std::fs::copy(&src_path, &dest_path).map(|_| ())
        };
        match copy_result {
            Ok(()) => {
                let _ = if src_path.is_dir() {
                    std::fs::remove_dir_all(&src_path)
                } else {
                    std::fs::remove_file(&src_path)
                };
            }
            Err(_) => {
                state
                    .response_queue
                    .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
                return;
            }
        }
    }

    // Update object's storage and path
    let src_storage = obj.storage_id;
    if let Some(obj) = state.objects.get_mut(&handle.0) {
        obj.rel_path = new_rel;
        obj.storage_id = dest_storage;
        obj.parent = dest_parent;
    }

    state
        .event_queue
        .push_back(build_event(EventCode::ObjectInfoChanged, &[handle.0]));
    state
        .event_queue
        .push_back(build_event(EventCode::StorageInfoChanged, &[src_storage.0]));
    if dest_storage != src_storage {
        state.event_queue.push_back(build_event(
            EventCode::StorageInfoChanged,
            &[dest_storage.0],
        ));
    }

    state
        .response_queue
        .push_back(build_response(OK, tx_id, &[]));
}

fn handle_copy_object(state: &mut VirtualDeviceState, tx_id: u32, params: &[u32]) {
    let handle = ObjectHandle(params.first().copied().unwrap_or(0));
    let dest_storage = StorageId(params.get(1).copied().unwrap_or(0));
    let dest_parent = ObjectHandle(params.get(2).copied().unwrap_or(0));

    let obj = match state.objects.get(&handle.0) {
        Some(o) => o.clone(),
        None => {
            state
                .response_queue
                .push_back(build_response(INVALID_OBJECT_HANDLE, tx_id, &[]));
            return;
        }
    };

    if state.is_read_only(dest_storage) {
        state
            .response_queue
            .push_back(build_response(STORE_READ_ONLY, tx_id, &[]));
        return;
    }

    let src_path = match state.resolve_path(handle) {
        Some(p) => p,
        None => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
            return;
        }
    };

    let dest_storage_state = match state.find_storage(dest_storage) {
        Some(s) => s,
        None => {
            state
                .response_queue
                .push_back(build_response(INVALID_STORAGE_ID, tx_id, &[]));
            return;
        }
    };

    let filename = obj.rel_path.file_name().unwrap_or_default().to_os_string();

    let dest_parent_rel = if dest_parent == ObjectHandle::ROOT || dest_parent.0 == 0 {
        PathBuf::new()
    } else {
        match state.objects.get(&dest_parent.0) {
            Some(o) => o.rel_path.clone(),
            None => {
                state
                    .response_queue
                    .push_back(build_response(INVALID_PARENT, tx_id, &[]));
                return;
            }
        }
    };

    let new_rel = dest_parent_rel.join(filename);
    let dest_path = dest_storage_state.config.backing_dir.join(&new_rel);

    if validate_path_within(&dest_storage_state.config.backing_dir, &dest_path).is_err() {
        state
            .response_queue
            .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
        return;
    }

    let copy_result = if src_path.is_dir() {
        copy_dir_all(&src_path, &dest_path)
    } else {
        std::fs::copy(&src_path, &dest_path).map(|_| ())
    };

    match copy_result {
        Ok(()) => {
            let new_handle = state.alloc_handle();
            state.objects.insert(
                new_handle.0,
                super::state::VirtualObject {
                    rel_path: new_rel,
                    storage_id: dest_storage,
                    parent: dest_parent,
                },
            );

            state
                .event_queue
                .push_back(build_event(EventCode::ObjectAdded, &[new_handle.0]));
            state.event_queue.push_back(build_event(
                EventCode::StorageInfoChanged,
                &[dest_storage.0],
            ));

            state
                .response_queue
                .push_back(build_response(OK, tx_id, &[new_handle.0]));
        }
        Err(_) => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
        }
    }
}

fn handle_get_object_prop_value(
    state: &mut VirtualDeviceState,
    op_code: u16,
    tx_id: u32,
    params: &[u32],
) {
    let handle = ObjectHandle(params.first().copied().unwrap_or(0));
    let prop_code = params.get(1).copied().unwrap_or(0) as u16;
    let property = ObjectPropertyCode::from(prop_code);

    let obj = match state.objects.get(&handle.0) {
        Some(o) => o.clone(),
        None => {
            state
                .response_queue
                .push_back(build_response(INVALID_OBJECT_HANDLE, tx_id, &[]));
            return;
        }
    };

    let payload = match property {
        ObjectPropertyCode::ObjectFileName => {
            let name = obj
                .rel_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            pack_string(&name)
        }
        ObjectPropertyCode::StorageId => crate::ptp::pack_u32(obj.storage_id.0).to_vec(),
        ObjectPropertyCode::ParentObject => crate::ptp::pack_u32(obj.parent.0).to_vec(),
        ObjectPropertyCode::ObjectSize => {
            let path = state.resolve_path(handle).unwrap();
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            crate::ptp::pack_u64(size).to_vec()
        }
        _ => {
            state
                .response_queue
                .push_back(build_response(OPERATION_NOT_SUPPORTED, tx_id, &[]));
            return;
        }
    };

    state
        .response_queue
        .push_back(build_data_container(op_code, tx_id, &payload));
    state
        .response_queue
        .push_back(build_response(OK, tx_id, &[]));
}

fn handle_set_object_prop_value(
    state: &mut VirtualDeviceState,
    tx_id: u32,
    params: &[u32],
    data_payload: Option<&[u8]>,
) {
    if !state.config.supports_rename {
        state
            .response_queue
            .push_back(build_response(OPERATION_NOT_SUPPORTED, tx_id, &[]));
        return;
    }

    let handle = ObjectHandle(params.first().copied().unwrap_or(0));
    let prop_code = params.get(1).copied().unwrap_or(0) as u16;
    let property = ObjectPropertyCode::from(prop_code);

    if property != ObjectPropertyCode::ObjectFileName {
        state
            .response_queue
            .push_back(build_response(OPERATION_NOT_SUPPORTED, tx_id, &[]));
        return;
    }

    let data = match data_payload {
        Some(d) => d,
        None => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
            return;
        }
    };

    let (new_name, _) = match unpack_string(data) {
        Ok(v) => v,
        Err(_) => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
            return;
        }
    };

    let obj = match state.objects.get(&handle.0) {
        Some(o) => o.clone(),
        None => {
            state
                .response_queue
                .push_back(build_response(INVALID_OBJECT_HANDLE, tx_id, &[]));
            return;
        }
    };

    if state.is_read_only(obj.storage_id) {
        state
            .response_queue
            .push_back(build_response(STORE_READ_ONLY, tx_id, &[]));
        return;
    }

    let old_path = match state.resolve_path(handle) {
        Some(p) => p,
        None => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
            return;
        }
    };

    let new_path = old_path.parent().unwrap().join(&new_name);

    // Find the backing dir for this object's storage
    let backing_dir = match state.find_storage(obj.storage_id) {
        Some(s) => s.config.backing_dir.clone(),
        None => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
            return;
        }
    };
    if validate_path_within(&backing_dir, &new_path).is_err() {
        state
            .response_queue
            .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
        return;
    }

    match std::fs::rename(&old_path, &new_path) {
        Ok(()) => {
            // Update the relative path
            if let Some(obj_mut) = state.objects.get_mut(&handle.0) {
                let parent_rel = obj_mut.rel_path.parent().map(|p| p.to_path_buf());
                obj_mut.rel_path = match parent_rel {
                    Some(p) if p != PathBuf::new() => p.join(&new_name),
                    _ => PathBuf::from(&new_name),
                };
            }

            state
                .response_queue
                .push_back(build_response(OK, tx_id, &[]));
        }
        Err(_) => {
            state
                .response_queue
                .push_back(build_response(GENERAL_ERROR, tx_id, &[]));
        }
    }
}

/// Check that `path` is inside `base_dir`. Returns the canonicalized path or an error.
fn validate_path_within(base_dir: &Path, path: &Path) -> Result<PathBuf, ()> {
    let canonical_base = base_dir.canonicalize().map_err(|_| ())?;

    // If the path exists, canonicalize it directly
    if path.exists() {
        let canonical = path.canonicalize().map_err(|_| ())?;
        if canonical.starts_with(&canonical_base) {
            return Ok(canonical);
        }
        return Err(());
    }

    // Path doesn't exist yet, so canonicalize its parent and check
    if let Some(parent) = path.parent() {
        if parent.exists() {
            let canonical_parent = parent.canonicalize().map_err(|_| ())?;
            if canonical_parent.starts_with(&canonical_base) {
                let filename = path.file_name().ok_or(())?;
                // Also reject filenames containing path separators or ..
                let name_str = filename.to_string_lossy();
                if name_str.contains("..") || name_str.contains('/') || name_str.contains('\\') {
                    return Err(());
                }
                return Ok(canonical_parent.join(filename));
            }
        }
    }
    Err(())
}

/// Recursively copy a directory.
fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_all(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
        }
    }
    Ok(())
}
