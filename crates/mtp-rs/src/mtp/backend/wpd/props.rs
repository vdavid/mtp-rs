//! WPD property-bag helpers, string marshalling, and HRESULT → neutral error mapping.
//!
//! All the small, mostly-`unsafe` glue between windows-rs and our neutral [`crate::mtp`] types lives
//! here so [`super::com`] reads top-to-bottom. The typed property-bag getters
//! (`GetStringValue`/`GetGuidValue`/`GetUnsignedLargeIntegerValue`) keep us out of raw `PROPVARIANT`
//! handling, as proven in the Phase 0 spike.

use crate::mtp::{Error, ObjectFormat, ObjectHandle, ObjectInfo, StorageId};
use std::ffi::c_void;

use windows::core::{Error as WinError, GUID, PWSTR};
use windows::Win32::Devices::PortableDevices::{
    IPortableDeviceProperties, IPortableDeviceValues, WPD_CONTENT_TYPE_FOLDER,
    WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT, WPD_OBJECT_CONTENT_TYPE, WPD_OBJECT_NAME,
    WPD_OBJECT_ORIGINAL_FILE_NAME, WPD_OBJECT_SIZE,
};
use windows::Win32::System::Com::CoTaskMemFree;

/// Build a NUL-terminated UTF-16 buffer for a `PCWSTR`/`PWSTR` argument. The returned `Vec` must
/// outlive the call that borrows its pointer.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Take ownership of a COM-allocated `PWSTR`: copy to a `String`, then `CoTaskMemFree` it.
///
/// # Safety
/// `p` must be a `PWSTR` the callee allocated with the COM task allocator and handed to us to free
/// (the WPD `Get*` string getters do exactly this), or null.
pub(crate) unsafe fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.0 as *const c_void));
    s
}

/// Map a windows-rs / WPD `HRESULT` error into the neutral [`Error`].
///
/// We match on the raw `HRESULT` so we
/// don't depend on every named constant being projected; the well-known values are stable.
pub(crate) fn map_hresult(e: WinError) -> Error {
    // HRESULT_FROM_WIN32(x) == 0x8007_0000 | (x & 0xFFFF) for the FACILITY_WIN32 codes we care about.
    const fn win32(code: u32) -> i32 {
        (0x8007_0000u32 | (code & 0xFFFF)) as i32
    }
    const E_ACCESSDENIED: i32 = 0x8007_0005u32 as i32;
    const E_NOTIMPL: i32 = 0x8000_4001u32 as i32;
    const STG_E_MEDIUMFULL: i32 = 0x8003_0070u32 as i32;
    const STG_E_ACCESSDENIED: i32 = 0x8003_0005u32 as i32;
    // WIN32_ERROR codes (pre-HRESULT-wrap).
    const ERROR_FILE_NOT_FOUND: u32 = 2;
    const ERROR_PATH_NOT_FOUND: u32 = 3;
    const ERROR_ACCESS_DENIED: u32 = 5;
    const ERROR_BUSY: u32 = 170;
    const ERROR_DEVICE_NOT_AVAILABLE: u32 = 4319;
    const ERROR_DEVICE_REMOVED: u32 = 1617;
    const ERROR_NOT_SUPPORTED: u32 = 50;

    match e.code().0 {
        E_ACCESSDENIED | STG_E_ACCESSDENIED => Error::AccessDenied,
        x if x == win32(ERROR_ACCESS_DENIED) => Error::AccessDenied,
        x if x == win32(ERROR_FILE_NOT_FOUND) || x == win32(ERROR_PATH_NOT_FOUND) => {
            Error::NotFound
        }
        x if x == win32(ERROR_BUSY) => Error::Busy,
        x if x == win32(ERROR_DEVICE_NOT_AVAILABLE) || x == win32(ERROR_DEVICE_REMOVED) => {
            Error::Disconnected
        }
        STG_E_MEDIUMFULL => Error::StorageFull,
        E_NOTIMPL => Error::Unsupported,
        x if x == win32(ERROR_NOT_SUPPORTED) => Error::Unsupported,
        _ => Error::Other {
            detail: format!("HRESULT {:#010x}: {}", e.code().0 as u32, e.message()),
        },
    }
}

/// Whether a WPD content-type GUID denotes a folder (or a storage/functional object, which we also
/// treat as a directory for traversal).
pub(crate) fn is_folder_content_type(ctype: &GUID) -> bool {
    *ctype == WPD_CONTENT_TYPE_FOLDER || *ctype == WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT
}

/// Read the neutral [`ObjectInfo`] for one WPD object whose `handle`/`parent`/`storage` the caller
/// already knows (the common case while listing a known directory).
///
/// `handle`, `parent`, and `storage` are supplied by the caller because, while listing the children
/// of a known directory, all are context we already have — cheaper and more reliable than
/// re-deriving them from each child's properties. The caller interns `wpd_id` into the shared
/// [`IdMap`](super::ids::IdMap) under its lock and passes the resulting `handle` here, so this
/// function never touches the (mutex-guarded) map while doing COM I/O. Datetimes are left `None` for
/// now (lenient; see the plan's "per-device property variance" risk).
///
/// # Safety
/// `props` must be a live `IPortableDeviceProperties` for the open device.
pub(crate) unsafe fn read_object_info(
    props: &IPortableDeviceProperties,
    handle: ObjectHandle,
    wpd_id: &str,
    parent: ObjectHandle,
    storage: StorageId,
) -> ObjectInfo {
    let id_w = wide(wpd_id);
    let vals = props.GetValues(windows::core::PCWSTR(id_w.as_ptr()), None);

    let (filename, size, folder) = match vals {
        Ok(v) => {
            // Prefer the original filesystem name; fall back to the display name.
            let filename = v
                .GetStringValue(&WPD_OBJECT_ORIGINAL_FILE_NAME)
                .map(|p| take_pwstr(p))
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    v.GetStringValue(&WPD_OBJECT_NAME)
                        .map(|p| take_pwstr(p))
                        .ok()
                })
                .unwrap_or_default();
            let ctype = v.GetGuidValue(&WPD_OBJECT_CONTENT_TYPE).unwrap_or_default();
            let folder = is_folder_content_type(&ctype);
            // Folders report no size; treat a missing size as 0.
            let size = if folder {
                0
            } else {
                v.GetUnsignedLargeIntegerValue(&WPD_OBJECT_SIZE)
                    .unwrap_or(0)
            };
            (filename, size, folder)
        }
        Err(_) => (String::new(), 0, false),
    };

    let format = if folder {
        ObjectFormat::ASSOCIATION
    } else {
        ObjectFormat::UNDEFINED
    };

    ObjectInfo {
        handle,
        storage_id: storage,
        parent,
        filename,
        size,
        format,
        created: None,
        modified: None,
        image_width: 0,
        image_height: 0,
        folder,
    }
}

/// Convenience: set the unsigned-integer client-info / property values without per-call ceremony.
///
/// # Safety
/// `bag` must be a live `IPortableDeviceValues`.
pub(crate) unsafe fn set_u32(
    bag: &IPortableDeviceValues,
    key: &windows::Win32::Foundation::PROPERTYKEY,
    value: u32,
) -> Result<(), Error> {
    bag.SetUnsignedIntegerValue(key, value).map_err(map_hresult)
}
